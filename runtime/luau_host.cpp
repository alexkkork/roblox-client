#include <Luau/Compiler.h>
#include <Luau/Bytecode.h>
#include <lua.h>
#include <lualib.h>
#include "random_compat.hpp"
#include "engine_bindings.hpp"
#include "world_bindings.hpp"
#include "source_bindings.hpp"
#include "closure_bindings.hpp"
#include "source_scheduler.hpp"
#include "authored_bindings.hpp"
#include "movement_bindings.hpp"
#include "script_execution.hpp"
#include "protected_error_trace.hpp"
#ifndef LOCAL_LUAU_FFI
#include "memory_categories.inc"
#endif

#include <cerrno>
#include <chrono>
#include <cmath>
#include <cstddef>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <fstream>
#include <filesystem>
#include <iostream>
#include <limits>
#include <sstream>
#include <string>
#ifdef _WIN32
#include "platform_support.hpp"
#else
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>
#include <fcntl.h>
#include <signal.h>
#include <poll.h>
#endif
#include <vector>
#include <set>

namespace {
using Clock = std::chrono::steady_clock;
constexpr size_t kMaxSource = 100 * 1024 * 1024;
constexpr size_t kMaxResult = 256 * 1024;
constexpr size_t kMaxOutput = 300 * 1024;
constexpr size_t kMaxWorldSnapshot = 8 * 1024 * 1024;

struct Limits {
    size_t cap = 32 * 1024 * 1024;
    size_t used = 0;
    Clock::time_point deadline;
    bool timed_out = false;
};

struct alignas(std::max_align_t) AllocationHeader { size_t size; };

void* capped_alloc(void* ud, void* ptr, size_t /*osize*/, size_t nsize) {
    auto* lim = static_cast<Limits*>(ud);
    if (nsize == 0) {
        if (ptr) {
            auto* header = static_cast<AllocationHeader*>(ptr) - 1;
            lim->used -= header->size;
            std::free(header);
        }
        return nullptr;
    }
    const size_t old = ptr ? (static_cast<AllocationHeader*>(ptr) - 1)->size : 0;
    if (old > lim->used || nsize > lim->cap - (lim->used - old))
        return nullptr;
    auto* header = static_cast<AllocationHeader*>(std::realloc(ptr ? static_cast<void*>(static_cast<AllocationHeader*>(ptr) - 1) : nullptr,
                                                                sizeof(AllocationHeader) + nsize));
    if (!header)
        return nullptr;
    header->size = nsize;
    lim->used = lim->used - old + nsize;
    return header + 1;
}

void interrupt(lua_State* L, int gc) {
    auto* lim = static_cast<Limits*>(lua_callbacks(L)->userdata);
    if (!lim || gc >= 0) return;
    if (Clock::now() >= lim->deadline) {
        lim->timed_out = true;
        if (lua_isyieldable(L)) lua_break(L);
    }
}

std::string json_escape(const std::string& in) {
    std::ostringstream out;
    for (unsigned char c : in) {
        switch (c) {
        case '"': out << "\\\""; break;
        case '\\': out << "\\\\"; break;
        case '\b': out << "\\b"; break;
        case '\f': out << "\\f"; break;
        case '\n': out << "\\n"; break;
        case '\r': out << "\\r"; break;
        case '\t': out << "\\t"; break;
        default:
            if (c < 0x20 || c >= 0x80) {
                const char* hex = "0123456789abcdef";
                out << "\\u00" << hex[c >> 4] << hex[c & 15];
            } else out << static_cast<char>(c);
        }
    }
    return out.str();
}

std::string error_json(const std::string& message, bool timeout = false) {
    return std::string("{\"ok\":false,\"values\":[],\"error\":\"") + json_escape(message) +
           "\",\"timed_out\":" + (timeout ? "true" : "false") + "}\n";
}
std::string world_error_json(world_bindings::World& world, const std::string& message, bool timeout = false) {
    using world_bindings::Json;
    Json result{{"ok",false},{"values",Json::array()},{"error",message},{"timed_out",timeout},
                {"output",world.output},{"output_truncated",world.output_truncated},{"mutations",world.mutations},
                {"joint_pose_errors",world.joint_pose_errors}};
    return result.dump(-1,' ',false,Json::error_handler_t::replace)+"\n";
}

void remove_global(lua_State* L, const char* name) {
    lua_pushnil(L);
    lua_setglobal(L, name);
}

bool emit_value(lua_State* L, int index, std::string& out, size_t& strings) {
    switch (lua_type(L, index)) {
    case LUA_TNIL: out += "null"; return true;
    case LUA_TBOOLEAN: out += lua_toboolean(L, index) ? "true" : "false"; return true;
    case LUA_TNUMBER: {
        const double n = lua_tonumber(L, index);
        if (!std::isfinite(n)) return false;
        std::ostringstream num;
        num.precision(std::numeric_limits<double>::max_digits10);
        num << n;
        out += num.str();
        return true;
    }
    case LUA_TSTRING: {
        size_t len = 0;
        const char* s = lua_tolstring(L, index, &len);
        strings += len;
        if (strings > kMaxResult) return false;
        out += "\"" + json_escape(std::string(s, len)) + "\"";
        return out.size() <= kMaxOutput;
    }
    default: return false;
    }
}

std::string run_vm(const std::string& source, const std::vector<double>& args,
                   bool is_bytecode, int timeout_ms, int memory_mb,
                   const LocalEngineContext* engine_context,
                   const std::string* world_snapshot_text,
                   const SourceBindings* source_bindings = nullptr) {
    std::unique_ptr<world_bindings::World> world;
    if (world_snapshot_text) {
        try { world=std::make_unique<world_bindings::World>(world_bindings::parse_snapshot(*world_snapshot_text)); }
        catch(const std::exception&e){return error_json(std::string("invalid world snapshot: ")+e.what());}
    }
    ProtectedScriptStore standalone_protected_scripts;
    auto* protected_scripts=world ? world->protected_scripts.get() : &standalone_protected_scripts;
    // Keep the store outside the VM invocation so errors retain observations.
    auto execute=[&]() -> std::string {
    Limits limits;
    limits.cap = static_cast<size_t>(memory_mb) * 1024 * 1024;
    limits.deadline = Clock::now() + std::chrono::milliseconds(timeout_ms);
    lua_State* L = lua_newstate(capped_alloc, &limits);
    if (!L) return error_json("memory limit reached while creating VM");
    std::unique_ptr<lua_State,decltype(&lua_close)> state(L,&lua_close);

    lua_Callbacks* cb = lua_callbacks(L);
    cb->userdata = &limits;
    cb->interrupt = interrupt;
    install_protected_error_trace(L);
    luaL_openlibs(L);
    // This is a general Luau VM surface. It deliberately has no file, module,
    // process, network, or client-engine bindings.
    for (const char* name : {"require", "loadstring", "dofile", "loadfile", "io", "debug"})
        remove_global(L, name);
    lua_getglobal(L, "os");
    if (lua_istable(L, -1)) { lua_pushnil(L); lua_setfield(L, -2, "exit"); }
    lua_pop(L, 1);
    install_random_compat(L);
    if(!is_bytecode)install_authored_bindings(L);
    if (engine_context) install_local_engine(L, *engine_context);
    remove_global(L, "print"); // stdout is reserved for the JSON result
    if (world) world_bindings::install(L,*world);
#ifndef LOCAL_LUAU_FFI
    if(!is_bytecode&&!source_bindings) {
        // The plain source CLI has no supervisor extensions. Preserve the
        // genuine Luau debug library and add standard allocation-tag APIs.
        lua_getfield(L,LUA_REGISTRYINDEX,"_LOADED");lua_getfield(L,-1,"debug");
        const int original_debug=lua_gettop(L);lua_newtable(L);
        for(const char* name:{"info","traceback"}) {
            lua_getfield(L,original_debug,name);lua_setfield(L,-2,name);
        }
        install_memory_category_bindings(L);lua_setglobal(L,"debug");lua_pop(L,2);
    }
#endif
    luaL_sandbox(L);

    std::string bytecode;
    if (is_bytecode) {
        bytecode = source;
    } else {
        try {
            bytecode = Luau::compile(source);
        } catch (const std::exception& e) {
            state.reset();
            return error_json(std::string("compile failed: ") + e.what());
        }
    }
    lua_State* co = lua_newthread(L);
    luaL_sandboxthread(co);
    // Instance properties are live. GETIMPORT must not retain values resolved
    // from chains such as workspace.Part.AssemblyMass while loading a chunk.
    if(world)lua_setsafeenv(co,LUA_GLOBALSINDEX,false);
    if (source_bindings) install_source_bindings(co, source_bindings,
        world ? world->script_thread_integrity.get() : nullptr,
        protected_scripts);
    std::unique_ptr<SourceScheduler> scheduler;
    std::unique_ptr<ScriptExecution> source_scripts;
    if(!is_bytecode) {
        scheduler=std::make_unique<SourceScheduler>(L,limits.deadline);scheduler->install(co);
        if(world)world->scheduler=scheduler.get();
        if(world&&source_bindings){source_scripts=std::make_unique<ScriptExecution>(co,*world,*scheduler,world_bindings::Json::parse(*world_snapshot_text));source_scripts->reconcile();}
    }
    // Keep authored globals in a shared overlay, while getgenv returns the
    // executor root. Raw getfenv writes must not change that root table.
    if(source_bindings){luaL_sandboxthread(co);lua_setsafeenv(co,LUA_GLOBALSINDEX,false);}
    auto load=[&]() {
        if(world)return world->load_script(co,"=user-source",bytecode,is_bytecode?"":source);
        auto payload=protected_scripts->make(is_bytecode?"":source,bytecode);
        return load_protected_script(co,"=user-source",payload);
    };
    if (load() != 0) {
        size_t n = 0;
        const char* e = lua_tolstring(co, -1, &n);
        std::string msg = e ? std::string(e, n) : "Luau load failed";
        state.reset();
        return error_json(msg);
    }
    if(source_bindings)mark_executor_closure(co,-1);
    for (double arg : args) lua_pushnumber(co, arg);
    int status;
    try {
        status = scheduler ? scheduler->run(co,static_cast<int>(args.size()),[&](){if(world)world_bindings::dispatch_events(co,*world);})
                           : lua_resume(co, L, static_cast<int>(args.size()));
    } catch(const std::exception& e) {
        state.reset();
        return world ? world_error_json(*world,std::string("source scheduler failed: ")+e.what())
                     : error_json(std::string("source scheduler failed: ")+e.what());
    }
    if (limits.timed_out || Clock::now() >= limits.deadline) {
        state.reset();
        return world ? world_error_json(*world,"execution time limit exceeded",true) : error_json("execution time limit exceeded", true);
    }
    if (status != 0) {
        size_t n = 0;
        const char* e = lua_tolstring(co, -1, &n);
        std::string msg = e ? std::string(e, n) : "Luau runtime error";
        state.reset();
        return world ? world_error_json(*world,msg) : error_json(msg);
    }
    if(world && world->event_overflow){state.reset();return world_error_json(*world,"local event queue limit exceeded");}
    if(world){
        using world_bindings::Json;
        Json result{{"ok",true},{"values",Json::array()},{"output",world->output},
                    {"output_truncated",world->output_truncated},{"mutations",world->mutations}};
        result["movement_intents"]=Json::parse(movement_bindings::drain(co));
        if(source_bindings){
            result["console_events"]=Json::parse(source_host_io(co,R"({"action":"drain"})"));
            result["teleport_queue"]=Json::parse(source_host_io(co,R"({"action":"teleport_peek"})"));
        }
        result["joint_pose_errors"]=world->joint_pose_errors;
        if(scheduler&&!scheduler->errors().empty())result["task_errors"]=scheduler->errors();
        try { for(int i=1;i<=lua_gettop(co);++i) result["values"].push_back(world_bindings::capture_value(co,*world,i)); }
        catch(const std::exception&e){state.reset();return world_error_json(*world,std::string("result capture failed: ")+e.what());}
        std::string out=result.dump(-1,' ',false,Json::error_handler_t::replace)+"\n";
        if(out.size()>kMaxOutput){state.reset();return world_error_json(*world,"world result exceeds output limit");}
        state.reset();return out;
    }
    std::string out = "{\"ok\":true,\"values\":[";
    const int count = lua_gettop(co);
    size_t strings = 0;
    for (int i = 1; i <= count; ++i) {
        if (i > 1) out += ',';
        if (!emit_value(co, i, out, strings)) {
            state.reset();
            return error_json("return values must be nil, boolean, finite number, or bounded string");
        }
    }
    out += "]";
    if(scheduler&&!scheduler->errors().empty())out += ",\"task_errors\":"+world_bindings::Json(scheduler->errors()).dump();
    if(source_bindings) {
        out += ",\"console_events\":"+source_host_io(co,R"({"action":"drain"})");
        out += ",\"teleport_queue\":"+source_host_io(co,R"({"action":"teleport_peek"})");
    }
    out += "}\n";
    state.reset();
    return out;
    };
    using world_bindings::Json;
    Json result;
    try { result=Json::parse(execute()); }
    catch(const std::exception& e) { result=Json::parse(error_json(std::string("VM worker failed: ")+e.what())); }
    auto add_observations=[&](Json& target) {
        const auto observed=protected_scripts->snapshot();
        target["runtime_protected_integrity"]={{"version",1},{"constructions",observed.constructions},
            {"copies",observed.copies},{"failures",observed.failures}};
        // A standalone VM has no World owner to check; do not invent a check.
        if(world)target["runtime_thread_integrity"]=world->script_thread_report();
    };
    add_observations(result);
    auto serialized=result.dump(-1,' ',false,Json::error_handler_t::replace)+"\n";
    if(serialized.size()>kMaxOutput) {
        result=Json::parse(error_json("worker result exceeds output limit"));
        add_observations(result);
        serialized=result.dump()+"\n";
    }
    return serialized;
}

class PersistentSourceSession {
    using Json=world_bindings::Json;
    struct Job {uint64_t id;lua_State* thread;uint64_t attribute_read_start;};
    Limits limits;
    SourceBindings bindings;
    std::unique_ptr<world_bindings::World> world;
    lua_State* main=nullptr;
    lua_State* environment=nullptr;
    std::unique_ptr<SourceScheduler> scheduler;
    std::vector<Job> jobs;
    std::unique_ptr<ScriptExecution> script_execution;
    uint64_t next_job=1;
    std::string request_operation,request_stage;

    Json job_diagnostics() const {
        Json output=Json::array();
        for(const auto& job:jobs) {
            Json item{{"job_id",job.id},{"scheduler_state",scheduler->task_state(job.thread)},
                      {"frames",Json::array()}};
            const auto progress=scheduler->task_progress(job.thread);
            item["resume_count"]=progress.resumes;item["yield_count"]=progress.yields;
            item["slice_interruptions"]=progress.interruptions;item["execution_ms"]=progress.execution_ms;
            for(auto read=world->attribute_reads.rbegin();read!=world->attribute_reads.rend();++read)
                if(read->first==job.thread&&read->second.at("sequence").get<uint64_t>()>job.attribute_read_start)
                    {item["last_attribute_read"]=read->second;break;}
            lua_Debug frame{};
            for(int depth=0;depth<8&&lua_getinfo(job.thread,depth,"sln",&frame);++depth)
                item["frames"].push_back(Json{{"source",frame.short_src?frame.short_src:"?"},
                    {"line",frame.currentline},{"function",std::string(frame.name?frame.name:"").substr(0,128)}});
            for(const auto& waiter:world->child_waiters)if(waiter.thread==job.thread) {
                item["wait"]={{"kind","child"},{"parent_id",waiter.parent},{"name",waiter.name.substr(0,256)},
                              {"name_truncated",waiter.name.size()>256},
                              {"has_timeout",waiter.deadline!=Clock::time_point::max()}};break;
            }
            for(const auto& [signal,waiters]:world->waiters)for(const auto& waiter:waiters)
                if(waiter.thread==job.thread)item["wait"]={{"kind","signal"},
                    {"instance_id",signal->node?signal->node->id:""},{"name",signal->key}};
            output.push_back(std::move(item));
        }
        return output;
    }

    Json collect_jobs() {
        Json completed=Json::array();
        for(auto it=jobs.begin();it!=jobs.end();) {
            int status=scheduler->completion(it->thread);
            if(status==LUA_YIELD){++it;continue;}
            Json result{{"job_id",it->id},{"ok",status==0},{"values",Json::array()},
                        {"completed",true},{"state",status==0?"completed":"failed"}};
            if(status==0) {
                try{for(int i=1;i<=lua_gettop(it->thread);++i)result["values"].push_back(world_bindings::capture_value(it->thread,*world,i));}
                catch(const std::exception& e){result["ok"]=false;result["state"]="failed";result["values"]=Json::array();result["error"]=e.what();}
            } else {
                size_t size=0;const char* error=lua_tolstring(it->thread,-1,&size);
                result["error"]=status==LUA_BREAK?"source task exceeded its CPU step budget":error?std::string(error,size):"source task failed or was cancelled";
                result["timed_out"]=status==LUA_BREAK;
            }
            completed.push_back(std::move(result));
            for(auto read=world->attribute_reads.begin();read!=world->attribute_reads.end();)
                if(read->first==it->thread)read=world->attribute_reads.erase(read);else ++read;
            scheduler->release_root(it->thread);it=jobs.erase(it);
        }
        return completed;
    }
public:
    Json failure_context() const {
        return {{"operation",request_operation},{"stage",request_stage}};
    }
    PersistentSourceSession(const std::string& snapshot,int memory_mb,SourceBindings source_bindings):bindings(source_bindings) {
        if(snapshot.size()>kMaxWorldSnapshot||memory_mb<8||memory_mb>local_test_memory_limit())throw std::runtime_error("invalid persistent VM limits");
        world=std::make_unique<world_bindings::World>(world_bindings::parse_snapshot(snapshot));
        world->persistent=true;
        const char* trace_remote_calls=std::getenv("RCR_TRACE_REMOTE_CALLS");
        world->trace_remote_calls=trace_remote_calls&&std::strcmp(trace_remote_calls,"1")==0;
        const char* trace_attributes=std::getenv("RCR_TRACE_ATTRIBUTE_READS");
        world->trace_attribute_reads=trace_attributes&&std::strcmp(trace_attributes,"1")==0;
        limits.cap=static_cast<size_t>(memory_mb)*1024*1024;limits.deadline=Clock::now()+std::chrono::seconds(2);
        main=lua_newstate(capped_alloc,&limits);if(!main)throw std::runtime_error("cannot allocate persistent Luau VM");
        try {
            auto* callbacks=lua_callbacks(main);callbacks->userdata=&limits;callbacks->interrupt=interrupt;
            install_protected_error_trace(main);
            luaL_openlibs(main);
            for(const char* name:{"require","loadstring","dofile","loadfile","io","debug"})remove_global(main,name);
            lua_getglobal(main,"os");if(lua_istable(main,-1)){lua_pushnil(main);lua_setfield(main,-2,"exit");}lua_pop(main,1);
            install_random_compat(main);install_authored_bindings(main);world_bindings::install(main,*world);luaL_sandbox(main);
            environment=lua_newthread(main);luaL_sandboxthread(environment);
            lua_setsafeenv(environment,LUA_GLOBALSINDEX,false);
            install_source_bindings(environment,&bindings,world->script_thread_integrity.get(),world->protected_scripts.get());
            scheduler=std::make_unique<SourceScheduler>(main,limits.deadline,true);scheduler->install(environment);world->scheduler=scheduler.get();
            script_execution=std::make_unique<ScriptExecution>(environment,*world,*scheduler,world_bindings::Json::parse(snapshot));
            luaL_sandboxthread(environment);lua_setsafeenv(environment,LUA_GLOBALSINDEX,false);
        } catch(const std::exception& error) {
            // Luau exceptions may own a reference to this VM's error stack.
            // Materialize the message before destroying the state.
            std::string message=error.what();lua_close(main);main=nullptr;throw std::runtime_error(message);
        } catch(...) {lua_close(main);main=nullptr;throw;}
    }
    ~PersistentSourceSession(){if(main)lua_close(main);}
    void add_integrity_observations(Json& response) const {
        response["runtime_thread_integrity"]=world->script_thread_report();
        response["runtime_protected_integrity"]=world->protected_script_report();
    }
    Json request(const Json& command) {
        request_operation=command.value("op",std::string());request_stage="validate";
        if(!command.is_object())throw std::runtime_error("persistent request must be an object");
        if(command.value("op",std::string())=="render")
            return Json{{"ok",true},{"values",Json::array()},{"frame",world_bindings::render_snapshot(*world)}};
        scheduler->begin_frame();
        const std::string operation=command.at("op").get<std::string>();
        int budget=command.value("timeout_ms",operation=="execute"?250:10);
        if(budget<1||budget>2000)throw std::runtime_error("CPU step budget must be 1..2000 ms");
        limits.timed_out=false;limits.deadline=Clock::now()+std::chrono::milliseconds(budget);
        if(command.contains("text_service_responses"))world_bindings::complete_text_service(environment,*world,command.at("text_service_responses"));
        if(operation=="host_io") {
            return Json{{"ok",true},{"values",Json::array()},{"persistent",true},
                        {"host_io",Json::parse(source_host_io(environment,command.at("command").dump()))}};
        }
        if(operation=="physics_feedback") {
            request_stage="physics_feedback";
            world_bindings::update_physics_feedback(environment,*world,command.at("feedback"));
            return Json{{"ok",true},{"values",Json::array()},{"persistent",true}};
        }
        request_stage="script_reconcile_before";
        Json script_status=script_execution->reconcile();
        uint64_t started=0;
        const Json* engine_poses=nullptr;
        Json engine_pose_result=nullptr;
        const Json* engine_owned_parts=nullptr;
        Json engine_owned_part_result=nullptr;
        if(operation=="execute") {
            if(jobs.size()>=128)throw std::runtime_error("too many active source jobs");
            const auto source=command.at("source").get<std::string>();
            if(source.size()>kMaxSource)throw std::runtime_error("source exceeds 100 MiB");
            std::string code=Luau::compile(source);
            lua_State* thread=lua_newthread(environment);
            int load=world->load_script(thread,"=persistent-source",code,source);
            if(load) {
                size_t size=0;const char* error=lua_tolstring(thread,-1,&size);std::string message=error?std::string(error,size):"source compilation failed";
                lua_pop(environment,1);
                return Json{{"ok",false},{"values",Json::array()},{"completed",true},{"state","failed"},{"error",message},
                            {"runtime_thread_integrity",world->script_thread_report()},
                            {"runtime_protected_integrity",world->protected_script_report()}};
            }
            mark_executor_closure(thread,-1);
            started=next_job++;jobs.push_back(Job{started,thread,world->attribute_read_sequence});
            scheduler->start(thread);lua_pop(environment,1);
        } else if(operation=="update") {
            request_stage="world_update";
            world_bindings::update_snapshot(environment,*world,command.at("delta").dump());
            if(command.at("delta").contains("script_sources"))script_execution->update(command.at("delta").at("script_sources"));
            if(command.at("delta").contains("initial_replication_complete"))script_execution->set_initial_ready(command.at("delta").at("initial_replication_complete").get<bool>());
            if(command.at("delta").contains("engine_poses"))engine_poses=&command.at("delta").at("engine_poses");
            if(command.at("delta").contains("engine_owned_part_updates"))engine_owned_parts=&command.at("delta").at("engine_owned_part_updates");
        } else if(operation=="chat") {
            world_bindings::complete_chat(environment,*world,command.at("responses"));
        } else if(operation=="remote_server_results") {
            world_bindings::complete_remote_server(environment,*world,command.at("responses"));
        } else if(operation=="gui_action") {
            world_bindings::render_gui_action(*world,command.at("action"));
        } else if(operation!="poll"&&operation!="init")throw std::runtime_error("unknown persistent source operation");
        request_stage="script_reconcile_after";
        script_status=script_execution->reconcile();
        if(engine_poses)engine_pose_result=world_bindings::apply_engine_poses(*world,*engine_poses);
        if(engine_owned_parts)engine_owned_part_result=world_bindings::apply_engine_owned_parts(*world,*engine_owned_parts);
        // World updates and script reconciliation are non-yielding host work.
        // They can exhaust a short transaction deadline before any ready job
        // runs. Reserve a bounded slice after that work, rather than leaving
        // ready jobs queued forever. A request gains at most five milliseconds;
        // the scheduler's accumulated non-yielding task limit still applies.
        limits.deadline=std::max(limits.deadline,Clock::now()+std::chrono::milliseconds(std::min(budget,5)));
        request_stage="scheduler";
        scheduler->step(limits.deadline,[&](){world_bindings::dispatch_events(environment,*world);},256,
                        [&](Clock::time_point deadline){limits.deadline=deadline;},
                        [&](){world_bindings::progress_invocations(environment,*world);},
                        [&](){
                            // Deliver work awaiting the host as soon as a task
                            // produces it. Spending the rest of a CPU slice on
                            // unrelated callbacks delays even an already-ready
                            // request or reply. Remaining tasks stay scheduled
                            // and continue in the next host transaction.
                            return !world->text_service_requests.empty()||!world->chat_requests.empty()||!world->remote_client_replies.empty()
                                ||!world->remote_server_events.empty()||!world->remote_server_calls.empty();
                        });
        request_stage="script_reconcile_final";
        script_status=script_execution->reconcile();
        request_stage="collect_jobs";
        Json completed=collect_jobs();
        Json response{{"ok",true},{"values",Json::array()},{"persistent",true},{"completed_jobs",completed},
            {"engine_owned_part_feedback_version",1},
            {"script_runtime",script_status},
            {"runtime_thread_integrity",world->script_thread_report()},
            {"runtime_protected_integrity",world->protected_script_report()},
            {"output",world->output},{"output_truncated",world->output_truncated},{"mutations",world->mutations},
            {"task_errors",scheduler->errors()},{"active_jobs",jobs.size()},
            {"pending_tasks",scheduler->pending()+world->events.size()},{"pending_events",world->events.size()},
            {"world_revision",world->sequence},{"server_actions",world->server_actions},
            {"server_events",world->server_events},{"unresolved_server_events",world->unresolved_server_events}};
        if(engine_pose_result.is_object()) {
            response["engine_pose_status"]=engine_pose_result.value("status",std::string("unknown"));
            response["engine_pose_frames_applied"]=engine_pose_result.value("frames_applied",size_t(0));
            response["engine_pose_frames_carried"]=engine_pose_result.value("frames_carried",size_t(0));
        }
        if(engine_owned_part_result.is_object()) {
            response["engine_owned_part_status"]=engine_owned_part_result.value("status",std::string("unknown"));
            response["engine_owned_part_frames_applied"]=engine_owned_part_result.value("frames_applied",size_t(0));
        }
        if(operation=="gui_action")response["textFocus"]={{"id",world->focused_textbox},{"revision",world->text_focus_revision}};
        response["chat_requests"]=world->chat_requests;world->chat_requests=Json::array();
        response["remote_client_replies"]=world->remote_client_replies;world->remote_client_replies=Json::array();
        response["remote_server_events"]=std::move(world->remote_server_events);
        world->remote_server_events=Json::array();world->remote_server_event_bytes=0;
        response["remote_server_calls"]=std::move(world->remote_server_calls);
        world->remote_server_calls=Json::array();world->remote_server_call_bytes=0;
        response["remote_call_diagnostics"]=std::move(world->remote_call_diagnostics);
        world->remote_call_diagnostics=Json::array();
        response["active_remote_server_requests"]=Json::array();
        for(const auto& waiter:world->remote_server_waiters)response["active_remote_server_requests"].push_back(waiter.request);
        response["animation_requests"]=world->animation.requests;world->animation.requests=Json::array();
        response["text_service_requests"]=std::move(world->text_service_requests);world->text_service_requests=Json::array();
        response["active_text_service_requests"]=Json::array();
        for(const auto& waiter:world->text_service_waiters)response["active_text_service_requests"].push_back(waiter.request);
        response["animation_intents"]=world->animation.intents;world->animation.intents=Json::array();
        request_stage="movement_drain";
        response["movement_intents"]=Json::parse(movement_bindings::drain(environment));
        request_stage="console_drain";
        response["console_events"]=Json::parse(source_host_io(environment,R"({"action":"drain"})"));
        request_stage="job_diagnostics";
        response["job_diagnostics"]=job_diagnostics();
        Json event_kinds=Json::object();
        for(const auto& event:world->events) {
            std::string kind=event.connection?event.connection->signal->key:"signal_wait";
            if(event_kinds.size()>=32&&!event_kinds.contains(kind))kind="other";
            event_kinds[kind]=event_kinds.value(kind,size_t(0))+1;
        }
        response["pending_event_kinds"]=std::move(event_kinds);
        response["joint_pose_errors"]=world->joint_pose_errors;world->joint_pose_errors=Json::array();
        response["active_chat_requests"]=Json::array();for(const auto& waiter:world->chat_waiters)response["active_chat_requests"].push_back(waiter.request);
        if(started) {
            response["job_id"]=started;response["state"]="running";response["completed"]=false;
            for(const auto& result:completed)if(result["job_id"]==started)
                for(auto it=result.begin();it!=result.end();++it)response[it.key()]=it.value();
        }
        if(world->event_overflow){response["ok"]=false;response["error"]="local event queue limit exceeded";}
        world->output.clear();world->output_truncated=false;world->mutations=Json::array();world->mutation_bytes=0;
        scheduler->clear_errors();world->event_overflow=false;
        return response;
    }
};

bool parse_num(const char* s, double& value) {
    char* end = nullptr;
    errno = 0;
    value = std::strtod(s, &end);
    return errno == 0 && end != s && *end == '\0' && std::isfinite(value);
}

bool parse_context_file(const std::string& path, LocalEngineContext& context, std::string& error) {
    if (path.empty()) return true;
    std::ifstream f(std::filesystem::u8path(path), std::ios::binary | std::ios::ate);
    if (!f) { error = "cannot read local context file"; return false; }
    auto size = f.tellg();
    if (size < 0 || size > 4096) { error = "context file exceeds 4 KiB"; return false; }
    std::string contents(static_cast<size_t>(size), '\0');
    f.seekg(0);
    if (!contents.empty() && !f.read(contents.data(), static_cast<std::streamsize>(contents.size()))) {
        error = "cannot read local context file"; return false;
    }
    std::set<std::string> seen;
    bool mode_ok = false, studio_seen = false, job_seen = false;
    bool session_metadata = false;
    size_t offset = 0;
    while (offset < contents.size()) {
        size_t end = contents.find('\n', offset);
        if (end == std::string::npos) end = contents.size();
        std::string line = contents.substr(offset, end - offset);
        offset = end + 1;
        if (line.empty()) continue;
        for (unsigned char c : line) {
            if (c < 0x20 || c > 0x7e) { error = "context file must contain printable ASCII and LF newlines"; return false; }
        }
        size_t eq = line.find('=');
        if (eq == std::string::npos) { error = "invalid context key/value line"; return false; }
        std::string key = line.substr(0, eq), value = line.substr(eq + 1);
        if (!seen.insert(key).second) { error = "duplicate context key"; return false; }
        if (key == "mode") {
            if (value != "offline" && value != "session") {
                error = "context mode must be offline or session"; return false;
            }
            session_metadata = value == "session";
            mode_ok = true;
        } else if (key == "job_id") {
            if (value.empty() || value.size() > 128) {
                error = "invalid job_id length"; return false;
            }
            context.job_id = value; job_seen = true;
        } else if (key == "is_studio") {
            if (value != "true" && value != "false") { error = "is_studio must be true or false"; return false; }
            context.is_studio = value == "true"; studio_seen = true;
        } else if (key == "services") {
            context.restrict_services = true;
            const std::set<std::string> allowed = {"Workspace", "Players", "RunService", "Lighting",
                "ReplicatedStorage", "ReplicatedFirst", "StarterGui", "StarterPlayer", "SoundService",
                "TweenService", "UserInputService", "HttpService", "CollectionService",
                "ContextActionService", "GuiService", "Teams", "TextService", "CoreGui"};
            size_t start = 0;
            while (start < value.size()) {
                size_t comma = value.find(',', start);
                if (comma == std::string::npos) comma = value.size();
                std::string name = value.substr(start, comma - start);
                if (!allowed.count(name) || !context.services.insert(name).second) {
                    error = "unknown or duplicate local service"; return false;
                }
                start = comma + 1;
            }
        } else { error = "unsupported local context key"; return false; }
    }
    if (!mode_ok || !job_seen || !seen.count("services")) {
        error = "context requires mode, job_id, and services"; return false;
    }
    if (session_metadata) {
        // This host is an interpreter client, not the Studio editor. Keep its
        // execution mode independent of caller-supplied expected check values.
        if (studio_seen) { error = "session mode cannot override IsStudio"; return false; }
        const std::string& id = context.job_id;
        if (id.size() != 36) { error = "session job_id must be a UUID"; return false; }
        for (size_t i = 0; i < id.size(); ++i) {
            const bool hyphen = i == 8 || i == 13 || i == 18 || i == 23;
            const char c = id[i];
            const bool hex = (c >= '0' && c <= '9') || (c >= 'a' && c <= 'f') ||
                             (c >= 'A' && c <= 'F');
            if (hyphen ? c != '-' : !hex) { error = "invalid session job_id"; return false; }
        }
        context.is_studio = false;
    } else if (!studio_seen || context.job_id.rfind("offline-fixture-", 0) != 0) {
        error = "offline context requires synthetic job_id and explicit is_studio"; return false;
    }
    return true;
}

bool validate_bytecode_header(const std::string& bytes, std::string& error) {
    if (bytes.size() < 2) { error = "bytecode header is incomplete"; return false; }
    const uint8_t version = static_cast<uint8_t>(bytes[0]);
    if (version < LBC_VERSION_MIN || version > LBC_VERSION_MAX) {
        error = "unsupported Luau bytecode version or encoded envelope"; return false;
    }
    if (version >= 4) {
        const uint8_t types = static_cast<uint8_t>(bytes[1]);
        if (types < LBC_TYPE_VERSION_MIN || types > LBC_TYPE_VERSION_MAX) {
            error = "unsupported Luau bytecode type version"; return false;
        }
    }
    return true;
}

#ifndef _WIN32
void write_all(int fd, const char* data, size_t size) {
    while (size) {
        ssize_t n = ::write(fd, data, size);
        if (n > 0) { data += n; size -= static_cast<size_t>(n); }
        else if (n < 0 && errno == EINTR) continue;
        else break;
    }
}

#endif

int main_cli(int argc, char** argv) {
    std::string source_path;
    std::string bytecode_path;
    std::string context_path;
    std::string world_snapshot_path;
    int timeout_ms = 250;
    int memory_mb = 512;
    bool args_started = false;
    bool internal_worker = false;
    std::vector<double> args;
    for (int i = 1; i < argc; ++i) {
        if (!args_started && std::strcmp(argv[i], "--internal-worker") == 0) { internal_worker=true; continue; }
        if (!args_started && std::strcmp(argv[i], "--") == 0) { args_started = true; continue; }
        if (args_started) {
            double n;
            if (!parse_num(argv[i], n)) { std::cout << error_json("arguments after -- must be finite numbers"); return 2; }
            args.push_back(n);
        } else if (std::strcmp(argv[i], "--source") == 0 && i + 1 < argc) source_path = argv[++i];
        else if (std::strcmp(argv[i], "--bytecode") == 0 && i + 1 < argc) bytecode_path = argv[++i];
        else if (std::strcmp(argv[i], "--timeout-ms") == 0 && i + 1 < argc) timeout_ms = std::atoi(argv[++i]);
        else if (std::strcmp(argv[i], "--memory-mb") == 0 && i + 1 < argc) memory_mb = std::atoi(argv[++i]);
        else if (std::strcmp(argv[i], "--context-file") == 0 && i + 1 < argc) context_path = argv[++i];
        else if (std::strcmp(argv[i], "--world-snapshot") == 0 && i + 1 < argc) world_snapshot_path = argv[++i];
        else { std::cout << error_json("usage: luau-host (--source PATH | --bytecode PATH) [--context-file PATH] [--world-snapshot PATH] [--timeout-ms 1..2000] [--memory-mb 8..512] [-- NUMBER ...]"); return 2; }
    }
    if ((source_path.empty() == bytecode_path.empty()) || timeout_ms < 1 || timeout_ms > 2000 || memory_mb < 8 || memory_mb > 512 || args.size() > 64) {
        std::cout << error_json("invalid source or resource limit"); return 2;
    }
    const bool is_bytecode = !bytecode_path.empty();
    if ((!world_snapshot_path.empty() && (is_bytecode || !context_path.empty()))) {
        std::cout << error_json("world snapshots require source mode and cannot be combined with a context file"); return 2;
    }
    const std::string input_path = is_bytecode ? bytecode_path : source_path;
    std::ifstream f(std::filesystem::u8path(input_path), std::ios::binary | std::ios::ate);
    if (!f) { std::cout << error_json("cannot read input file"); return 2; }
    auto source_size = f.tellg();
    if (source_size < 0 || static_cast<uint64_t>(source_size) > kMaxSource) {
        std::cout << error_json("input exceeds 100 MiB limit"); return 2;
    }
    std::string source(static_cast<size_t>(source_size), '\0');
    f.seekg(0);
    if (!source.empty() && !f.read(source.data(), static_cast<std::streamsize>(source.size()))) {
        std::cout << error_json("cannot read input file"); return 2;
    }
    if (is_bytecode) {
        std::string bytecode_error;
        if (!validate_bytecode_header(source, bytecode_error)) {
            std::cout << error_json(bytecode_error); return 2;
        }
    }
    LocalEngineContext engine_context;
    std::string context_error;
    if (!context_path.empty() && !parse_context_file(context_path, engine_context, context_error)) {
        std::cout << error_json(context_error); return 2;
    }
    std::string world_snapshot_text;
    if (!world_snapshot_path.empty()) {
        std::ifstream snapshot(std::filesystem::u8path(world_snapshot_path),std::ios::binary|std::ios::ate);
        if(!snapshot){std::cout<<error_json("cannot read world snapshot");return 2;}
        auto n=snapshot.tellg();if(n<0||static_cast<uint64_t>(n)>kMaxWorldSnapshot){std::cout<<error_json("world snapshot exceeds 8 MiB");return 2;}
        world_snapshot_text.resize(static_cast<size_t>(n));snapshot.seekg(0);
        if(!world_snapshot_text.empty()&&!snapshot.read(world_snapshot_text.data(),static_cast<std::streamsize>(world_snapshot_text.size()))){std::cout<<error_json("cannot read world snapshot");return 2;}
    }

#ifdef _WIN32
    if(internal_worker) {
        try { std::cout << run_vm(source,args,is_bytecode,timeout_ms,memory_mb,
            context_path.empty()?nullptr:&engine_context,world_snapshot_path.empty()?nullptr:&world_snapshot_text); }
        catch(...) { std::cout << error_json("VM worker failed"); }
        return 0;
    }
    // Re-execute the same CLI in a child; the parent drains stdout while
    // enforcing a wall deadline, just as the POSIX fork supervisor does.
    wchar_t executable[32768];
    DWORD path_size=GetModuleFileNameW(nullptr,executable,32768);
    if(!path_size||path_size>=32768){std::cout<<error_json("cannot locate VM executable");return 2;}
    std::wstring command=GetCommandLineW();
    // Insert before the original arguments (which may contain the -- marker).
    auto first=command.find(L'"',1);
    if(command[0]==L'"')first=first==std::wstring::npos?command.size():first+1;
    else first=command.find(L' ');
    if(first==std::wstring::npos)first=command.size();
    command.insert(first,L" --internal-worker");
    // Windows does not reap a child when its parent is terminated. Own the VM
    // in a kill-on-close job so Python timeouts also stop the actual worker.
    HANDLE job=CreateJobObjectW(nullptr,nullptr);
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION limits{};
    limits.BasicLimitInformation.LimitFlags=JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    if(!job||!SetInformationJobObject(job,JobObjectExtendedLimitInformation,&limits,sizeof(limits))) {
        if(job)CloseHandle(job);
        std::cout<<error_json("cannot create VM process job");return 2;
    }
    SECURITY_ATTRIBUTES security{sizeof(SECURITY_ATTRIBUTES),nullptr,TRUE};
    HANDLE reader=nullptr,writer=nullptr;
    if(!CreatePipe(&reader,&writer,&security,0)){CloseHandle(job);std::cout<<error_json("cannot create result pipe");return 2;}
    SetHandleInformation(reader,HANDLE_FLAG_INHERIT,0);
    STARTUPINFOW startup{};startup.cb=sizeof(startup);startup.dwFlags=STARTF_USESTDHANDLES;
    startup.hStdInput=GetStdHandle(STD_INPUT_HANDLE);startup.hStdOutput=writer;startup.hStdError=GetStdHandle(STD_ERROR_HANDLE);
    PROCESS_INFORMATION process{};
    BOOL created=CreateProcessW(executable,command.data(),nullptr,nullptr,TRUE,CREATE_NO_WINDOW|CREATE_SUSPENDED,nullptr,nullptr,&startup,&process);
    CloseHandle(writer);
    if(!created){CloseHandle(reader);CloseHandle(job);std::cout<<error_json("cannot start VM worker");return 2;}
    if(!AssignProcessToJobObject(job,process.hProcess)||ResumeThread(process.hThread)==DWORD(-1)) {
        TerminateProcess(process.hProcess,2);WaitForSingleObject(process.hProcess,1000);
        CloseHandle(process.hThread);CloseHandle(process.hProcess);CloseHandle(reader);CloseHandle(job);
        std::cout<<error_json("cannot supervise VM worker");return 2;
    }
    CloseHandle(process.hThread);
    std::string result;bool overflow=false,timed_out=false;
    const auto deadline=Clock::now()+std::chrono::milliseconds(timeout_ms+2000);
    for(;;) {
        DWORD available=0;
        while(PeekNamedPipe(reader,nullptr,0,nullptr,&available,nullptr)&&available) {
            char buffer[8192];DWORD count=0;
            if(!ReadFile(reader,buffer,std::min<DWORD>(available,sizeof(buffer)),&count,nullptr)||!count)break;
            if(result.size()+count>kMaxOutput)overflow=true;else result.append(buffer,count);
        }
        if(WaitForSingleObject(process.hProcess,0)==WAIT_OBJECT_0) {
            // One last drain after the writer has closed.
            if(!PeekNamedPipe(reader,nullptr,0,nullptr,&available,nullptr)||!available)break;
        }
        if(Clock::now()>=deadline){TerminateProcess(process.hProcess,124);WaitForSingleObject(process.hProcess,1000);timed_out=true;break;}
        Sleep(2);
    }
    CloseHandle(reader);CloseHandle(process.hProcess);CloseHandle(job);
    if(timed_out){std::cout<<error_json("worker wall time limit exceeded",true);return 124;}
    if(overflow)std::cout<<error_json("result size limit exceeded");
    else if(result.empty())std::cout<<error_json("VM worker returned no result");
    else std::cout<<result;
    return 0;
#else
    int pipefd[2];
    if (pipe(pipefd) != 0) { std::cout << error_json("cannot create result pipe"); return 2; }
    pid_t pid = fork();
    if (pid < 0) { close(pipefd[0]); close(pipefd[1]); std::cout << error_json("cannot start VM worker"); return 2; }
    if (pid == 0) {
        close(pipefd[0]);
        std::string result;
        try { result = run_vm(source, args, is_bytecode, timeout_ms, memory_mb, context_path.empty() ? nullptr : &engine_context,
                             world_snapshot_path.empty() ? nullptr : &world_snapshot_text); }
        catch (...) { result = error_json("VM worker failed"); }
        write_all(pipefd[1], result.data(), result.size());
        close(pipefd[1]);
        _exit(0);
    }
    close(pipefd[1]);
    fcntl(pipefd[0], F_SETFL, fcntl(pipefd[0], F_GETFL) | O_NONBLOCK);
    const auto deadline = Clock::now() + std::chrono::milliseconds(timeout_ms + 2000);
    std::string result;
    bool done = false, overflow = false;
    int child_status = 0;
    while (!done) {
        char buf[8192];
        while (true) {
            ssize_t n = read(pipefd[0], buf, sizeof(buf));
            if (n > 0) {
                if (result.size() + static_cast<size_t>(n) > kMaxOutput) overflow = true;
                else result.append(buf, static_cast<size_t>(n));
            } else break;
        }
        pid_t w = waitpid(pid, &child_status, WNOHANG);
        if (w == pid) { done = true; break; }
        if (Clock::now() >= deadline) {
            kill(pid, SIGKILL); waitpid(pid, &child_status, 0); close(pipefd[0]);
            std::cout << error_json("worker wall time limit exceeded", true);
            return 124;
        }
        pollfd pfd{pipefd[0], POLLIN, 0};
        poll(&pfd, 1, 10);
    }
    char buf[8192];
    while (true) {
        ssize_t n = read(pipefd[0], buf, sizeof(buf));
        if (n > 0) {
            if (result.size() + static_cast<size_t>(n) > kMaxOutput) overflow = true;
            else result.append(buf, static_cast<size_t>(n));
        } else break;
    }
    close(pipefd[0]);
    if (overflow) std::cout << error_json("result size limit exceeded");
    else if (result.empty()) std::cout << error_json("VM worker returned no result");
    else std::cout << result;
    return 0;
#endif
}
} // namespace

#ifdef LOCAL_LUAU_FFI
// Source-only boundary for the Rust process supervisor. Authentication keeps
// using the independent verified-bytecode path above.
extern "C" int local_luau_source(const unsigned char* source, size_t source_len,
    const double* arguments, size_t argument_count,
    const unsigned char* snapshot, size_t snapshot_len,
    int timeout_ms, int memory_mb, char** output, size_t* output_len,
    int (*codec)(unsigned int,const unsigned char*,size_t,unsigned char**,size_t*,int),
    void (*release)(unsigned char*,size_t)) noexcept {
    if (!output || !output_len) return 1;
    *output = nullptr;
    *output_len = 0;
    try {
        if ((!source && source_len) || source_len > local_test_source_limit(kMaxSource) ||
            (!arguments && argument_count) || argument_count > 64 ||
            (!snapshot && snapshot_len) || snapshot_len > kMaxWorldSnapshot ||
            timeout_ms < 1 || timeout_ms > local_test_time_limit() || memory_mb < 8 || memory_mb > local_test_memory_limit() ||
            !codec || !release)
            return 1;
        std::vector<double> args;
        for (size_t i = 0; i < argument_count; ++i) {
            if (!std::isfinite(arguments[i])) return 1;
            args.push_back(arguments[i]);
        }
        std::string text(source ? reinterpret_cast<const char*>(source) : "", source_len);
        std::string world(snapshot ? reinterpret_cast<const char*>(snapshot) : "", snapshot_len);
        SourceBindings bindings{codec,release};
        std::string result = run_vm(text, args, false, timeout_ms, memory_mb,
            nullptr, snapshot ? &world : nullptr, &bindings);
        if (result.size() > 1024 * 1024) return 1;
        auto* buffer = static_cast<char*>(std::malloc(result.size()));
        if (!buffer) return 1;
        std::memcpy(buffer, result.data(), result.size());
        *output = buffer;
        *output_len = result.size();
        return 0;
    } catch (...) {
        return 1;
    }
}

extern "C" void local_luau_free(char* output) noexcept { std::free(output); }
extern "C" int local_luau_session_request(void** handle,const unsigned char* input,size_t length,
    char** output,size_t* output_len,
    int (*codec)(unsigned int,const unsigned char*,size_t,unsigned char**,size_t*,int),
    void (*release)(unsigned char*,size_t)) noexcept {
    if(!handle||!output||!output_len||!input||!codec||!release||length>608*1024*1024)return 1;
    *output=nullptr;*output_len=0;
    try {
        using Json=world_bindings::Json;Json response;
        try {
            Json command=Json::parse(input,input+length);
            if(!*handle) {
                if(command.value("op","")!="init")throw std::runtime_error("persistent VM requires init first");
                auto session=std::make_unique<PersistentSourceSession>(command.at("snapshot").dump(),command.value("memory_mb",512),SourceBindings{codec,release});
                response=session->request(Json{{"op","init"}});*handle=session.release();
            } else {
                if(command.value("op","")=="init")throw std::runtime_error("persistent VM is already initialized");
                response=static_cast<PersistentSourceSession*>(*handle)->request(command);
            }
        }catch(const std::exception& e){
            response=Json{{"ok",false},{"values",Json::array()},{"error",e.what()},{"persistent",true}};
            if(*handle)response["runtime_failure_context"]=static_cast<PersistentSourceSession*>(*handle)->failure_context();
        }
        if(*handle)static_cast<PersistentSourceSession*>(*handle)->add_integrity_observations(response);
        std::string data=response.dump(-1,' ',false,Json::error_handler_t::replace);
        if(data.size()>1024*1024) {
            response=Json{{"ok",false},{"values",Json::array()},{"error","persistent response exceeds 1 MiB"}};
            if(*handle)static_cast<PersistentSourceSession*>(*handle)->add_integrity_observations(response);
            data=response.dump();
        }
        auto* result=static_cast<char*>(std::malloc(data.size()));if(!result)return 1;
        std::memcpy(result,data.data(),data.size());*output=result;*output_len=data.size();return 0;
    }catch(...){return 1;}
}
extern "C" void local_luau_session_close(void* handle) noexcept {delete static_cast<PersistentSourceSession*>(handle);}
#else
#ifdef _WIN32
int wmain(int argc,wchar_t** argv) { return host_wide_main(argc,argv,main_cli); }
#else
int main(int argc, char** argv) { return main_cli(argc, argv); }
#endif
#endif
