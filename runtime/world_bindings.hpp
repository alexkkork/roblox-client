#pragma once

#include <lua.h>
#include "json.hpp"
#include "script_thread_integrity.hpp"
#include "protected_script_load.hpp"
#include "animation_state.hpp"
#include "tween_state.hpp"
#include "client_service_state.hpp"
#include "render_step_state.hpp"
#include "input_state.hpp"
#include "context_action_state.hpp"

#include <string>
#include <unordered_map>
#include <vector>
#include <memory>
#include <deque>
#include <unordered_set>
#include <chrono>

class SourceScheduler;

namespace world_bindings {
using Json = nlohmann::json;

struct Node {
    std::string id;
    std::string class_name;
    std::string name;
    std::string parent;
    Json properties = Json::object();
    Json origins = Json::object();
    Json attributes = Json::object();
    bool attributes_known = false;
    std::vector<std::string> collection_tags;
    bool collection_tags_known = false;
    bool destroyed = false;
    bool server_owned = true;
    bool constructor_defaults_known = false;
    Json revisions = Json::object();
    Json script_code;
    std::string script_origin;
    std::string image_load_token;
    bool image_loaded=false;
};

struct Signal {
    struct World* world=nullptr;
    Node* node=nullptr;
    std::string key;
};
struct Connection {
    Signal* signal=nullptr;
    int callback=LUA_NOREF;
    int creator_ref=LUA_NOREF;
    bool connected=true;
    bool once=false;
    bool once_pending=false;
    bool auto_disconnected=false;
    bool manually_disconnected=false;
    std::string owner;
    int identity=8;
};
struct PendingEvent {
    Connection* connection=nullptr;
    lua_State* waiter=nullptr;
    Json arguments=Json::array();
    int argument_ref=LUA_NOREF;
    int argument_count=0;
    int waiter_ref=LUA_NOREF;
};
struct SignalWaiter {lua_State* thread;int reference;};
struct ChildWaiter {
    lua_State* thread;int reference;std::string parent;std::string name;
    std::chrono::steady_clock::time_point deadline;
};
struct HandleWaiter {
    lua_State* thread;int reference;std::string id;
    std::chrono::steady_clock::time_point deadline;
};
struct ChatWaiter {
    lua_State* thread;int reference;uint64_t request;
    std::string channel;
    std::chrono::steady_clock::time_point deadline;
    bool universe=false;
};
struct TextServiceWaiter {
    lua_State* thread=nullptr;
    int reference=LUA_NOREF;
    uint64_t request=0;
    std::string key,kind;
    std::chrono::steady_clock::time_point deadline;
};
struct BindableCallback {int reference=LUA_NOREF;std::string owner;int identity=8;};
struct BindableInvocation {
    lua_State* caller=nullptr;
    lua_State* callee=nullptr;
    int caller_ref=LUA_NOREF,arguments_ref=LUA_NOREF,callback_ref=LUA_NOREF,count=0;
    std::string node,owner;
    int identity=8;
    bool cancelled=false;
};
struct RemoteClientInvocation {
    int identity=8;
    std::string node,owner,error;
    int32_t request=0;
    Json arguments=Json::array();
    lua_State* callee=nullptr;
    int callback_ref=LUA_NOREF;
    bool cancelled=false;
};
struct RemoteServerWaiter {
    lua_State* thread=nullptr;
    int reference=LUA_NOREF;
    uint64_t request=0;
    std::string node;
};

struct World {
    std::vector<TextServiceWaiter> text_service_waiters;
    Json text_service_requests=Json::array();
    uint64_t next_text_service_request=1;
    std::unordered_map<std::string,Json> text_service_cache;
    size_t text_service_cache_bytes=0;
    // Optional local diagnostics, bounded independently of script activity.
    // Values are never retained and observations do not imply a current wait.
    bool trace_attribute_reads = false;
    uint64_t attribute_read_sequence = 0;
    std::deque<std::pair<lua_State*, Json>> attribute_reads;
    std::string motion_topology_root;
    std::unordered_set<std::string> motion_topology_parts;
    ClientServiceState client_services;
    Json session_stats = Json::object();
    std::string stats_ping_item;
    RenderStepState render_steps;
    AnimationState animation;
    std::unordered_map<std::string,TweenPlaybackState> tweens;
    std::chrono::steady_clock::time_point heartbeat_previous{};
    double heartbeat_delta=1.0/60.0;
    double simulation_time=0.0; // RunService.Stepped `time` argument
    std::chrono::steady_clock::time_point render_previous{};
    double render_cap=60;
    std::unordered_map<std::string,std::unordered_map<std::string,bool>> scriptability;
    Json input_capabilities = Json::object();
    InputState input;
    ContextActionState context_actions;
    Json display = {{"size",{0,0}},{"device_insets",{0,0,0,0}},{"core_insets",{0,0,0,0}}};
    std::unordered_map<std::string,Json> display_watches;
    std::unordered_set<std::string> active_scroll_layouts;
    // Stable list-layout tie breaking tracks insertion into a parent, not
    // global creation order or unordered-map iteration order.
    uint64_t next_gui_parent_order=1;
    std::unordered_map<std::string,uint64_t> gui_parent_order;
    Json viewport = Json::object();
    std::unordered_map<std::string, Json> camera_viewports;
    std::unordered_map<std::string, Json> pivot_caches;
    std::unordered_map<std::string, Json> model_scale_caches;
    std::unordered_map<std::string, Json> attachment_watches;
    std::unordered_map<std::string, Json> joint_active_watches;
    std::unordered_map<std::string, Json> assembly_root_watches;
    std::unordered_map<std::string, Json> part_mass_watches;
    std::unordered_set<std::string> pending_motor_transforms;
    std::unordered_map<std::string, Json> motor_applied_transforms;
    std::unordered_map<std::string, std::pair<std::string,std::string>> active_pose_joints;
    bool joint_topology_dirty = false;
    Json joint_pose_errors = Json::array();
    std::unique_ptr<ScriptThreadIntegrity> script_thread_integrity=std::make_unique<ScriptThreadIntegrity>();
    std::unique_ptr<ProtectedScriptStore> protected_scripts=std::make_unique<ProtectedScriptStore>();
    int load_script(lua_State* state,const char* name,const std::string& code,const std::string& source="") {
        script_thread_integrity->check_load_thread();
        auto payload=protected_scripts->make(source,code);
        return load_protected_script(state,name,payload);
    }
    Json script_thread_report() const {
        const auto observed=script_thread_integrity->snapshot();
        return Json{{"version",1},{"checks",observed.checks},{"failures",observed.failures}};
    }
    Json protected_script_report() const {
        const auto observed=protected_scripts->snapshot();
        return Json{{"version",1},{"constructions",observed.constructions},
                    {"copies",observed.copies},{"failures",observed.failures}};
    }
    std::unordered_map<std::string, Node> nodes;
    std::vector<std::string> order;
    std::unordered_map<std::string, std::string> services;
    std::string local_player;
    std::string job_id;
    std::string game_id;
    Json class_parents = Json::object();
    // Class metadata is finalized by parse_snapshot and never changed by
    // world deltas. Cache each class's closure within this World so repeated
    // joint/transform queries do not allocate and walk the same graph.
    mutable std::unordered_map<std::string, std::unordered_set<std::string>> class_ancestors;
    Json defaults = Json::object();
    Json default_origins = Json::object();
    double sequence = 0;
    std::unordered_map<std::string, Json> local_properties;
    std::unordered_set<std::string> pending_jump_requests;
    std::unordered_set<std::string> locally_dead_humanoids;
    Json mutations = Json::array();
    size_t mutation_bytes = 0;
    std::string output;
    bool output_truncated = false;
    uint64_t next_local_id = 1;
    SourceScheduler* scheduler=nullptr;
    std::unordered_map<std::string,std::unique_ptr<Signal>> signals;
    std::vector<std::unique_ptr<Connection>> connections;
    std::string gui_pointer_target;
    std::string gui_pointer_input;
    std::string focused_textbox;
    std::string overlay_root;
    uint64_t text_focus_revision=0;
    uint64_t text_editor_revision=0;
    int text_cursor=-1;
    int text_selection=-1;
    std::unordered_map<Signal*,std::vector<SignalWaiter>> waiters;
    std::deque<PendingEvent> events;
    bool event_overflow=false;
    bool hierarchy_events_observed=false;
    bool collection_events_observed=false;
    bool player_membership_events_observed=false;
    std::vector<int> event_argument_refs;
    Json class_events = Json::object();
    Json class_event_signatures = Json::object();
    Json class_property_types = Json::object();
    std::unordered_set<std::string> locally_destroyed;
    size_t server_actions = 0;
    size_t server_events = 0;
    size_t unresolved_server_events = 0;
    std::unordered_map<std::string,std::string> pending_characters;
    bool persistent=false;
    bool initial_replication_complete=false;
    std::vector<ChildWaiter> child_waiters;
    std::vector<HandleWaiter> handle_waiters;
    bool chat_enabled=false;
    uint64_t next_chat_request=1;
    Json chat_requests=Json::array();
    std::vector<ChatWaiter> chat_waiters;
    size_t chat_messages=0;
    std::unordered_map<std::string,BindableCallback> bindable_callbacks;
    std::vector<BindableInvocation> bindable_invocations;
    std::vector<RemoteClientInvocation> remote_client_invocations;
    Json remote_client_replies=Json::array();
    Json remote_server_events=Json::array();
    size_t remote_server_event_bytes=0;
    uint64_t next_remote_server_request=1;
    std::vector<RemoteServerWaiter> remote_server_waiters;
    Json remote_server_calls=Json::array();
    size_t remote_server_call_bytes=0;
    // Opt-in local-only caller trace for InvokeServer. Never part of wire calls.
    bool trace_remote_calls=false;
    Json remote_call_diagnostics=Json::array();
};

// Parses a bounded version-1 server snapshot. Missing properties stay unknown.
World parse_snapshot(const std::string& json_text);

// Value-only source datatype transport; no network or engine execution.
int push_translation_cframe(lua_State* state, double x, double y, double z);
bool is_tween_info(lua_State* state, int index);
bool capture_tween_info(lua_State* state, int index, Json& value);
bool push_tween_info_json(lua_State* state, const Json& value);
bool capture_geometry_enum_item(lua_State* state, int index, Json& value);
bool push_geometry_enum_item(lua_State* state, const char* type, int value);
bool read_geometry_enum_item(lua_State* state, int index, const char* type, int& value);
void install_geometry_value_enums(lua_State* state);

// Installs a local read/write simulation over the snapshot in a fresh Luau VM.
// Mutations are retained in World::mutations and are never sent to a server.
void install(lua_State* L, World& world);
// Apply controller feedback into the local script view after a simulation step.
// This overlay is separate from received snapshot properties and never emits mutations.
void update_physics_feedback(lua_State* L, World& world, const Json& feedback);
Json capture_value(lua_State* L, World& world, int index);
void dispatch_events(lua_State* from,World& world);
// Settle function callbacks between scheduler turns without advancing a frame.
void progress_invocations(lua_State* from,World& world);
void update_snapshot(lua_State* from,World& world,const std::string& text);
// Apply a validated local engine pose to the local Character overlay only.
// This never changes received node properties, replication sequence, or mutations.
Json apply_engine_poses(World& world,const Json& poses);
Json apply_engine_owned_parts(World& world,const Json& updates);
void complete_chat(lua_State* from,World& world,const Json& responses);
void complete_text_service(lua_State* from,World& world,const Json& responses);
void complete_remote_server(lua_State* from,World& world,const Json& responses);
void push_instance(lua_State* L,World& world,const std::string& id);
std::string instance_display_name(World& world,const std::string& id);
Json script_nodes(World& world);
// Read-only bounded projection for local browser/native display surfaces.
Json render_snapshot(World& world);
void render_gui_action(World& world,const Json& action);
void stop_script_tasks(lua_State* from,World& world,const std::string& owner);
void push_player_scripts(lua_State* L,World& world);
int clone_client_template(lua_State* L);
}
