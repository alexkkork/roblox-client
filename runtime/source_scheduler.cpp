#include "source_scheduler.hpp"
#include "local_test_limits.hpp"
#include "closure_bindings.hpp"
#include <algorithm>
#include <lualib.h>
#include <cmath>
#include <stdexcept>
#include <thread>

namespace {
const auto engine_started=std::chrono::steady_clock::now();
double engine_elapsed(){return std::chrono::duration<double>(std::chrono::steady_clock::now()-engine_started).count();}
}

void SourceScheduler::thread_lifecycle(lua_State* parent,lua_State* thread) {
    if(!parent) {
        delete static_cast<ThreadOwner*>(lua_getthreaddata(thread));
        lua_setthreaddata(thread,nullptr);
        return;
    }
    auto* context=static_cast<ThreadOwner*>(lua_getthreaddata(parent));
    if(context){auto* child=new ThreadOwner{context->scheduler,context->owner};child->identity=context->identity;lua_setthreaddata(thread,child);}
}
void SourceScheduler::coroutine_interrupt(lua_State* parent,lua_Debug* debug) {
    auto* context=static_cast<ThreadOwner*>(lua_getthreaddata(parent));
    if(context)context->interrupted_child=static_cast<lua_State*>(debug->userdata);
}
int SourceScheduler::resume_interrupted(lua_State* thread,lua_State* from,int arguments) {
    std::vector<lua_State*> parents;
    lua_State* target=thread;
    for(;;) {
        auto* context=static_cast<ThreadOwner*>(lua_getthreaddata(target));
        if(!context||!context->interrupted_child)break;
        // coroutine.close/task.cancel can reset a suspended parent. Its old
        // stack no longer retains the child, so never inspect that pointer.
        if(lua_status(target)!=LUA_BREAK){context->interrupted_child=nullptr;break;}
        auto* child=context->interrupted_child;
        if(lua_status(child)!=LUA_BREAK){context->interrupted_child=nullptr;break;}
        parents.push_back(target);target=child;
    }
    int status=lua_resume(target,target==thread?from:nullptr,target==thread?arguments:0);
    while(status!=LUA_BREAK&&!parents.empty()) {
        target=parents.back();parents.pop_back();
        auto* context=static_cast<ThreadOwner*>(lua_getthreaddata(target));
        context->interrupted_child=nullptr;
        // The VM continuation transfers the child's exact return/yield/error
        // values. The scheduler must not consume or reconstruct those values.
        status=lua_resume(target,target==thread?from:nullptr,0);
    }
    return status;
}
void SourceScheduler::set_owner(Task& task,const std::string& owner_name) {
    auto* context=static_cast<ThreadOwner*>(lua_getthreaddata(task.thread));
    if(!context) {
        context=new ThreadOwner{this,owner_name};
        if(owner_name.compare(0,7,"script:")==0)context->identity=2;
        lua_setthreaddata(task.thread,context);
    } else {
        if(context->owner!=owner_name&&owner_name.compare(0,7,"script:")==0)context->identity=2;
        context->owner=owner_name;
    }
    task.owner=owner_name;
}
int SourceScheduler::identity(lua_State* thread) {
    auto* context=static_cast<ThreadOwner*>(lua_getthreaddata(thread));return context?context->identity:8;
}
bool SourceScheduler::has_capability(lua_State* thread,const std::string& capability) {
    const int id=identity(thread);
    if(capability=="None")return true;
    if(capability=="Plugin")return id==1||id==3||id==4||id==5||id==6||id==7||id==8;
    if(capability=="LocalUser")return id==1||id==3||id==4||id==6||id==7||id==8;
    if(capability=="RobloxScript")return id==3||id==6||id==7||id==8;
    if(capability=="WritePlayer"||capability=="RobloxEngine"||capability=="NotAccessible")return id==7||id==8;
    return false;
}
void SourceScheduler::set_identity(lua_State* thread,int value) {
    auto* context=static_cast<ThreadOwner*>(lua_getthreaddata(thread));
    if(!context)throw std::runtime_error("thread security context unavailable");
    context->identity=value;
}
int SourceScheduler::identity_api(lua_State* L) {
    if(lua_toboolean(L,lua_upvalueindex(1))) {
        // Identity is the VM's native integer field. Preserve Luau's numeric
        // conversion even for nonstandard IDs; unknown IDs grant no engine
        // capabilities and do not change the thread's source provenance.
        int value=luaL_checkinteger(L,1);
        // Identities are never negative (the workspace setidentity checks this too).
        luaL_argcheck(L,value>=0,1,"identity must not be negative");
        try{set_identity(L,value);}catch(const std::exception& e){return luaL_error(L,"%s",e.what()),0;}
        return 0;
    }
    lua_pushinteger(L,identity(L));return 1;
}

SourceScheduler::Task& SourceScheduler::track(lua_State* thread) {
    auto found=tasks_.find(thread);if(found!=tasks_.end())return found->second;
    if(tasks_.size()>=2048)collect_finished();
    if(tasks_.size()>=2048)throw std::runtime_error("source task limit exceeded");
    lua_pushthread(thread);int reference=lua_ref(thread,-1);lua_pop(thread,1);
    Task task;task.thread=thread;task.reference=reference;task.owner=owner(thread);
    return tasks_.emplace(thread,task).first->second;
}
const char* SourceScheduler::task_state(lua_State* thread) const {
    auto found=tasks_.find(thread);if(found==tasks_.end())return "untracked";
    switch(found->second.state) {
        case Running:return "executing";case Ready:return "ready";
        case Sleeping:return "sleeping";case Waiting:return "waiting";
        case Done:return "completed";case Cancelled:return "cancelled";
    }
    return "unknown";
}
SourceScheduler::Progress SourceScheduler::task_progress(lua_State* thread) const {
    auto found=tasks_.find(thread);
    if(found==tasks_.end())return {};
    auto progress=found->second.progress;
    progress.response_priority_active=found->second.response_ready;
    return progress;
}
void SourceScheduler::resume(Task& task,lua_State* from) {
    if(task.state!=Ready)return;
    if(task.guard&&!task.guard()){task.state=Done;task.response_ready=false;task.response_priority_cpu=Clock::duration{};return;}
    task.state=Running;
    const auto began=Clock::now();
    int status=resume_interrupted(task.thread,from,task.arguments);task.arguments=0;
    const auto elapsed=Clock::now()-began;
    task.uninterrupted_cpu+=elapsed;
    if(task.response_ready)task.response_priority_cpu+=elapsed;
    ++task.progress.resumes;
    task.progress.execution_ms+=std::chrono::duration<double,std::milli>(elapsed).count();
    if(status==LUA_YIELD)++task.progress.yields;
    if(status==LUA_BREAK)++task.progress.interruptions;
    if(status==LUA_YIELD) {
        task.uninterrupted_cpu=Clock::duration{};
        // A real Luau yield ends the response continuation. If it becomes
        // runnable again later, it joins the normal fair scheduling lanes.
        task.response_ready=false;
        task.response_priority_cpu=Clock::duration{};
        if(task.state==Running)task.state=Waiting;
        return;
    }
    // A persistent worker's short network tick is a scheduling slice, not a
    // lifetime limit for a callback. Rotate interrupted work through the ready
    // queue so another task can run, retaining the cumulative non-yielding cap.
    if(status==LUA_BREAK&&persistent_&&(!task.root||large_local_test())&&
       task.uninterrupted_cpu<std::chrono::milliseconds(local_test_time_limit())) {
        // Bound the response boost separately from the larger task lifetime
        // cap, so a CPU-heavy continuation cannot stay in the priority lane.
        if(task.response_ready&&task.response_priority_cpu>=std::chrono::milliseconds(32)) {
            task.response_ready=false;
            task.response_priority_cpu=Clock::duration{};
        }
        task.state=Ready;enqueue(task);return;
    }
    task.response_ready=false;
    task.response_priority_cpu=Clock::duration{};
    task.state=Done;
    task.result=status;
    if(task.root)root_status_=status;
    else if(status!=0 && task.report_errors && errors_.size()<64) {
        if(status==LUA_BREAK){errors_.push_back("background task exceeded CPU step budget");return;}
        size_t n=0;const char* message=lua_tolstring(task.thread,-1,&n);
        errors_.push_back(message?std::string(message,n):"background task failed");
    }
}
void SourceScheduler::install(lua_State* L) {
    auto* callbacks=lua_callbacks(L);
    if(callbacks->userthread&&callbacks->userthread!=thread_lifecycle)
        throw std::runtime_error("scheduler thread lifecycle already installed");
    if(callbacks->debuginterrupt&&callbacks->debuginterrupt!=coroutine_interrupt)
        throw std::runtime_error("scheduler coroutine interrupt already installed");
    callbacks->userthread=thread_lifecycle;
    callbacks->debuginterrupt=coroutine_interrupt;
    // The VM does not invoke userthread destruction for its main state.
    // It has no script owner; only child threads need metadata storage.
    if(L!=main_&&!lua_getthreaddata(L))lua_setthreaddata(L,new ThreadOwner{this,""});
    lua_newtable(L);
    const char* names[]={"spawn","defer","delay","wait","cancel"};
    for(int i=0;i<5;i++) {
        lua_pushlightuserdata(L,this);lua_pushinteger(L,i);
        lua_pushcclosurek(L,task_api,names[i],2,nullptr);lua_setfield(L,-2,names[i]);
    }
    lua_getfield(L,-1,"wait");lua_setfield(L,LUA_REGISTRYINDEX,"Headless.SchedulerWait");
    lua_setreadonly(L,-1,true);lua_setglobal(L,"task");
    lua_pushlightuserdata(L,this);lua_pushcclosurek(L,legacy_wait_api,"wait",1,legacy_wait_continue);lua_setglobal(L,"wait");
    lua_pushcfunction(L,elapsed_time_api,"elapsedTime");lua_setglobal(L,"elapsedTime");
    lua_pushlightuserdata(L,this);lua_pushcclosure(L,time_api,"time",1);lua_setglobal(L,"time");
    for(int delayed=0;delayed<2;++delayed) {
        const char* name=delayed?"delay":"spawn";
        lua_pushlightuserdata(L,this);lua_pushboolean(L,delayed);
        lua_pushcclosure(L,legacy_schedule_api,name,2);lua_setglobal(L,name);
    }
    lua_getglobal(L,"pcall");lua_setglobal(L,"ypcall");
    for(const auto& alias:{std::pair{"Wait","wait"},std::pair{"Spawn","spawn"},std::pair{"Delay","delay"},std::pair{"ElapsedTime","elapsedTime"}}) {
        lua_getglobal(L,alias.second);lua_setglobal(L,alias.first);
    }
    // Static name tables: a range-for over `cond ? initializer_list : initializer_list`
    // read the lists' arrays after they were destroyed, and in practice
    // overwrote the global `wait` with setthreadidentity.
    static const char* const identity_names[2][3]={
        {"getthreadidentity","getidentity","getthreadcontext"},
        {"setthreadidentity","setidentity","setthreadcontext"}};
    for(int setter=0;setter<2;++setter) {
        lua_pushboolean(L,setter);lua_pushcclosure(L,identity_api,identity_names[setter][0],1);
        mark_executor_closure(L,-1);
        for(const char* name:identity_names[setter]) {
            lua_pushvalue(L,-1);lua_setglobal(L,name);
        }
        lua_pop(L,1);
    }
}
int SourceScheduler::elapsed_time_api(lua_State* L){lua_pushnumber(L,engine_elapsed());return 1;}
int SourceScheduler::time_api(lua_State* L) {
    auto& self=*static_cast<SourceScheduler*>(lua_touserdata(L,lua_upvalueindex(1)));
    lua_pushnumber(L,std::chrono::duration<double>(Clock::now()-self.started_).count());return 1;
}
int SourceScheduler::legacy_schedule_api(lua_State* L) {
    auto& self=*static_cast<SourceScheduler*>(lua_touserdata(L,lua_upvalueindex(1)));
    const bool delayed=lua_toboolean(L,lua_upvalueindex(2));
    const double duration=delayed?luaL_checknumber(L,1):0;
    const int function_index=delayed?2:1;
    luaL_checktype(L,function_index,LUA_TFUNCTION);
    if(!std::isfinite(duration)||duration>86400)return luaL_error(L,"invalid legacy delay duration"),0;
    try {
        lua_State* child=lua_newthread(L);
        lua_pushvalue(L,function_index);lua_xmove(L,child,1);
        auto& task=self.track(child);self.set_owner(task,self.owner(L));
        task.started=Clock::now();task.due=task.started+
            std::chrono::duration_cast<Clock::duration>(std::chrono::duration<double>(std::max(.03,duration)));
        task.state=Sleeping;task.elapsed=true;task.legacy_callback=true;task.sleeping_frame=self.frame_;
        lua_pop(L,1);return 0;
    }catch(const std::exception& error){luaL_error(L,"legacy scheduling failed: %s",error.what());return 0;}
}
void SourceScheduler::wake_timer(Task& task,Clock::time_point now) {
    if(task.elapsed) {
        lua_pushnumber(task.thread,std::chrono::duration<double>(now-task.started).count());task.arguments=1;
        if(task.legacy_callback){lua_pushnumber(task.thread,engine_elapsed());task.arguments=2;}
        task.elapsed=false;task.legacy_callback=false;
    }
    task.state=Ready;
}
int SourceScheduler::legacy_wait_continue(lua_State* L,int) {
    if(lua_gettop(L)!=1||lua_type(L,1)!=LUA_TNUMBER){luaL_error(L,"wait resumed without elapsed time");return 0;}
    lua_pushnumber(L,engine_elapsed());return 2;
}
int SourceScheduler::legacy_wait_api(lua_State* L) {
    auto& self=*static_cast<SourceScheduler*>(lua_touserdata(L,lua_upvalueindex(1)));
    double duration=luaL_optnumber(L,1,.03);
    if(!std::isfinite(duration)||duration>86400){luaL_error(L,"invalid wait duration");return 0;}
    if(!lua_isyieldable(L)){luaL_error(L,"wait requires a yieldable thread");return 0;}
    try {
        auto& task=self.track(L);task.started=Clock::now();
        task.due=task.started+std::chrono::duration_cast<Clock::duration>(std::chrono::duration<double>(std::max(.029,duration)));
        task.state=Sleeping;task.elapsed=true;task.sleeping_frame=self.frame_;
    }catch(const std::exception& e){luaL_error(L,"wait failed: %s",e.what());return 0;}
    lua_settop(L,0);return lua_yield(L,0);
}
int SourceScheduler::task_api(lua_State* L) {
    try{return task_api_inner(L);}catch(const std::exception& e){luaL_error(L,"task operation failed: %s",e.what());return 0;}
}
int SourceScheduler::task_api_inner(lua_State* L) {
    auto& self=*static_cast<SourceScheduler*>(lua_touserdata(L,lua_upvalueindex(1)));
    int operation=lua_tointeger(L,lua_upvalueindex(2));
    if(operation==3) {
        double duration=luaL_optnumber(L,1,0);
        if(!std::isfinite(duration)||duration>86400){luaL_error(L,"invalid task.wait duration");return 0;}
        if(!lua_isyieldable(L)){luaL_error(L,"task.wait requires a yieldable thread");return 0;}
        auto& task=self.track(L);task.started=Clock::now();
        task.due=task.started+std::chrono::duration_cast<Clock::duration>(std::chrono::duration<double>(std::max(0.001,duration)));
        task.state=Sleeping;task.elapsed=true;task.sleeping_frame=self.frame_;return lua_yield(L,0);
    }
    if(operation==4) {
        luaL_checktype(L,1,LUA_TTHREAD);auto* target=lua_tothread(L,1);
        const int state=lua_costatus(L,target);
        auto found=self.tasks_.find(target);
        if(target==L||state==LUA_CORUN||state==LUA_CONOR||
           (found!=self.tasks_.end()&&found->second.state==Running)) {
            luaL_error(L,"cannot cancel a running thread");return 0;
        }
        if(found!=self.tasks_.end()&&found->second.state!=Done&&found->second.state!=Cancelled) {
            found->second.state=Cancelled;
            found->second.response_ready=false;
            found->second.response_priority_cpu=Clock::duration{};
        }
        lua_resetthread(target);return 0;
    }
    int function_index=operation==2?2:1;
    double duration=operation==2?luaL_checknumber(L,1):0;
    if(!std::isfinite(duration)||duration>86400){luaL_error(L,"invalid task.delay duration");return 0;}
    int type=lua_type(L,function_index);
    if(type!=LUA_TFUNCTION&&type!=LUA_TTHREAD){luaL_error(L,"task expects a function or thread");return 0;}
    int count=lua_gettop(L)-function_index;
    lua_State* child=nullptr;
    if(type==LUA_TFUNCTION) {
        child=lua_newthread(L);
        lua_pushvalue(L,function_index);lua_xmove(L,child,1);
    } else {
        child=lua_tothread(L,function_index);lua_pushvalue(L,function_index);
        auto existing=self.tasks_.find(child);
        if(existing!=self.tasks_.end() && existing->second.state!=Waiting) {luaL_error(L,"thread cannot be resumed in its current state");return 0;}
        if(lua_status(child)==LUA_OK&&lua_gettop(child)==0){luaL_error(L,"cannot schedule a dead thread");return 0;}
    }
    for(int i=0;i<count;i++)lua_pushvalue(L,function_index+1+i);
    lua_xmove(L,child,count);
    auto& task=self.track(child);task.arguments=count;task.elapsed=false;self.set_owner(task,self.owner(child));
    if(operation==2) {
        task.state=Sleeping;task.sleeping_frame=self.frame_;task.due=Clock::now()+std::chrono::duration_cast<Clock::duration>(std::chrono::duration<double>(std::max(0.0,duration)));
    } else {
        task.state=Ready;
        if(operation==0)self.resume(task,L);else self.enqueue(task);
    }
    return 1; // The new/existing thread was retained on this caller's stack.
}
lua_State* SourceScheduler::callback_now(lua_State* from,int reference,const std::function<int(lua_State*)>& arguments,const std::string& owner_name,int callback_identity) {
    lua_State* child=lua_newthread(from);auto& task=track(child);
    lua_getref(child,reference);task.arguments=arguments(child);
    set_owner(task,owner_name);if(callback_identity>=0)set_identity(child,callback_identity);
    task.state=Ready;resume(task,from);return child;
}
lua_State* SourceScheduler::callback(lua_State* from,int reference,const std::function<int(lua_State*)>& arguments,std::function<bool()> guard,const std::string& owner_name,int callback_identity) {
    lua_State* child=lua_newthread(from);auto& task=track(child);lua_pop(from,1);
    lua_getref(child,reference);task.arguments=arguments(child);task.state=Ready;task.guard=std::move(guard);set_owner(task,owner_name);
    if(callback_identity>=0)set_identity(child,callback_identity);
    enqueue(task);return child;
}
void SourceScheduler::wait_signal(lua_State* thread) {
    auto& task=track(thread);task.state=Waiting;task.elapsed=false;
}
lua_State* SourceScheduler::invoke_callback(lua_State* from,int reference,const std::function<int(lua_State*)>& arguments,const std::string& owner_name,int callback_identity) {
    lua_State* child=lua_newthread(from);auto& task=track(child);lua_pop(from,1);
    try {
        lua_getref(child,reference);task.arguments=arguments(child);
        set_owner(task,owner_name);if(callback_identity>=0)set_identity(child,callback_identity);
        task.retained=true;task.report_errors=false;task.state=Ready;
        enqueue(task);return child;
    }catch(...) {task.state=Cancelled;task.response_ready=false;task.retained=false;collect_finished();throw;}
}
void SourceScheduler::release_invocation(lua_State* thread) {
    auto it=tasks_.find(thread);if(it==tasks_.end())return;
    it->second.retained=false;it->second.report_errors=true;collect_finished();
}
void SourceScheduler::wake_signal(lua_State* thread,const std::function<int(lua_State*)>& arguments,bool response_ready) {
    auto found=tasks_.find(thread);
    if(found==tasks_.end()||found->second.state!=Waiting)return;
    auto& task=found->second;task.arguments=arguments(thread);task.state=Ready;
    task.response_ready=response_ready;
    task.response_priority_cpu=Clock::duration{};
    enqueue(task);
}
void SourceScheduler::start(lua_State* root,int arguments) {
    auto& task=track(root);task.root=true;task.arguments=arguments;task.state=Ready;resume(task,main_);
}
std::string SourceScheduler::owner(lua_State* thread) const {
    auto* context=static_cast<ThreadOwner*>(lua_getthreaddata(thread));
    if(context&&context->scheduler==this)return context->owner;
    auto found=tasks_.find(thread);return found==tasks_.end()?"":found->second.owner;
}
lua_State* SourceScheduler::spawn_owned(lua_State* from,int index,const std::string& owner_name) {
    index=lua_absindex(from,index);lua_State* child=lua_newthread(from);
    lua_pushvalue(from,index);lua_xmove(from,child,1);
    auto& task=track(child);set_owner(task,owner_name);task.state=Ready;
    resume(task,from);return child;
}
void SourceScheduler::cancel_owner(const std::string& owner_name) {
    for(auto& [thread,task]:tasks_)if(task.owner==owner_name&&task.state!=Running) {
        task.state=Cancelled;task.response_ready=false;task.response_priority_cpu=Clock::duration{};lua_resetthread(thread);
    }
}
int SourceScheduler::completion(lua_State* root) const {
    auto task=tasks_.find(root);
    if(task==tasks_.end())return LUA_ERRRUN;
    if(task->second.state==Cancelled)return LUA_ERRRUN;
    return task->second.state==Done?task->second.result:LUA_YIELD;
}
size_t SourceScheduler::pending() const {
    size_t count=0;for(const auto& [thread,task]:tasks_)if(task.state!=Done&&task.state!=Cancelled)++count;return count;
}
void SourceScheduler::release_root(lua_State* root) {
    auto task=tasks_.find(root);if(task!=tasks_.end())task->second.root=false;
    collect_finished();
}
void SourceScheduler::collect_finished() {
    // A cancelled task may still have an entry in ready_. Retain just that
    // task until its final queue entry is consumed, rather than the whole pool.
    for(auto it=tasks_.begin();it!=tasks_.end();) {
        auto& task=it->second;
        // coroutine.close (or an external resume) can finish a VM thread while
        // its scheduler entry is asleep or awaiting a signal. Such a job must
        // not remain pending forever or be reported as observed success.
        if(task.state==Sleeping||task.state==Waiting) {
            int state=lua_costatus(main_,task.thread);
            if(state==LUA_COFIN||state==LUA_COERR) {
                task.state=Done;task.result=LUA_ERRRUN;
                if(state==LUA_COFIN) {
                    lua_settop(task.thread,0);
                    lua_pushliteral(task.thread,"managed task ended outside the scheduler (closed or externally resumed)");
                }
            }
        }
        if(!task.root&&!task.retained&&task.queued==0&&(task.state==Done||task.state==Cancelled)){lua_unref(main_,task.reference);it=tasks_.erase(it);}
        else ++it;
    }
}
void SourceScheduler::enqueue(Task& task) {++task.queued;ready_.push_back(&task);}
SourceScheduler::Task* SourceScheduler::dequeue() {
    auto selected=ready_.begin();
    auto response=std::find_if(ready_.begin(),ready_.end(),[](const Task* task){
        return task->state==Ready&&task->response_ready;
    });
    auto ordinary=std::find_if(ready_.begin(),ready_.end(),[](const Task* task){
        return task->state!=Ready||!task->response_ready;
    });
    // A delivered RPC continuation keeps priority across CPU slices, but only
    // for a bounded CPU budget. Alternate ordinary turns while it is elevated.
    const bool use_response=response!=ready_.end()&&(prefer_response_||ordinary==ready_.end());
    if(use_response)selected=response;
    else if(ordinary!=ready_.end())selected=ordinary;
    if(!use_response&&persistent_&&large_local_test()) {
        // Reserve alternating turns for interactive authored work in the
        // dedicated test profile. A full callback batch must not dilute an
        // authored job to one quantum per 128 callbacks. Each lane retains
        // FIFO order and unused turns are available to the other lane.
        auto preferred=std::find_if(ready_.begin(),ready_.end(),[&](const Task* task) {
            return task->state==Ready&&!task->response_ready&&(task->root||task->retained)==prefer_authored_;
        });
        if(preferred!=ready_.end())selected=preferred;
    }
    auto* task=*selected;ready_.erase(selected);--task->queued;
    prefer_response_=!use_response;
    prefer_authored_=!(task->root||task->retained);
    return task;
}
size_t SourceScheduler::callback_capacity() {
    collect_finished();
    // Leave task slots for foreground jobs and keep a small runnable batch.
    constexpr size_t callback_limit=2048-128,ready_limit=128;
    if(tasks_.size()>=callback_limit||ready_.size()>=ready_limit)return 0;
    return std::min(callback_limit-tasks_.size(),ready_limit-ready_.size());
}
size_t SourceScheduler::invocation_capacity() {
    collect_finished();
    // Request/response continuations need a slot even when the ordinary
    // event batch is full. Retain the same overall task and authoring reserve.
    constexpr size_t limit=2048-128;
    return tasks_.size()<limit?limit-tasks_.size():0;
}
void SourceScheduler::step(Clock::time_point deadline,std::function<void()> pump,size_t max_resumes,
                           std::function<void(Clock::time_point)> set_interrupt_deadline,
                           std::function<void()> progress_invocations,
                           std::function<bool()> should_yield) {
    if(!frame_open_)begin_frame();
    frame_open_=false;
    size_t resumed=0;
    const auto began=Clock::now();
    const auto pump_after=began+(deadline-began)/2;
    bool pumped=false;
    auto pump_once=[&](){if(!pumped&&pump){pump();pumped=true;}};
    auto output_ready=[&](){return should_yield&&should_yield();};
    auto wake_due=[&](Clock::time_point now) {
        std::vector<Task*> due;
        for(auto& [thread,task]:tasks_)if(task.state==Sleeping&&task.due<=now&&task.sleeping_frame<frame_) {
            wake_timer(task,now);due.push_back(&task);
        }
        // Preserve due-time order without overtaking work that is already
        // ready. A short repeating timer placed at the front on every poll
        // can otherwise run just before an expensive world pump, permanently
        // starving the same ready job behind it.
        std::stable_sort(due.begin(),due.end(),[](const Task* a,const Task* b){return a->due<b->due;});
        for(auto* task:due)enqueue(*task);
    };
    while(resumed<max_resumes) {
        // A host can hand control back as soon as this step has produced
        // externally consumable work. The ready queue remains intact, so the
        // ordinary scheduler resumes it on the next step.
        if(output_ready())break;
        auto now=Clock::now();
        wake_due(now);
        // Run already-due timers and queued work before pumping the world.
        // Event dispatch can be expensive; doing it first on every short poll
        // can otherwise consume every slice before timers ever become ready.
        if(ready_.empty()) {
            if(now>=deadline)break;
            pump_once();
            if(output_ready())break;
            now=Clock::now();
            wake_due(now);
        }
        // Materializing deferred event arguments or the pump can consume the
        // remaining step budget. Keep ready tasks queued for the next request.
        now=Clock::now();
        if(ready_.empty()||now>=deadline||now+std::chrono::microseconds(250)>=deadline)break;
        auto* task=dequeue();
        // The host's interrupt callback observes this per-task quantum. Normal
        // foreground jobs keep their existing request budget; background tasks
        // and the explicit large-test profile support resumable time slices.
        if(set_interrupt_deadline) {
            const bool resumable=!task->root||large_local_test();
            // Large authored computations otherwise receive only a few ms
            // between costly world updates. Keep ordinary callbacks at 2 ms
            // and cap the authored turn at 8 ms inside the same request budget.
            const auto quantum=std::chrono::milliseconds(task->root&&large_local_test()?8:2);
            set_interrupt_deadline(resumable?std::min(deadline,Clock::now()+quantum):deadline);
        }
        try{resume(*task,main_);}catch(...) {
            if(set_interrupt_deadline)set_interrupt_deadline(deadline);
            throw;
        }
        if(set_interrupt_deadline)set_interrupt_deadline(deadline);
        ++resumed;
        if(output_ready())break;
        // A function return can make its waiting caller runnable immediately.
        // Settle those calls between task turns without advancing simulation,
        // render signals, or the timer frame. The same deadline and resume
        // limit still bound a chain of nested/sequential invocations.
        if(progress_invocations&&Clock::now()<deadline)progress_invocations();
        if(output_ready())break;
        // Give a small ready batch a turn before potentially expensive world
        // work. Pump once within the first half-slice or eight resumes.
        if(Clock::now()<deadline&&(resumed>=8||Clock::now()>=pump_after||ready_.empty())) {
            pump_once();
            if(output_ready())break;
        }
    }
    collect_finished();
}
int SourceScheduler::run(lua_State* root,int arguments,std::function<void()> pump) {
    auto& root_task=track(root);root_task.root=true;root_task.arguments=arguments;
    resume(root_task,main_);
    while(Clock::now()<deadline_) {
        ++frame_;
        if(pump)pump();
        auto now=Clock::now();
        if(now>=deadline_)break;
        bool pending=false;
        for(auto& [thread,task]:tasks_) {
            if(task.state==Sleeping && task.due<=now && task.sleeping_frame<frame_) {
                wake_timer(task,now);enqueue(task);
            }
            // Background game scripts do not extend the lifetime of a completed
            // one-shot authored job. Its own tasks/callbacks still drain fully.
            if(task.state!=Done&&task.state!=Cancelled&&(task.owner.empty()||root_task.state!=Done))pending=true;
        }
        if(root_task.state==Cancelled){lua_pushliteral(root,"root source task was cancelled");return LUA_ERRRUN;}
        if(!pending)return root_status_;
        if(!ready_.empty()) {
            auto* next=dequeue();resume(*next,main_);
        } else std::this_thread::sleep_for(std::chrono::milliseconds(1));
    }
    return LUA_BREAK;
}
