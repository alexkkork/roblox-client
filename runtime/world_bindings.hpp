#pragma once

#include <lua.h>
#include "json.hpp"
#include "shared_json.hpp"
#include "rust_attribute_store.hpp"
#include "rust_world_view.hpp"
#include "script_thread_integrity.hpp"
#include "protected_script_load.hpp"
#include "animation_state.hpp"
#include "tween_state.hpp"
#include "client_service_state.hpp"
#include "render_step_state.hpp"
#include "input_state.hpp"
#include "client_idle_state.hpp"
#include "context_action_state.hpp"
#include "sound_state.hpp"

#include <string>
#include <string_view>
#include <cstddef>
#include <unordered_map>
#include <vector>
#include <memory>
#include <deque>
#include <unordered_set>
#include <chrono>
#include <cstdint>
#include <functional>
#include <mutex>

class SourceScheduler;
class ActorRuntime;
namespace shared_table_bindings { class SharedTablePool; }

namespace world_bindings {
using Json = nlohmann::json;
struct World;
struct HierarchyEventIndex;

// Lazily parsed, immutable constructor metadata. The class object remains cached
// for process lifetime so returned descriptor pointers stay valid across calls.
const Json* constructor_class(std::string_view class_name);
std::size_t constructor_class_cache_size();
void apply_constructor_class_parents(Json& class_parents);

// A read-only view of one validated network delta. The view borrows the
// already-parsed JSON object and can be shared by the root world and Actor
// worlds without serializing or reparsing it for each consumer.
class ValidatedDeltaView {
public:
    explicit ValidatedDeltaView(const Json& delta, const void* shared_world_view=nullptr);
    ValidatedDeltaView(Json&&) = delete;
    const Json& value() const noexcept { return *delta_; }
    // Borrowed only for the current native request. It must never be retained
    // by World or a Node; each compact node binds its own Rust record handle.
    const void* shared_world_view() const noexcept { return shared_world_view_; }
    const Json& received_part_pose_fields(const Json& action_fields,
                                          const Json& frame) const;

private:
    const Json* delta_;
    const void* shared_world_view_;
    // Keys borrow delta_->actions[*].fields; values live only for this command.
    // Root and Actor updates use the view sequentially on the host thread.
    mutable std::unordered_map<const Json*, Json> received_part_pose_fields_;
};

// Script metadata is absent on most replicated instances. Keep its common
// empty state inline as one pointer; copies remain value-like, while reads do
// not allocate. Mutating a copied Node gets an independent ScriptOnlyState.
struct ScriptOnlyState {
    SharedJson code;
    std::string origin;
    uint64_t disabled_generation = 0;
};

class LazyScriptState {
    std::unique_ptr<ScriptOnlyState> state_;
    static std::unique_ptr<ScriptOnlyState> clone(
            const std::unique_ptr<ScriptOnlyState>& source) {
        return source ? std::make_unique<ScriptOnlyState>(*source)
                      : std::unique_ptr<ScriptOnlyState>{};
    }
    static const ScriptOnlyState& empty_state() {
        static const ScriptOnlyState empty;
        return empty;
    }
    ScriptOnlyState& ensure() {
        if (!state_) state_ = std::make_unique<ScriptOnlyState>();
        return *state_;
    }
public:
    LazyScriptState() = default;
    LazyScriptState(const LazyScriptState& other)
        : state_(clone(other.state_)) {}
    LazyScriptState& operator=(const LazyScriptState& other) {
        if (this == &other) return *this;
        state_ = clone(other.state_);
        return *this;
    }
    LazyScriptState(LazyScriptState&&) noexcept = default;
    LazyScriptState& operator=(LazyScriptState&&) noexcept = default;

    const SharedJson& code() const { return state_ ? state_->code : empty_state().code; }
    SharedJson& code_mut() { return ensure().code; }
    const std::string& origin() const { return state_ ? state_->origin : empty_state().origin; }
    void set_origin(std::string value) {
        if (!state_ && value.empty()) return;
        ensure().origin = std::move(value);
    }
    uint64_t disabled_generation() const {
        return state_ ? state_->disabled_generation : 0;
    }
    void bump_disabled_generation() { ++ensure().disabled_generation; }
    void set_disabled_generation(uint64_t value) {
        if (!state_ && value == 0) return;
        ensure().disabled_generation = value;
    }
    bool allocated() const noexcept { return bool(state_); }
};
static_assert(sizeof(LazyScriptState) == sizeof(void*),
              "lazy script state wrapper must remain pointer-sized");

struct Node {
    std::string id;
    std::string class_name;
    std::string name;
    std::string parent;
    SharedJson properties = SharedJson::object();
    SharedJson origins = SharedJson::object();
    // A per-node Arc<NodeRecord>, never a retained whole-world view.
    std::shared_ptr<RustWorldNodeView> rust_world_node;
    RustAttributeStore attributes;
    std::vector<std::string> collection_tags;
    bool collection_tags_known = false;
    bool destroyed = false;
    // Set only after a local Destroy/ClearAllChildren boundary has performed
    // the one-time synchronous script-owner cleanup. Replicated removals keep
    // this false and continue through normal lifecycle reconciliation.
    bool script_destroy_cleanup_complete = false;
    bool server_owned = true;
    bool constructor_defaults_known = false;
    SharedJson revisions = SharedJson::object();
    LazyScriptState script_state;
    std::string image_load_token;
    bool image_loaded=false;
};

// Conservative C++ prefilter for Actor VM allocation. This does not decide
// script eligibility or source readiness; it only avoids a VM when there is
// no descendant script class that can run in a client Actor context.
std::unordered_set<std::string> actors_that_may_own_client_work(World& world);

struct Signal {
    struct World* world=nullptr;
    Node* node=nullptr;
    std::string key;
};
struct Connection : std::enable_shared_from_this<Connection> {
    enum class NativeKind { None, ClientIdle };
    Signal* signal=nullptr;
    int callback=LUA_NOREF;
    int creator_ref=LUA_NOREF;
    // Set for yielding Wait records so their creator registry ref can be
    // released when the wait completes and the record is retired.
    lua_State* creator_thread=nullptr;
    bool connected=true;
    bool enabled=true;
    bool once=false;
    bool once_pending=false;
    bool auto_disconnected=false;
    bool manually_disconnected=false;
    // A yielding RBXScriptSignal:Wait is represented in connection
    // introspection while its thread is blocked. It has no callback closure.
    bool signal_waiter=false;
    std::string owner;
    int identity=8;
    uint64_t id=0;
    // IDs restart in each World; generation prevents stale Actor proxies from
    // resolving against a replacement VM with the same Actor ID.
    uint64_t generation=1;
    NativeKind native_kind=NativeKind::None;
};
struct PendingEvent {
    Connection* connection=nullptr;
    lua_State* waiter=nullptr;
    Json arguments=Json::array();
    int argument_ref=LUA_NOREF;
    int argument_count=0;
    int waiter_ref=LUA_NOREF;
    bool response_ready=false;
    bool engine_idle_event=false;
};
struct SignalWaiter {lua_State* thread;int reference;Connection* connection=nullptr;};
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
struct BindableCallback {int reference=LUA_NOREF;std::string owner;int identity=8;int creator_ref=LUA_NOREF;};
struct BindableInvocation {
    lua_State* caller=nullptr;
    lua_State* callee=nullptr;
    int caller_ref=LUA_NOREF,arguments_ref=LUA_NOREF,callback_ref=LUA_NOREF,count=0,creator_ref=LUA_NOREF;
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
    int creator_ref=LUA_NOREF;
    bool cancelled=false;
};
struct RemoteServerWaiter {
    lua_State* thread=nullptr;
    int reference=LUA_NOREF;
    uint64_t request=0;
    std::string node;
};

// Opt-in, main-thread-only Sound load diagnostics. The worker only reports
// AudioAsset::Info through its future; World owns all trace timestamps.
struct AudioLoadTraceState {
    std::string asset_uri_sha256;
    std::string bytes_sha256;
    size_t byte_count=0;
    std::chrono::steady_clock::time_point assigned_at{};
    std::chrono::steady_clock::time_point request_at{};
    std::chrono::steady_clock::time_point decoded_at{};
    bool assigned=false;
    bool request_started=false;
    bool decoded=false;
    bool failed=false;
    bool first_property_observed=false;
};

// Session-scoped snapshots for explicitly registered local workspace assets.
// Each VM retains its own file table; only paths passed to getcustomasset are
// published. File writes refresh the snapshot and deletes invalidate it.
class SessionCustomAssetRegistry {
public:
    bool register_asset(const std::string& scope,const std::string& path,
                        const std::string& uri,const char* bytes,size_t size);
    void update_file(const std::string& scope,const std::string& path,
                     const char* bytes,size_t size,bool present);
    std::shared_ptr<const std::string> resolve(const std::string& uri) const;
private:
    mutable std::mutex mutex_;
    std::unordered_map<std::string,std::string> assets_; // URI -> scoped path key
    std::unordered_map<std::string,std::shared_ptr<const std::string>> files_;
    size_t retained_bytes_=0;
};

struct World {
    bool lightweight_assets = false;
    SharedJsonPool received_origin_pool;
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
    ClientIdleState client_idle;
    bool client_idle_control_enabled=false;
    bool client_idle_disconnect_enabled=true;
    bool client_idle_event_due=false;
    bool client_idle_event_observed=false;
    bool client_idle_disconnect_requested=false;
    uint64_t client_idle_event_count=0;
    std::string client_idle_player_id;
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
    // Derived Humanoid.RootPart observations for subscribed Humanoids.
    std::unordered_map<std::string, Json> humanoid_root_part_watches;
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
    // Keeps immutable JSON snapshot records alive while Node SharedJson fields
    // alias them. Node facades and all runtime identity remain World-local.
    std::shared_ptr<const Json> snapshot_owner;
    // Local clone birth hashes distinguish a retried Clone subtree from an
    // unrelated node that reused one of its IDs.
    std::unordered_map<std::string, std::string> local_clone_birth_hashes;
    std::vector<std::string> order;
    std::unordered_map<std::string, std::string> services;
    std::string local_player;
    std::string job_id;
    std::string game_id;
    Json class_parents = Json::object();
    // Owns the Rust index when the Rust host has installed its engine API.
    // World moves/copies share immutable metadata and the synchronized cache.
    mutable std::shared_ptr<void> rust_class_graph;
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
    std::unordered_map<std::string,AudioLoadTraceState> audio_load_trace;
    size_t audio_load_trace_record_count=0;
    bool audio_load_trace_limit_reported=false;
    std::shared_ptr<SessionCustomAssetRegistry> custom_asset_registry=
        std::make_shared<SessionCustomAssetRegistry>();
    size_t script_bytecode_trace_count = 0;
    bool script_bytecode_trace_limit_reported = false;
    uint64_t next_local_id = 1;
    std::string local_id_prefix="local:";
    SourceScheduler* scheduler=nullptr;
    // Installed by ScriptExecution after its bootstrap succeeds. Called only
    // on an effective enabled-to-disabled transition for a live script node.
    std::function<void(lua_State*,const std::string&)> script_disabled_transition;
    const void* script_disabled_transition_owner=nullptr;
    std::function<void(lua_State*,const std::string&)> script_destroy_transition;
    const void* script_destroy_transition_owner=nullptr;
    ActorRuntime* actor_runtime=nullptr;
    lua_State* actor_environment=nullptr;
    std::shared_ptr<shared_table_bindings::SharedTablePool> shared_table_pool;
    std::string actor_id;
    std::string allocate_local_id() {
        for(;;) {
            std::string id=local_id_prefix+std::to_string(next_local_id++);
            if(nodes.find(id)==nodes.end())return id;
        }
    }
    std::unordered_map<std::string,std::unique_ptr<Signal>> signals;
    // A signal is created only by signal_for(). Keep its instance ID indexed
    // so unobserved replicated properties need no serialized signal key.
    std::unordered_set<std::string> signal_nodes;
    std::vector<std::shared_ptr<Connection>> connections;
    uint64_t next_connection_id=1;
    // Set once when an Actor VM is created and copied to its connections.
    uint64_t connection_generation=1;
    std::unordered_map<std::string,SoundPlayback> sounds;
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
    // Borrowed only while applying one ordered world update; never retained
    // across VM callbacks or copied into an Actor snapshot.
    HierarchyEventIndex* hierarchy_event_index=nullptr;
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
    Json client_idle_report() const {
        if(!client_idle_control_enabled)return nullptr;
        auto clock=client_idle;
        const auto now=std::chrono::duration_cast<std::chrono::milliseconds>(
            std::chrono::steady_clock::now().time_since_epoch()).count();
        return Json{{"version",1},
                    {"player_id",client_idle_player_id},
                    {"event_due",client_idle_event_observed},
                    {"disconnect_requested",client_idle_disconnect_requested},
                    {"disconnect_enabled",client_idle_disconnect_enabled},
                    {"elapsed_inactive_ms",clock.elapsed_ms(now)},
                    {"native_notifications",client_idle_event_count}};
    }
};

// Parses a bounded version-1 server snapshot. Missing properties stay unknown.
World parse_snapshot(const std::string& json_text);
World parse_snapshot(const Json& snapshot);
World parse_snapshot(const Json& snapshot, const void* shared_world_view);
World parse_snapshot(std::shared_ptr<const Json> snapshot);

// Value-only source datatype transport; no network or engine execution.
int push_translation_cframe(lua_State* state, double x, double y, double z);
bool is_tween_info(lua_State* state, int index);
bool capture_tween_info(lua_State* state, int index, Json& value);
bool push_tween_info_json(lua_State* state, const Json& value);
bool capture_geometry_enum_item(lua_State* state, int index, Json& value);
bool push_geometry_enum_item(lua_State* state, const char* type, int value);
bool read_geometry_enum_item(lua_State* state, int index, const char* type, int& value);
void install_geometry_value_enums(lua_State* state);
void install_rotation_curve_key(lua_State* state);
bool is_rotation_curve_key(lua_State* state,int index);
bool capture_rotation_curve_key(lua_State* state,int index,Json& value);
bool push_rotation_curve_key(lua_State* state,const Json& value);
void install_content_values(lua_State* state);
bool is_content_value(lua_State* state,int index);
bool capture_content_value(lua_State* state,int index,Json& value);
bool push_content_value(lua_State* state,const Json& value,World* world=nullptr);

// Installs a local read/write simulation over the snapshot in a fresh Luau VM.
// Mutations are retained in World::mutations and are never sent to a server.
void install(lua_State* L, World& world);
// Apply controller feedback into the local script view after a simulation step.
// This overlay is separate from received snapshot properties and never emits mutations.
void update_physics_feedback(lua_State* L, World& world, const Json& feedback);
Json capture_value(lua_State* L, World& world, int index);
void push_serialized_value(lua_State* L, World& world, const Json& value);
// Actor messages cross VM boundaries as bounded tagged data; delivery is
// queued by ActorRuntime and emits through the destination World's local signal.
bool enqueue_actor_message(World& world,const std::string& actor_id,
                           const std::string& topic,const Json& arguments);
bool actor_message_index(lua_State* L,World& world,Node& actor,const std::string& key);
Json apply_actor_mutations(lua_State* L,World& world,const std::string& actor_id,
                           const Json& actions);
Json apply_local_mutations(lua_State* L,World& world,const Json& actions);
void dispatch_events(lua_State* from,World& world);
// Settle function callbacks between scheduler turns without advancing a frame.
void progress_invocations(lua_State* from,World& world);
void update_snapshot(lua_State* from,World& world,const std::string& text,
                     bool defer_camera_refresh=false,
                     Json* deferred_animation_responses=nullptr);
void update_snapshot(lua_State* from,World& world,const ValidatedDeltaView& delta,
                     bool defer_camera_refresh=false,
                     Json* deferred_animation_responses=nullptr);
void complete_animation_responses(World& world,const Json& responses);
void refresh_camera(World& world);
Json snapshot_tweens(const World& world);
void restore_tweens(World& world,const Json& snapshot);
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
// Remove signal Wait registrations whose coroutine completed or was cancelled.
// Passing a signal limits pruning to that signal; null prunes the whole World.
void prune_signal_waiters(World& world,const Signal* signal=nullptr);
// Read-only bounded projection for local browser/native display surfaces.
Json render_snapshot(World& world);
void render_gui_action(World& world,const Json& action);
bool stop_script_tasks(lua_State* from,World& world,const std::string& owner,lua_State* deferred_main=nullptr,
                       bool lifecycle_disable=false);
void push_player_scripts(lua_State* L,World& world);
int clone_client_template(lua_State* L);
}
