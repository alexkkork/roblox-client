#include "platform_support.hpp"
#include "world_bindings.hpp"
#include "actor_runtime.hpp"
#include "shared_table_bindings.hpp"
#include "legacy_value_bindings.hpp"
#include <Luau/Compiler.h>
#include "sha256.hpp"
#include <class_registry_data.hpp>
#include "source_scheduler.hpp"
#include "source_bindings.hpp"
#include "authored_bindings.hpp"
#include "movement_bindings.hpp"
#include "json_bindings.hpp"
#include "vector_math_compat.hpp"
#include "vector2_bindings.hpp"
#include "audio_asset.hpp"
#include "spatial_values.hpp"
#include "rust_engine_bridge.hpp"
#include "received_collision_group.hpp"

#include <lualib.h>

#include <algorithm>
#include <array>
#include <atomic>
#include <cassert>
#include <cstring>
#include <climits>
#include <cstdlib>
#include <cmath>
#include <memory>
#include <mutex>
#include <new>
#include <optional>
#include <limits>
#include <deque>
#include <fstream>
#include <iomanip>
#include <sstream>
#include <set>
#include <stdexcept>
#include <string_view>
#include <unordered_set>

namespace world_bindings {
struct HierarchyEventIndex {
    struct Link { std::string parent; size_t order; };
    std::unordered_map<Node*,Link> links;
    std::unordered_map<std::string,std::unordered_set<Node*>> children;
    size_t indexed_nodes=0;
};
namespace {
constexpr const char* kInstanceMeta = "WorldSnapshot.Instance";
constexpr const char* kCFrameMeta = "WorldSnapshot.CFrame";
constexpr const char* kSettingsId = "__world_snapshot_settings__";
constexpr const char* kSignalMeta = "WorldSnapshot.Signal";
constexpr const char* kConnectionMeta = "WorldSnapshot.Connection";
constexpr const char* kInstanceHandleMeta = "WorldSnapshot.InstanceHandle";
constexpr const char* kChatStatusMeta = "WorldSnapshot.TextChatMessageStatus.Item";
constexpr const char* kChatEnumMeta = "WorldSnapshot.TextChatMessageStatus.Enum";
constexpr const char* kChatStatusNames[] = {"Unknown","Success","Sending","TextFilterFailed","Floodchecked","InvalidPrivacySettings","InvalidTextChannelPermissions","MessageTooLong","ModerationTimeout"};
constexpr size_t kMaxNodes = 100000;
constexpr size_t kMaxMutations = 10000;
constexpr size_t kMaxMutationBytes = 128 * 1024;
constexpr size_t kMaxPrinted = 128 * 1024;
static bool omitted_lightweight_asset(std::string_view key) {
#define ROVE_ASSET_PROPERTY(name) if(key==name)return true;
#include "lightweight_assets.inc"
#undef ROVE_ASSET_PROPERTY
    return false;
}
constexpr size_t kMaxMutationValue = 96 * 1024;
constexpr size_t kMaxRegisteredAssetBytes = 100 * 1024 * 1024;
constexpr size_t kMaxRegisteredAssets = 512;
constexpr int kMaxJsonDepth = 32;

std::string workspace_asset_uri(const std::string& scope,const std::string& path) {
    static constexpr char hex[]="0123456789ABCDEF";
    auto encode=[&](const std::string& value) {
        std::string out;out.reserve(value.size());
        for(unsigned char c:value) {
            const bool safe=(c>='A'&&c<='Z')||(c>='a'&&c<='z')||(c>='0'&&c<='9')
                ||c=='_'||c=='-'||c=='.'||c=='/'||c=='~';
            if(safe)out.push_back(char(c));
            else {out.push_back('%');out.push_back(hex[c>>4]);out.push_back(hex[c&15]);}
        }
        return out;
    };
    std::string uri="rbxasset://workspace/";
    if(!scope.empty())uri+="actor/"+encode(scope)+"/";
    uri+=encode(path);return uri;
}
int register_workspace_asset(lua_State* L) {
    auto* world=static_cast<World*>(lua_touserdata(L,lua_upvalueindex(1)));
    const char* path=luaL_checkstring(L,1);
    const char* uri=luaL_checkstring(L,2);
    size_t size=0;const char* bytes=luaL_checklstring(L,3,&size);
    if(!world||!world->custom_asset_registry)return luaL_error(L,"workspace asset registry unavailable"),0;
    bool valid=false,allocation_failed=false;
    try {
        valid=workspace_asset_uri(world->actor_id,path)==uri
            &&world->custom_asset_registry->register_asset(world->actor_id,path,uri,bytes,size);
    } catch(...) {allocation_failed=true;}
    if(allocation_failed)return luaL_error(L,"workspace asset registry limit or allocation failure"),0;
    if(!valid)return luaL_error(L,"workspace asset registry capacity or URI collision"),0;
    return 0;
}
int update_workspace_file(lua_State* L) {
    auto* world=static_cast<World*>(lua_touserdata(L,lua_upvalueindex(1)));
    const char* path=luaL_checkstring(L,1);
    if(!world||!world->custom_asset_registry)return luaL_error(L,"workspace asset registry unavailable"),0;
    size_t size=0;const char* bytes=nullptr;const bool present=!lua_isnoneornil(L,2);
    if(present)bytes=luaL_checklstring(L,2,&size);
    bool failed=false;
    try {world->custom_asset_registry->update_file(world->actor_id,path,bytes,size,present);}
    catch(...) {failed=true;}
    if(failed)return luaL_error(L,"workspace asset registry update failed"),0;
    return 0;
}
int resolve_workspace_asset(lua_State* L) {
    auto* world=static_cast<World*>(lua_touserdata(L,lua_upvalueindex(1)));
    const char* uri=luaL_checkstring(L,1);
    if(!world||!world->custom_asset_registry){lua_pushnil(L);return 1;}
    try {
        auto bytes=world->custom_asset_registry->resolve(uri);
        if(bytes)lua_pushlstring(L,bytes->data(),bytes->size());else lua_pushnil(L);
    } catch(...) {lua_pushnil(L);}
    return 1;
}
int workspace_asset_uri_binding(lua_State* L) {
    auto* world=static_cast<World*>(lua_touserdata(L,lua_upvalueindex(1)));
    const char* path=luaL_checkstring(L,1);
    if(!world){lua_pushnil(L);return 1;}
    try {
        const auto uri=workspace_asset_uri(world->actor_id,path);
        lua_pushlstring(L,uri.data(),uri.size());
    } catch(...) {lua_pushnil(L);}
    return 1;
}
void install_workspace_asset_registry(lua_State* L,World& w) {
    auto install=[&](const char* name,lua_CFunction function) {
        lua_pushlightuserdata(L,&w);
        lua_pushcclosure(L,function,name,1);
        lua_setfield(L,-2,name);
    };
    lua_newtable(L);
    install("register",register_workspace_asset);
    install("update",update_workspace_file);
    install("resolve",resolve_workspace_asset);
    install("uri",workspace_asset_uri_binding);
    lua_setfield(L,LUA_REGISTRYINDEX,"Headless.WorkspaceAssetBridge");
}
#include "input_capabilities.inc"

const ConstructorClassIndexEntry* constructor_index_entry(std::string_view name) {
    const auto begin=std::begin(kConstructorClassIndex);
    const auto end=std::end(kConstructorClassIndex);
    const auto found=std::lower_bound(begin,end,name,[](const ConstructorClassIndexEntry& entry,std::string_view key) {
        return std::string_view(entry.name)<key;
    });
    return found!=end&&std::string_view(found->name)==name?found:nullptr;
}
struct ConstructorCacheEntry {
    std::once_flag parsed_once;
    std::unique_ptr<Json> parsed;
};
std::array<ConstructorCacheEntry,kConstructorRegistryClassCount>& constructor_cache() {
    static std::array<ConstructorCacheEntry,kConstructorRegistryClassCount> cache;
    return cache;
}
std::atomic<std::size_t>& constructor_cache_count() {
    static std::atomic<std::size_t> count{0};
    return count;
}
const Json* cached_constructor_class(std::string_view name) {
    const auto* entry=constructor_index_entry(name);
    if(!entry)return nullptr;
    const auto index=static_cast<std::size_t>(entry-kConstructorClassIndex);
    auto& cached=constructor_cache()[index];
    std::call_once(cached.parsed_once,[&] {
        const char* first=reinterpret_cast<const char*>(kConstructorRegistrySource+entry->offset);
        const char* last=first+entry->length;
        auto parsed=std::make_unique<Json>(Json::parse(first,last));
        if(!parsed->is_object())throw std::runtime_error("invalid indexed constructor class");
        cached.parsed=std::move(parsed);
        constructor_cache_count().fetch_add(1,std::memory_order_relaxed);
    });
    return cached.parsed.get();
}
size_t constructor_cache_size_internal() {
    return constructor_cache_count().load(std::memory_order_relaxed);
}
void apply_constructor_class_parents_internal(Json& class_parents) {
    for(const auto& entry:kConstructorParentIndex)
        class_parents[entry.name]=entry.is_null?Json(nullptr):Json(entry.parent);
}
#include "client_settings.inc"
#include "player_defaults.inc"

const Json* property_descriptor(const Node& node, const std::string& key) {
    if(const auto* setting=client_setting_descriptor(node,key))return setting;
    if(const auto* field=player_constructor_property(node,key))return field;
    if(node.class_name=="Players"&&key=="LocalPlayer") {
        static const Json player{{"type","Instance"},{"writable",false}};return &player;
    }
    if(node.class_name=="Camera"&&key=="CameraType") {
        static const Json mode{{"type","Enum"},{"enum_type","CameraType"},{"writable",true}};return &mode;
    }
    if(node.class_name=="GuiService"&&key=="TouchControlsEnabled") {
        static const Json enabled{{"type","boolean"},{"writable",true},{"value",true}};return &enabled;
    }
    if(node.class_name=="ImageHandleAdornment"&&key=="Size") {
        // Public writable Vector2 in the pinned API. Its constructor value
        // was not captured, so a read stays unavailable until assigned.
        static const Json size{{"type","Vector2"},{"writable",true}};return &size;
    }
    if(node.class_name=="BillboardGui") {
        static const Json size{{"type","UDim2"},{"writable",true}};
        static const Json offset{{"type","Vector2"},{"writable",true}};
        if(key=="Size")return &size;
        if(key=="SizeOffset")return &offset;
    }
    if((node.class_name=="Part"||node.class_name=="WedgePart"||node.class_name=="FormFactorPart")&&key=="formFactorRaw") {
        // The 0.745 FormFactorPart constructor at 0x1010b3620 stores 1 at
        // +0x1a8, the byte read by the reflected formFactorRaw accessor.
        static const Json field{{"type","Enum"},{"enum_type","FormFactor"},{"writable",true},{"value",1}};return &field;
    }
    if((node.class_name=="Script"||node.class_name=="LocalScript"||node.class_name=="ModuleScript")&&key=="isPlayerScript") {
        static const Json field{{"type","boolean"},{"writable",true}};return &field;
    }
    if(node.class_name=="Script"&&key=="RunContext") {
        // BaseScript.RunContext is exposed by the client for Script objects.
        // Script's documented constructor default is Legacy (0).
        static const Json script{{"type","Enum"},{"enum_type","RunContext"},{"writable",true},
            {"value",Json{{"$enum",{{"type","RunContext"},{"value",0},{"name","Legacy"}}}}}};
        return &script;
    }
    if(node.class_name=="Player"&&key=="GameplayPaused") {
        static const Json descriptor{{"type","boolean"},{"writable",true}};return &descriptor;
    }
    if(node.class_name=="SpecialMesh"&&key=="MeshType") {
        static const Json enumeration{{"type","Enum"},{"writable",true}};return &enumeration;
    }
    if((node.class_name=="Mouse"||node.class_name=="PlayerMouse"||node.class_name=="PluginMouse")&&key=="TargetFilter") {
        static const Json filter{{"type","Instance"},{"writable",true},{"value",nullptr}};return &filter;
    }
    if((node.class_name=="Mouse"||node.class_name=="PlayerMouse"||node.class_name=="PluginMouse")&&key=="Icon") {
        static const Json icon{{"type","string"},{"engine_type","ContentId"},{"writable",true},{"value",""}};return &icon;
    }
    if(node.class_name=="GetTextBoundsParams"&&key=="Font") {
        static const Json font{{"type","Font"},{"writable",true}};return &font;
    }
    if(node.class_name=="TextLabel"||node.class_name=="TextButton"||node.class_name=="TextBox") {
        static const Json font{{"type","Font"},{"writable",true}},enumeration{{"type","Enum"},{"writable",true}};
        static const Json content{{"type","string"},{"writable",false}};
        if(key=="FontFace")return &font;
        if(key=="Font")return &enumeration;
        if(key=="ContentText")return &content;
        if(key=="TextStrokeColor3") {
            static const Json stroke{{"type","Color3"},{"writable",true},{"value",Json{{"$color3",{0,0,0}}}}};
            return &stroke;
        }
        if(key=="TextTruncate") {
            static const Json truncate{{"type","Enum"},{"writable",true},
                {"value",Json{{"$enum",{{"type","TextTruncate"},{"value",0},{"name","None"}}}}}};
            return &truncate;
        }
        if(node.class_name=="TextBox"&&key=="PlaceholderColor3") {
            static const Json placeholder{{"type","Color3"},{"writable",true},{"value",Json{{"$color3",{.7,.7,.7}}}}};
            return &placeholder;
        }
    }
    if(node.class_name=="UIStroke") {
        if(key=="ApplyStrokeMode") {
            static const Json mode{{"type","Enum"},{"writable",true},
                {"value",Json{{"$enum",{{"type","ApplyStrokeMode"},{"value",0},{"name","Contextual"}}}}}};
            return &mode;
        }
        if(key=="StrokeSizingMode") {
            static const Json mode{{"type","Enum"},{"writable",true},
                {"value",Json{{"$enum",{{"type","StrokeSizingMode"},{"value",0},{"name","FixedSize"}}}}}};
            return &mode;
        }
        if(key=="BorderStrokePosition") {
            static const Json position{{"type","Enum"},{"writable",true},
                {"value",Json{{"$enum",{{"type","BorderStrokePosition"},{"value",1},{"name","Center"}}}}}};
            return &position;
        }
        if(key=="LineJoinMode") {
            static const Json join{{"type","Enum"},{"writable",true},
                {"value",Json{{"$enum",{{"type","LineJoinMode"},{"value",0},{"name","Round"}}}}}};
            return &join;
        }
        if(key=="BorderOffset") {
            static const Json offset{{"type","UDim"},{"writable",true},{"value",Json{{"$udim",{0,0}}}}};
            return &offset;
        }
    }
    if(node.class_name=="UIGradient") {
        static const Json white{{"$color3",{1,1,1}}};
        static const Json color{{"type","ColorSequence"},{"writable",true},
            {"value",Json{{"$color_sequence",Json::array({Json{{"$color_keypoint",{0,white}}},Json{{"$color_keypoint",{1,white}}}})}}}};
        static const Json transparency{{"type","NumberSequence"},{"writable",true},
            {"value",Json{{"$number_sequence",Json::array({Json{{"$number_keypoint",{0,0,0}}},Json{{"$number_keypoint",{1,0,0}}}})}}}};
        static const Json offset{{"type","Vector2"},{"writable",true},{"value",Json{{"$vector2",{0,0}}}}};
        static const Json tile{{"type","Enum"},{"writable",true},
            {"value",Json{{"$enum",{{"type","GradientTileMode"},{"value",0},{"name","Clamp"}}}}}};
        static const Json shape{{"type","Enum"},{"writable",true},
            {"value",Json{{"$enum",{{"type","GradientType"},{"value",0},{"name","Linear"}}}}}};
        if(key=="Color")return &color;
        if(key=="Transparency")return &transparency;
        if(key=="Offset")return &offset;
        if(key=="TileMode")return &tile;
        if(key=="Type")return &shape;
    }
    if(node.class_name=="ImageLabel"||node.class_name=="ImageButton") {
        static const Json loaded{{"type","boolean"},{"writable",false},{"value",false}};
        static const Json color{{"type","Color3"},{"writable",true},{"value",Json{{"$color3",{1,1,1}}}}};
        static const Json point{{"type","Vector2"},{"writable",true},{"value",Json{{"$vector2",{0,0}}}}};
        static const Json axis{{"$udim",{1,0}}};
        static const Json tile{{"type","UDim2"},{"writable",true},{"value",Json{{"$udim2",{axis,axis}}}}};
        static const Json scale{{"type","Enum"},{"writable",true},
            {"value",Json{{"$enum",{{"type","ScaleType"},{"value",0},{"name","Stretch"}}}}}};
        static const Json sampling{{"type","Enum"},{"writable",true},
            {"value",Json{{"$enum",{{"type","ResamplerMode"},{"value",0},{"name","Default"}}}}}};
        if(key=="ImageColor3")return &color;
        if(key=="ImageRectOffset"||key=="ImageRectSize")return &point;
        if(key=="TileSize")return &tile;
        if(key=="ScaleType")return &scale;
        if(key=="ResampleMode")return &sampling;
        if(key=="IsLoaded")return &loaded;
    }
    // These compound values were omitted by the constructor capture. Declare
    // their public writable types without inventing initial property values.
    if(node.class_name=="ArcHandles"&&key=="Axes") {
        static const Json axes{{"type","Axes"},{"writable",true}};return &axes;
    }
    if(node.class_name=="Handles"&&key=="Faces") {
        static const Json faces{{"type","Faces"},{"writable",true}};return &faces;
    }
    if(node.class_name=="InputObject"&&(key=="KeyCode"||key=="UserInputType"||key=="UserInputState"||key=="Position"||key=="Delta")) {
        static const Json enumeration{{"type","Enum"},{"writable",true}},vector{{"type","Vector3"},{"writable",true}};
        return key=="Position"||key=="Delta"?&vector:&enumeration;
    }
    if(node.class_name=="UserInputService") {
        static const Json enumeration{{"type","Enum"},{"writable",true}},boolean{{"type","boolean"},{"writable",true}};
        if(key=="MouseBehavior")return &enumeration;
        if(key=="MouseIconEnabled")return &boolean;
        if(key=="MouseDeltaSensitivity"){static const Json number{{"type","number"},{"writable",true}};return &number;}
    }

    if(node.class_name=="VectorForce"&&key=="RelativeTo") {
        static const Json relative{{"type","Enum"},{"writable",true}};return &relative;
    }
    if(node.class_name=="LinearVelocity") {
        static const Json enumeration{{"type","Enum"},{"writable",true}},vector{{"type","Vector2"},{"writable",true}};
        if(key=="RelativeTo"||key=="VelocityConstraintMode"||key=="ForceLimitMode")return &enumeration;
        if(key=="PlaneVelocity"||key=="MaxPlanarAxesForce")return &vector;
    }
    // The constructor capture omits sequence-valued properties. The pinned
    // API descriptor identifies these writable sequence/range properties;
    // describing their type does not invent an unreceived initial value.
    if(node.class_name=="Trail"||node.class_name=="ParticleEmitter") {
        static const Json color{{"type","ColorSequence"},{"writable",true}};
        static const Json sequence{{"type","NumberSequence"},{"writable",true}};
        static const Json range{{"type","NumberRange"},{"writable",true}};
        static const Json vector{{"type","Vector2"},{"writable",true}};
        if(key=="Color")return &color;
        if(key=="Transparency"||(node.class_name=="Trail"&&key=="WidthScale")||
           (node.class_name=="ParticleEmitter"&&(key=="Size"||key=="Squash")))return &sequence;
        if(node.class_name=="ParticleEmitter") {
            if(key=="FlipbookFramerate"||key=="Lifetime"||key=="RotSpeed"||key=="Rotation"||key=="Speed")return &range;
            if(key=="SpreadAngle")return &vector;
        }
    }
    const Json* cls=cached_constructor_class(node.class_name);
    if(!cls)return nullptr;
    const auto& properties = cls->at("properties");
    auto property = properties.find(key);
    return property == properties.end() ? nullptr : &*property;
}

// The wire schema calls BasePart.Color "Color3uint8" (type 20). The
// snapshot encoder has already converted its bytes into a tagged Color3.
// Do not substitute a constructor color when neither value was received.
const Json* received_color_alias(const Node& node,const Json& fields) {
    const Json* descriptor=property_descriptor(node,"Color");
    auto value=fields.find("Color3uint8");
    if(!descriptor||descriptor->value("type","")!="Color3"||value==fields.end())return nullptr;
    if(!value->is_object()||!value->contains("$color3")||!(*value)["$color3"].is_array()||(*value)["$color3"].size()!=3)
        throw std::runtime_error("received Color3uint8 must be a tagged Color3");
    for(const auto& c:(*value)["$color3"])if(!c.is_number()||!std::isfinite(c.get<double>())||c.get<double>()<0||c.get<double>()>1)
        throw std::runtime_error("received Color3uint8 component is out of range");
    return &*value;
}

bool valid_utf8(const char* s,size_t n);
std::string bytes_hex(const char* data,size_t size);

// Native wire getter 10108232C prepends the group-ID byte to the public name.
// The snapshot preserves non-UTF-8 wire strings as $bytes_hex. Decode the wire
// representation before projecting the received name into the Luau property.
Json received_collision_group_alias(const Node& node,const Json& fields) {
    const Json* descriptor=property_descriptor(node,"CollisionGroup");
    auto value=fields.find("CollisionGroupReplicate");
    if(!descriptor||descriptor->value("type","")!="string"||value==fields.end())return nullptr;
    return received_collision_group_value(*value);
}

const Json* humanoid_health_default(const Node& node,const std::string& key) {
    // Replicated Humanoids also begin with constructor health state. Missing
    // snapshot fields do not mean that the engine property does not exist.
    // Keep this separate from received values and use the pinned registry.
    if(node.class_name!="Humanoid"||(key!="Health"&&key!="MaxHealth"))return nullptr;
    const Json* descriptor=property_descriptor(node,key);
    return descriptor?&descriptor->at("value"):nullptr;
}

struct Handle { World* world; Node* node; };
struct SignalHandle { Signal* signal; };
struct ConnectionHandle {
    Connection* connection=nullptr;
    // Local handles keep disconnected inspection records alive until Lua
    // collects them. Foreign and signal-Wait proxies resolve by ID instead.
    std::shared_ptr<Connection> ownership;
    ActorRuntime* actor_runtime=nullptr;
    World* waiter_world=nullptr;
    uint64_t connection_id=0;
    uint64_t generation=0;
    bool foreign_state=false;
    bool foreign_lua_connection=false;
    bool waiter_proxy=false;
    bool waiter_lua_connection=false;
    char owner_actor_id[257]{}; // empty identifies the root VM
};
void destroy_connection_handle(lua_State*,void* value) {
    static_cast<ConnectionHandle*>(value)->~ConnectionHandle();
}
ConnectionHandle* new_connection_handle(lua_State* L) {
    auto* handle=static_cast<ConnectionHandle*>(lua_newuserdatadtor(
        L,sizeof(ConnectionHandle),destroy_connection_handle));
    new(handle)ConnectionHandle{};
    luaL_getmetatable(L,kConnectionMeta);lua_setmetatable(L,-2);
    return handle;
}
size_t active_connection_count(const World& world) {
    return std::count_if(world.connections.begin(),world.connections.end(),[](const auto& connection) {
        // Wait records remain admitted until their queued wake is retired.
        // Native subscriptions retain their existing lifetime and admission.
        return connection->connected||connection->signal_waiter
            ||connection->native_kind!=Connection::NativeKind::None;
    });
}
void prune_disconnected_connections(lua_State* L,World& world) {
    std::unordered_set<const Connection*> queued;
    for(const auto& event:world.events)if(event.connection)queued.insert(event.connection);
    bool retired=false;
    auto end=std::remove_if(world.connections.begin(),world.connections.end(),[&](const auto& connection) {
        if(connection->connected||connection->signal_waiter
           ||connection->native_kind!=Connection::NativeKind::None
           ||connection.use_count()!=1||queued.count(connection.get()))return false;
        // No Lua handle, dispatch snapshot, queued event, or callback guard
        // refers to this record. Its Lua captures can now be released too.
        if(connection->callback!=LUA_NOREF)lua_unref(L,connection->callback);
        if(connection->creator_ref!=LUA_NOREF)lua_unref(L,connection->creator_ref);
        connection->callback=LUA_NOREF;connection->creator_ref=LUA_NOREF;
        retired=true;return true;
    });
    world.connections.erase(end,world.connections.end());
    // Captures may have been marked before their last native registry root
    // disappeared. Make bounded incremental progress rather than retaining
    // them until new allocations collide with the VM's memory cap.
    if(retired&&lua_gc(L,LUA_GCISRUNNING,0))lua_gc(L,LUA_GCSTEP,64);
}
Connection* resolve_wait_connection(ConnectionHandle& handle);
bool connection_compat_index(lua_State* L,Connection& connection,const char* key);
bool connection_handle_compat_index(lua_State* L,ConnectionHandle& handle,const char* key);
void enforce_scriptability(lua_State* L,World& w,Node& n,std::string& key);
std::string hidden_property_name(const Node& n,const std::string& key);
struct InstanceHandleValue {World* world;std::string id;};
void push_chat_status(lua_State* L,int value) {
    if(value<1||value>9)throw std::runtime_error("unsupported TextChatMessageStatus");
    auto* item=static_cast<int*>(lua_newuserdatatagged(L,sizeof(int),0));*item=value;
    luaL_getmetatable(L,kChatStatusMeta);lua_setmetatable(L,-2);
}
int chat_readonly(lua_State* L){luaL_error(L,"chat enum values are read-only");return 0;}
int chat_status_index(lua_State* L) {
    int value=*static_cast<int*>(luaL_checkudata(L,1,kChatStatusMeta));const char* key=luaL_checkstring(L,2);
    if(std::strcmp(key,"Name")==0)lua_pushstring(L,kChatStatusNames[value-1]);
    else if(std::strcmp(key,"Value")==0)lua_pushinteger(L,value);
    else if(std::strcmp(key,"EnumType")==0){lua_getfield(L,LUA_REGISTRYINDEX,"Headless.ChatStatusEnum");}
    else {luaL_error(L,"unknown TextChatMessageStatus member");return 0;}
    return 1;
}
int chat_status_string(lua_State* L){int v=*static_cast<int*>(luaL_checkudata(L,1,kChatStatusMeta));lua_pushfstring(L,"Enum.TextChatMessageStatus.%s",kChatStatusNames[v-1]);return 1;}
int chat_status_equal(lua_State* L){auto* a=static_cast<int*>(luaL_checkudata(L,1,kChatStatusMeta));auto* b=static_cast<int*>(luaL_checkudata(L,2,kChatStatusMeta));lua_pushboolean(L,*a==*b);return 1;}
int chat_enum_items(lua_State* L){luaL_checkudata(L,1,kChatEnumMeta);lua_createtable(L,9,0);for(int v=1;v<=9;v++){push_chat_status(L,v);lua_rawseti(L,-2,v);}return 1;}
int chat_enum_index(lua_State* L){const char* key=luaL_checkstring(L,2);if(std::strcmp(key,"GetEnumItems")==0){lua_pushcfunction(L,chat_enum_items,"GetEnumItems");return 1;}for(int v=1;v<=9;v++)if(std::strcmp(key,kChatStatusNames[v-1])==0){push_chat_status(L,v);return 1;}luaL_error(L,"unknown TextChatMessageStatus item");return 0;}
int chat_enum_string(lua_State* L){lua_pushliteral(L,"TextChatMessageStatus");return 1;}
void destroy_instance_handle(lua_State*,void* value){static_cast<InstanceHandleValue*>(value)->~InstanceHandleValue();}
void push_instance_handle(lua_State* L,World& world,const std::string& id) {
    auto* h=static_cast<InstanceHandleValue*>(lua_newuserdatadtor(L,sizeof(InstanceHandleValue),destroy_instance_handle));
    new(h)InstanceHandleValue{&world,id};luaL_getmetatable(L,kInstanceHandleMeta);lua_setmetatable(L,-2);
}
struct CFrameValue { std::array<double, 3> p{}; std::array<double, 9> r{1,0,0,0,1,0,0,0,1}; bool rotation_known=true; Json source=Json::object(); };
void destroy_cframe(lua_State*, void* value) { static_cast<CFrameValue*>(value)->~CFrameValue(); }
CFrameValue* new_cframe(lua_State* L) {
    auto* value = static_cast<CFrameValue*>(lua_newuserdatadtor(L, sizeof(CFrameValue), destroy_cframe));
    new(value) CFrameValue();
    return value;
}

Handle* checked_handle(lua_State* L, int i) {
    return static_cast<Handle*>(luaL_checkudata(L, i, kInstanceMeta));
}
bool has_meta(lua_State* L, int i, const char* name) {
    if (!lua_getmetatable(L, i)) return false;
    luaL_getmetatable(L, name);
    bool same = lua_rawequal(L, -1, -2) != 0;
    lua_pop(L, 2);
    return same;
}
#include "geometry_enums.inc"
#include "enum_root.inc"
#include "export_enums.inc"
#include "security_capabilities.inc"
#include "tween_bindings.inc"
#include "http_url_encode.inc"

void push_instance_cache(lua_State* L) {
    lua_rawgetfield(L,LUA_REGISTRYINDEX,"Headless.InstanceCache");
    if(lua_istable(L,-1))return;
    lua_pop(L,1);lua_newtable(L);
    // Canonicalization must not own the lifetime of a Lua wrapper. The World
    // retains the engine node; a script holding the userdata keeps its wrapper
    // alive, while a later lookup can recreate an otherwise collected wrapper.
    lua_newtable(L);lua_pushliteral(L,"v");lua_rawsetfield(L,-2,"__mode");
    lua_setmetatable(L,-2);lua_pushvalue(L,-1);
    lua_rawsetfield(L,LUA_REGISTRYINDEX,"Headless.InstanceCache");
}
void push_node(lua_State* L, World& world, Node* node) {
    push_instance_cache(L);const int cache=lua_gettop(L);
    lua_pushlightuserdata(L, node);
    lua_rawget(L, cache);
    if (lua_isuserdata(L, -1)){lua_remove(L,cache);return;}
    lua_pop(L, 1);
    auto* h = static_cast<Handle*>(lua_newuserdatatagged(L, sizeof(Handle), 0));
    h->world = &world; h->node = node;
    luaL_getmetatable(L, kInstanceMeta); lua_setmetatable(L, -2);
    lua_pushlightuserdata(L, node); lua_pushvalue(L, -2); lua_rawset(L, cache);lua_remove(L,cache);
}
Node* find_node(World& world, const std::string& id) {
    auto it = world.nodes.find(id);
    return it == world.nodes.end() ? nullptr : &it->second;
}
#include "content_values.inc"
const Json* local_value(World& world, const Node& node, const std::string& key) {
    auto it = world.local_properties.find(node.id);
    if (it == world.local_properties.end() || !it->second.is_object()) return nullptr;
    auto p = it->second.find(key);
    return p == it->second.end() ? nullptr : &*p;
}
void clear_local_property(World& world,const std::string& id,const std::string& key) {
    auto it=world.local_properties.find(id);
    if(it!=world.local_properties.end()&&it->second.is_object())it->second.erase(key);
}
std::string parent_id(World& world, const Node& node) {
    if (const Json* v = local_value(world, node, "Parent")) {
        if (v->is_object() && v->contains("$ref") && (*v)["$ref"].is_string()) return (*v)["$ref"].get<std::string>();
        return {};
    }
    return node.parent;
}
bool valid_utf8(const char* s,size_t n){
    size_t i=0;while(i<n){unsigned char c=s[i];if(c<0x80){i++;continue;}size_t count=0;uint32_t code=0;
        if((c&0xe0)==0xc0){count=1;code=c&0x1f;if(code<2)return false;}
        else if((c&0xf0)==0xe0){count=2;code=c&0x0f;}
        else if((c&0xf8)==0xf0){count=3;code=c&0x07;if(code>4)return false;}
        else return false;
        if(i+count>=n)return false;for(size_t j=1;j<=count;j++){unsigned char t=s[i+j];if((t&0xc0)!=0x80)return false;code=(code<<6)|(t&0x3f);}
        if((count==2&&code<0x800)||(count==3&&code<0x10000)||code>0x10ffff||(code>=0xd800&&code<=0xdfff))return false;i+=count+1;
    }return true;
}
std::string instance_name(World& world, const Node& node) {
    if (const Json* v = local_value(world, node, "Name"); v && v->is_string()) return v->get<std::string>();
    return node.name;
}
bool display_property(World& world,const Node& node,const std::string& key);
bool display_readonly_property(World& world,const Node& node,const std::string& key);
bool mouse_projection_property(World&,const Node&,const std::string&);
const Json* gui_default_property(World& world,const Node& node,const std::string& key);
std::string gui_property_type(World& world,const Node& node,const std::string& key);
std::string part_transform_origin(World&,const Node&,const std::string&);
bool part_transform_index(lua_State*,World&,Node&,const std::string&);
std::string attachment_origin(World&,const Node&,const std::string&);
bool attachment_index(lua_State*,World&,Node&,const std::string&);
void watch_attachment_property(World&,Node&,const std::string&);
std::string joint_origin(World&,const Node&,const std::string&);
bool joint_index(lua_State*,World&,Node&,const std::string&);
void watch_joint_property(World&,Node&,const std::string&);
std::string assembly_root_origin(World&,const Node&,const std::string&);
Node* humanoid_root_part(World&,Node&);
std::string humanoid_root_part_origin(World&,const Node&);
void watch_humanoid_root_part(World&,Node&,const std::string&);
bool sync_humanoid_root_part_watch(World&,Node&);
void refresh_humanoid_root_part_events(World&);
std::string part_mass_origin(World&,const Node&,const std::string&);
bool part_mass_index(lua_State*,World&,Node&,const std::string&);
const Json* part_mass_default(World&,const Node&,const std::string&);
Json observed_part_mass(World&,Node&,const std::string&);
void watch_part_mass(World&,Node&,const std::string&);
bool assembly_root_index(lua_State*,World&,Node&,const std::string&);
void watch_assembly_root(World&,Node&,const std::string&);
void invalidate_joint_topology(World&,Node&,const std::string&);
std::string text_font_origin(World&,const Node&,const std::string&);
bool text_layout_node(const Node&);
std::array<double,2> text_intrinsic_size(World&,const Node&,double);
bool text_wraps(World&,const Node&);
bool gui_image(const Node&);
std::optional<std::string> text_content(World&,const Node&);
const Json* completed_creation_default(World& world,const Node& node,const std::string& key) {
    if(!node.constructor_defaults_known)return nullptr;
    auto cls=world.class_property_types.find(node.class_name);
    const auto* descriptor=property_descriptor(node,key);
    // Writable local properties (for example CollisionGroup's public alias)
    // also start at their measured constructor value when the wire uses a
    // different property name. Do not extend this to computed read-only
    // values, whose value can depend on subsequently received geometry.
    const bool reflected=cls!=world.class_property_types.end()&&cls->contains(key);
    if(!reflected&&!(descriptor&&descriptor->value("writable",false)))return nullptr;
    return descriptor&&descriptor->contains("value")?&descriptor->at("value"):nullptr;
}
std::string origin(World& world, const Node& node, const std::string& key) {
    if(node.class_name=="Humanoid"&&key=="RootPart")return humanoid_root_part_origin(world,node);
    if(node.class_name=="Sound"&&(key=="TimePosition"||key=="TimeLength"||key=="IsLoaded"||key=="Playing"||key=="IsPlaying"||key=="IsPaused"))return "local_audio";
    if(!node.server_owned&&node.class_name=="UserGameSettings"&&client_setting_descriptor(node,key))return "local_settings";
    if(key=="IsLoaded"&&gui_image(node))return "local_display";
    if(key=="ContentText"&&text_layout_node(node))
        return text_content(world,node)?"derived_local":"unavailable";
    if(mouse_projection_property(world,node,key))
        return key=="X"||key=="Y"?"local_input":key=="ViewSizeX"||key=="ViewSizeY"?"local_display":"local_camera";
    if(key=="Source"&&(node.class_name=="LocalScript"||node.class_name=="ModuleScript"||node.class_name=="Script")) {
        if(node.script_state.code().is_object()&&node.script_state.code().contains("source"))return node.script_state.code().value("authored_override",false)?"local":"received";
        return "class_default";
    }
    if(auto derived=assembly_root_origin(world,node,key);!derived.empty())return derived;
    if(auto derived=part_mass_origin(world,node,key);!derived.empty())return derived;
    if(auto derived=joint_origin(world,node,key);!derived.empty())return derived;
    if(auto derived=attachment_origin(world,node,key);!derived.empty())return derived;
    if(auto derived=part_transform_origin(world,node,key);!derived.empty())return derived;
    if(auto derived=text_font_origin(world,node,key);!derived.empty())return derived;
    if (node.class_name == "UserInputService" && (input_capability_key(key)||key=="PreferredInput"||key=="MouseBehavior"||key=="MouseIconEnabled"||key=="MouseDeltaSensitivity")) return "local_input";
    if (node.class_name == "InputObject" && (key=="KeyCode"||key=="UserInputType"||key=="UserInputState"||key=="Position"||key=="Delta")) return "local_input";
    if (display_property(world,node,key)) return "local_display";
    if (node.class_name == "Camera" && key == "ViewportSize") return "local_viewport";
    if (local_value(world, node, key)) return "local";
    auto explicit_origin = node.origins.find(key);
    if(key=="Parent"&&!node.parent.empty()&&!find_node(world,node.parent))return "unresolved_reference";
    if(key=="Parent"&&node.properties.contains(key)) {
        const Json& parent=node.properties[key];
        if(parent.is_object()&&parent.size()==1&&parent.contains("$ref")&&parent["$ref"].is_string()&&!find_node(world,parent["$ref"].get<std::string>()))return "unresolved_reference";
    }
    if(explicit_origin!=node.origins.end()&&explicit_origin->is_string()) {
        const std::string tag=explicit_origin->get<std::string>();
        if(tag=="received"||tag=="derived_received"||tag=="issued_session"||tag=="local"||tag=="class_default")return tag;
    }
    if (node.properties.contains(key)) {
        const Json&v=node.properties[key];
        if(v.is_object()&&v.size()==1&&v.contains("$ref")&&v["$ref"].is_string()&&!find_node(world,v["$ref"].get<std::string>()))return "unresolved_reference";
        return "received";
    }
    if (key == "Name" || key == "ClassName" || key == "Parent") return "received";
    if(world.lightweight_assets&&omitted_lightweight_asset(key))return "unavailable";
    if(completed_creation_default(world,node,key))return "class_default";
    auto d = world.defaults.find(node.class_name);
    auto o = world.default_origins.find(node.class_name);
    if (d != world.defaults.end() && d->is_object() && d->contains(key) && o != world.default_origins.end()
        && o->is_object() && o->contains(key) && (*o)[key].is_string()) {
        const std::string tag=(*o)[key].get<std::string>();
        if(tag=="received"||tag=="derived_received"||tag=="issued_session"||tag=="local"||tag=="class_default")return tag;
    }
    if(humanoid_health_default(node,key))return "class_default";
    if(node.class_name=="Humanoid"&&key=="Jump")return "local_default";
    if(gui_default_property(world,node,key))return "class_default";
    if(part_mass_default(world,node,key))return "class_default";
    if(node.class_name=="GuiService"&&key=="TouchControlsEnabled")return "local_default";
    return "unavailable";
}
const Json* input_default(const Node&,const std::string&);
const Json* property_value(World& world, const Node& node, const std::string& key) {
    if(key=="IsLoaded"&&gui_image(node)) {
        static const Json loaded=true,unloaded=false;
        return node.image_loaded?&loaded:&unloaded;
    }
    if (const Json* v = local_value(world, node, key)) return v;
    auto p = node.properties.find(key);
    if (p != node.properties.end()) return &*p;
    if(world.lightweight_assets&&omitted_lightweight_asset(key))return nullptr;
    if(const auto* value=completed_creation_default(world,node,key))return value;
    auto d = world.defaults.find(node.class_name);
    auto o = world.default_origins.find(node.class_name);
    if (d != world.defaults.end() && d->is_object() && d->contains(key) && o != world.default_origins.end()
        && o->is_object() && o->contains(key) && (*o)[key].is_string() && !(*o)[key].get<std::string>().empty())
        return &(*d)[key];
    if(const Json* value=humanoid_health_default(node,key))return value;
    if(node.class_name=="Humanoid"&&key=="Jump"){static const Json idle=false;return &idle;}
    if(const Json* value=part_mass_default(world,node,key))return value;
    if(const Json* value=input_default(node,key))return value;
    if(node.class_name=="GuiService"&&key=="TouchControlsEnabled"){static const Json enabled=true;return &enabled;}
    return gui_default_property(world,node,key);
}
bool script_disabled_state(World& world,const Node& node) {
    if(node.class_name!="Script"&&node.class_name!="LocalScript")return false;
    const Json* disabled=property_value(world,node,"Disabled");
    const Json* enabled=property_value(world,node,"Enabled");
    return (disabled&&*disabled==true)||(enabled&&*enabled==false);
}
void notify_script_disabled_transition(lua_State* from,World& world,Node& node,bool was_disabled) {
    if(!was_disabled&&!node.destroyed&&script_disabled_state(world,node)) {
        node.script_state.bump_disabled_generation();
        if(world.script_disabled_transition)world.script_disabled_transition(from,node.id);
    }
}
double json_number(const Json&v){
    if(v.is_number())return v.get<double>();
    if(v.is_object()&&v.size()==1&&v.contains("$number")&&v["$number"].is_string()){
        std::string s=v["$number"].get<std::string>();if(s=="inf")return INFINITY;if(s=="-inf")return -INFINITY;if(s=="nan")return NAN;
    }
    throw std::runtime_error("invalid numeric snapshot value");
}
Json encode_number(double n){if(std::isnan(n))return Json{{"$number","nan"}};if(std::isinf(n))return Json{{"$number",n>0?"inf":"-inf"}};return n;}

void push_json(lua_State* L, World& world, const Json& v, int depth = 0);
void push_vector(lua_State* L, const Json& v) {
    if (!v.is_array() || v.size() != 3) throw std::runtime_error("invalid Vector3 snapshot value");
    double a[3];
    for (size_t i = 0; i < 3; ++i) a[i]=json_number(v[i]);
    lua_pushvector(L, a[0], a[1], a[2]);
}
void push_cframe(lua_State* L, const Json& v) {
    if (!v.is_object() || !v.contains("position") || !v["position"].is_array() || v["position"].size() != 3)
        throw std::runtime_error("invalid CFrame snapshot value");
    auto* cf = new_cframe(L);
    cf->source=v;for (size_t i=0;i<3;i++) cf->p[i]=json_number(v["position"][i]);
    if(v.contains("rotation")&&v["rotation"].is_array()&&v["rotation"].size()==9){for(size_t i=0;i<9;i++)cf->r[i]=json_number(v["rotation"][i]);}
    else cf->rotation_known=false;
    luaL_getmetatable(L, kCFrameMeta); lua_setmetatable(L, -2);
}
#include "rotation_curve_key.inc"
void push_json(lua_State* L, World& world, const Json& v, int depth) {
    if (depth > kMaxJsonDepth) throw std::runtime_error("snapshot property nesting limit exceeded");
    if (v.is_null()) lua_pushnil(L);
    else if (v.is_boolean()) lua_pushboolean(L, v.get<bool>());
    else if (v.is_number()) {
        double n = v.get<double>(); if (!std::isfinite(n)) throw std::runtime_error("nonfinite snapshot number");
        lua_pushnumber(L, n);
    } else if (v.is_string()) {
        const auto& s = v.get_ref<const std::string&>(); lua_pushlstring(L, s.data(), s.size());
    } else if (v.is_array()) {
        lua_createtable(L, static_cast<int>(v.size()), 0);
        for (size_t i=0;i<v.size();i++) { push_json(L, world, v[i], depth+1); lua_rawseti(L, -2, static_cast<int>(i+1)); }
    } else if (v.is_object()) {
        if(v.size()==1&&v.contains("$remote_array")) {
            const auto& items=v["$remote_array"];
            if(!items.is_array()||items.size()>10000)throw std::runtime_error("invalid remote array");
            lua_createtable(L,int(items.size()),0);int table=lua_gettop(L),i=1;
            for(const auto& item:items){push_json(L,world,item,depth+1);lua_rawseti(L,table,i++);}
        } else if(v.size()==1&&v.contains("$remote_map")) {
            const auto& items=v["$remote_map"];
            if(!items.is_array()||items.size()>10000)throw std::runtime_error("invalid remote dictionary");
            lua_createtable(L,0,int(items.size()));int table=lua_gettop(L);
            for(const auto& pair:items){
                if(!pair.is_array()||pair.size()!=2)throw std::runtime_error("invalid remote dictionary entry");
                push_json(L,world,pair[0],depth+1);push_json(L,world,pair[1],depth+1);lua_rawset(L,table);
            }
        } else if (v.size()==1 && v.contains("$ref")) {
            if (!v["$ref"].is_string()) throw std::runtime_error("invalid instance reference");
            Node* n = find_node(world, v["$ref"].get<std::string>());
            if (!n) lua_pushnil(L); else push_node(L, world, n);
        } else if(v.size()==1&&v.contains("$chat_status")) {
            push_chat_status(L,v["$chat_status"].get<int>());
        } else if(v.size()==1&&v.contains("$instance_handle")) {
            if(!v["$instance_handle"].is_null()&&!v["$instance_handle"].is_string())throw std::runtime_error("invalid InstanceHandle value");
            push_instance_handle(L,world,v["$instance_handle"].is_null()?"":v["$instance_handle"].get<std::string>());
        } else if(v.size()==1&&v.contains("$number")) {
            lua_pushnumber(L,json_number(v));
        } else if (v.size()==1 && v.contains("$vector3")) push_vector(L, v["$vector3"]);
        else if (v.size()==1 && v.contains("$cframe")) push_cframe(L, v["$cframe"]);
        else if (v.size()==1 && (v.contains("$bytes_hex")||v.contains("$buffer_hex"))) {
            const bool buffer=v.contains("$buffer_hex");
            const auto& encoded=v[buffer?"$buffer_hex":"$bytes_hex"];
            if(!encoded.is_string())throw std::runtime_error("invalid byte-string value");
            std::string hex=encoded.get<std::string>();
            if(hex.size()%2||hex.size()>2*1024*1024)throw std::runtime_error("invalid byte-string length");
            auto nib=[](char c)->int{if(c>='0'&&c<='9')return c-'0';if(c>='a'&&c<='f')return c-'a'+10;if(c>='A'&&c<='F')return c-'A'+10;return -1;};
            std::string bytes;bytes.reserve(hex.size()/2);
            for(size_t i=0;i<hex.size();i+=2){int a=nib(hex[i]),b=nib(hex[i+1]);if(a<0||b<0)throw std::runtime_error("invalid byte-string hex");bytes.push_back(char((a<<4)|b));}
            if(buffer){void* data=lua_newbuffer(L,bytes.size());if(!bytes.empty())std::memcpy(data,bytes.data(),bytes.size());}
            else lua_pushlstring(L,bytes.data(),bytes.size());
        }
        else if(push_ussi_export_enum_json(L,v)||push_ussi_security_capabilities_json(L,v)) {}
        else if(push_geometry_enum_json(L,v)) {}
        else if(push_content_value(L,v,&world)||push_authored_value(L,v)) {}
        else {
            if (v.size() > 10000) throw std::runtime_error("snapshot property table item limit exceeded");
            lua_createtable(L, 0, static_cast<int>(v.size()));
            for (auto it=v.begin();it!=v.end();++it) { lua_pushlstring(L,it.key().data(),it.key().size()); push_json(L,world,it.value(),depth+1); lua_rawset(L,-3); }
        }
    } else throw std::runtime_error("unsupported snapshot property value");
}

bool is_descendant_of(World& world, const Node& node, const std::string& ancestor) {
    std::string p = parent_id(world, node);
    size_t hops=0;
    while (!p.empty() && hops++ <= world.nodes.size()) {
        if (p == ancestor) return true;
        Node* parent = find_node(world,p); if (!parent) return false;
        p = parent_id(world,*parent);
    }
    return false;
}
bool class_is_a(const World& world, const std::string& actual, const std::string& requested) {
    if (actual == requested || requested == "Instance" || requested == "Object") return true;
    if (const auto* api = rust_engine_api()) {
        if (!world.rust_class_graph) {
            const std::string metadata = world.class_parents.dump();
            void* index = api->class_graph_new(reinterpret_cast<const uint8_t*>(metadata.data()), metadata.size());
            if (!index) throw std::runtime_error("invalid Rust class metadata");
            world.rust_class_graph = std::shared_ptr<void>(index, api->class_graph_free);
        }
        const int result = api->class_graph_is_a(world.rust_class_graph.get(),
            reinterpret_cast<const uint8_t*>(actual.data()), actual.size(),
            reinterpret_cast<const uint8_t*>(requested.data()), requested.size());
        if (result < 0) throw std::runtime_error("invalid Rust class query");
        return result != 0;
    }
    auto cached=world.class_ancestors.find(actual);
    if(cached!=world.class_ancestors.end())return cached->second.count(requested)!=0;
    std::unordered_set<std::string> seen;
    std::vector<std::string> todo{actual};
    while (!todo.empty()) {
        std::string c=std::move(todo.back()); todo.pop_back();
        if (!seen.insert(c).second) continue;
        auto it=world.class_parents.find(c);
        if (it==world.class_parents.end()) continue;
        if (it->is_string()) todo.push_back(it->get<std::string>());
        else if (it->is_array()) for (const auto& p:*it) if (p.is_string()) todo.push_back(p.get<std::string>());
    }
    bool result=seen.count(requested)!=0;
    world.class_ancestors.emplace(actual,std::move(seen));
    return result;
}
std::string character_motion_root_id(World&,Node&);
#include "physics_force_bindings.inc"
void push_children(lua_State* L, World& world, const Node& parent, bool recursive) {
    lua_newtable(L); int idx=1;
    for (const std::string& id:world.order) {
        Node& child=world.nodes.at(id);
        bool match = recursive ? is_descendant_of(world,child,parent.id) : parent_id(world,child)==parent.id;
        if (match) { push_node(L,world,&child); lua_rawseti(L,-2,idx++); }
    }
}
int is_a(lua_State* L) {
    Handle* h=checked_handle(L,1); const char* requested=luaL_checkstring(L,2);
    lua_pushboolean(L,class_is_a(*h->world,h->node->class_name,requested)); return 1;
}
int get_children(lua_State* L) { Handle* h=checked_handle(L,1); push_children(L,*h->world,*h->node,false); return 1; }
int get_descendants(lua_State* L) { Handle* h=checked_handle(L,1); push_children(L,*h->world,*h->node,true); return 1; }
int find_child(lua_State* L) {
    Handle* h=checked_handle(L,1); std::string name=luaL_checkstring(L,2); bool recursive=lua_gettop(L)>=3 && lua_toboolean(L,3);
    World& w=*h->world;
    for(const std::string& id:w.order) { Node& n=w.nodes.at(id); if((recursive?is_descendant_of(w,n,h->node->id):parent_id(w,n)==h->node->id) && instance_name(w,n)==name) { push_node(L,w,&n); return 1; } }
    lua_pushnil(L); return 1;
}
int find_class(lua_State* L, bool which) {
    Handle* h=checked_handle(L,1); std::string name=luaL_checkstring(L,2); bool recursive=which && lua_gettop(L)>=3 && lua_toboolean(L,3);
    World& w=*h->world;
    for(const std::string& id:w.order) { Node& n=w.nodes.at(id); bool child=recursive?is_descendant_of(w,n,h->node->id):parent_id(w,n)==h->node->id; if(child && (which?class_is_a(w,n.class_name,name):n.class_name==name)) { push_node(L,w,&n); return 1; } }
    lua_pushnil(L); return 1;
}
int find_child_class(lua_State* L) { return find_class(L,false); }
int find_child_which(lua_State* L) { return find_class(L,true); }
enum class AncestorMatch { Name, Class, Inherits };
int find_ancestor(lua_State* L, AncestorMatch mode) {
    Handle* h=checked_handle(L,1);
    size_t length=0;const char* value=luaL_checklstring(L,2,&length);
    std::string requested(value,length);World& w=*h->world;
    std::string id=parent_id(w,*h->node);
    // Received snapshots can contain unresolved parents. Bound traversal even
    // for malformed input; local parent writes already reject cycles.
    std::set<std::string> seen{h->node->id};
    while(!id.empty() && seen.insert(id).second) {
        Node* n=find_node(w,id);if(!n)break;
        bool match=mode==AncestorMatch::Name?instance_name(w,*n)==requested:
            mode==AncestorMatch::Class?n->class_name==requested:
            class_is_a(w,n->class_name,requested);
        if(match){push_node(L,w,n);return 1;}
        id=parent_id(w,*n);
    }
    lua_pushnil(L);return 1;
}
int find_ancestor_name(lua_State* L){return find_ancestor(L,AncestorMatch::Name);}
int find_ancestor_class(lua_State* L){return find_ancestor(L,AncestorMatch::Class);}
int find_ancestor_which(lua_State* L){return find_ancestor(L,AncestorMatch::Inherits);}
int is_ancestor(lua_State* L) {
    Handle* h=checked_handle(L,1);Handle* other=checked_handle(L,2);
    lua_pushboolean(L,h->world==other->world && h->node!=other->node &&
        is_descendant_of(*h->world,*other->node,h->node->id));return 1;
}
int is_descendant(lua_State* L) {
    Handle* h=checked_handle(L,1); Handle* other=checked_handle(L,2);
    lua_pushboolean(L,is_descendant_of(*h->world,*h->node,other->node->id)); return 1;
}
int get_full_name(lua_State* L) {
    Handle* h=checked_handle(L,1); World& w=*h->world;
    std::vector<std::string> parts; Node* n=h->node; size_t hops=0;
    while(n && hops++<=w.nodes.size()) { parts.push_back(instance_name(w,*n)); std::string p=parent_id(w,*n); n=p.empty()?nullptr:find_node(w,p); }
    std::string out; for(auto it=parts.rbegin();it!=parts.rend();++it){if(!out.empty())out+='.';out+=*it;}
    lua_pushlstring(L,out.data(),out.size()); return 1;
}
Node* ensure_http_service(World& world);
Node* ensure_client_service(World& world,const std::string& service);
bool local_service_supported(const std::string& service) {
    if(service=="VirtualInputManager")return true;
    if(service=="VRService"||service=="SoundService")return true;
    if(service=="PhysicsService"||service=="EncodingService")return true;
    // PathfindingService is NotReplicated; its Instance must be available
    // locally even before a path is requested. Path computation methods
    // remain unavailable until the navigation backend is implemented.
    if(service=="PathfindingService"||service=="CorePackages"||service=="SharedTableRegistry")return true;
    return service=="TextService"||service=="RunService"||service=="HttpService"||service=="ServerStorage"||service=="ServerScriptService"||service=="GuiService"||service=="UserInputService"||service=="ContextActionService"||service=="Lighting"||service=="CoreGui"||service=="Debris"||service=="CollectionService"||service=="TweenService"||service=="TestService"||service=="Stats";
}
int gui_ten_foot_interface(lua_State* L) {
    auto* h=checked_handle(L,1);
    if(h->node->class_name!="GuiService"){luaL_error(L,"GuiService receiver required");return 0;}
    // This host has a desktop console/web UI, not Roblox's console UI mode.
    // This reports presentation mode; it is not an account/platform assertion.
    lua_pushboolean(L,false);return 1;
}
int encoding_codec(lua_State* L) {
    auto* receiver=checked_handle(L,1);
    if(receiver->node->class_name!="EncodingService"||receiver->node->destroyed)
        return luaL_error(L,"EncodingService receiver required"),0;
    const auto operation=static_cast<EncodingCodec>(lua_tointeger(L,lua_upvalueindex(1)));
    int parameter=0;
    switch(operation) {
        case EncodingCodec::BufferHash:
        case EncodingCodec::StringHash:
            parameter=checked_geometry_value(L,3,kHashAlgorithm);
            break;
        case EncodingCodec::Compress: {
            checked_geometry_value(L,3,kCompressionAlgorithm);
            const double requested=luaL_optnumber(L,4,1);
            if(!std::isfinite(requested)||std::trunc(requested)!=requested||requested < -7||requested > 22)
                return luaL_argerror(L,4,"Zstandard level must be an integer from -7 to 22"),0;
            parameter=static_cast<int>(requested);
            break;
        }
        case EncodingCodec::Decompress:
        case EncodingCodec::DecompressedSize:
            checked_geometry_value(L,3,kCompressionAlgorithm);
            break;
        case EncodingCodec::Base64Encode:
        case EncodingCodec::Base64Decode:
            break;
    }
    return source_encoding_codec(L,operation,2,parameter);
}
bool encoding_service_index(lua_State* L,const std::string& key) {
    constexpr std::pair<const char*,EncodingCodec> methods[]={
        {"Base64Encode",EncodingCodec::Base64Encode},{"Base64Decode",EncodingCodec::Base64Decode},
        {"CompressBuffer",EncodingCodec::Compress},{"DecompressBuffer",EncodingCodec::Decompress},
        {"GetDecompressedBufferSize",EncodingCodec::DecompressedSize},
        {"ComputeBufferHash",EncodingCodec::BufferHash},{"ComputeStringHash",EncodingCodec::StringHash},
    };
    for(const auto& [name,operation]:methods)if(key==name) {
        lua_pushinteger(L,static_cast<int>(operation));
        lua_pushcclosure(L,encoding_codec,name,1);
        return true;
    }
    return false;
}
int get_service(lua_State* L) {
    Handle* h=checked_handle(L,1);
    if(h->node->id==kSettingsId) {
        if(std::string(luaL_checkstring(L,2))!="UserGameSettings")return luaL_error(L,"unknown user settings service"),0;
        push_node(L,*h->world,find_node(*h->world,kUserGameSettingsId));return 1;
    }
    if(h->node->id!=h->world->game_id){luaL_error(L,"GetService requires ServiceProvider");return 0;}
    std::string service=luaL_checkstring(L,2); auto it=h->world->services.find(service);
    if(it==h->world->services.end()&&local_service_supported(service)) {
        try {push_node(L,*h->world,ensure_client_service(*h->world,service));return 1;}
        catch(const std::exception& error){luaL_error(L,"local service unavailable: %s",error.what());return 0;}
    }
    if(it==h->world->services.end()){luaL_error(L,"service unavailable in snapshot: %s",service.c_str());return 0;}
    Node* n=find_node(*h->world,it->second); if(!n){luaL_error(L,"snapshot service reference is invalid");return 0;}
    push_node(L,*h->world,n); return 1;
}
int make_user_settings(lua_State*L){
    if(lua_gettop(L)!=0){luaL_error(L,"UserSettings takes no arguments");return 0;}
    auto*w=static_cast<World*>(lua_touserdata(L,lua_upvalueindex(1)));
    Node*n=find_node(*w,kSettingsId);if(!n){luaL_error(L,"UserSettings model unavailable");return 0;}
    push_node(L,*w,n);return 1;
}
#include "world_missing_engine_globals.inc"
int global_stats(lua_State* L) {
    auto& world=*static_cast<World*>(lua_touserdata(L,lua_upvalueindex(1)));
    try {
        auto it=world.services.find("Stats");
        Node* stats=it==world.services.end()?ensure_client_service(world,"Stats"):find_node(world,it->second);
        if(!stats||stats->destroyed)throw std::runtime_error("Stats service unavailable");
        push_node(L,world,stats);return 1;
    }catch(const std::exception& e){luaL_error(L,"stats: %s",e.what());return 0;}
}
int find_service(lua_State* L) {
    Handle* h=checked_handle(L,1);
    if(h->node->id==kSettingsId) {
        if(std::string(luaL_checkstring(L,2))=="UserGameSettings")push_node(L,*h->world,find_node(*h->world,kUserGameSettingsId));
        else lua_pushnil(L);
        return 1;
    }
    if(h->node->id!=h->world->game_id){luaL_error(L,"FindService requires ServiceProvider");return 0;}
    std::string service=luaL_checkstring(L,2); auto it=h->world->services.find(service);
    if(it==h->world->services.end()) {
        Node* local=nullptr;
        if(local_service_supported(service))for(auto& [id,node]:h->world->nodes)if(!node.server_owned&&!node.destroyed&&node.class_name==service){local=&node;break;}
        if(local)push_node(L,*h->world,local);else lua_pushnil(L);
    } else {Node*n=find_node(*h->world,it->second);if(n)push_node(L,*h->world,n);else lua_pushnil(L);} return 1;
}

int cframe_components(lua_State* L) { auto* c=static_cast<CFrameValue*>(luaL_checkudata(L,1,kCFrameMeta));if(!c->rotation_known){luaL_error(L,"CFrame rotation metadata is unavailable");return 0;}for(double v:c->p)lua_pushnumber(L,v);for(double v:c->r)lua_pushnumber(L,v);return 12; }
int vector3_new(lua_State*L){
    if(lua_gettop(L)>3){luaL_error(L,"Vector3.new expects at most three numbers");return 0;}
    double x=lua_isnone(L,1)?0:luaL_checknumber(L,1),y=lua_isnone(L,2)?0:luaL_checknumber(L,2),z=lua_isnone(L,3)?0:luaL_checknumber(L,3);
    // Studio 0.745 preserves IEEE infinity and NaN in vectors. Geometry and
    // movement consumers validate their own finite-input requirements.
    lua_pushvector(L,x,y,z);return 1;
}
#include "vector3_methods.inc"
int cframe_look_at(lua_State* L);
int cframe_quaternion_new(lua_State* L);
int cframe_new(lua_State*L){
    int n=lua_gettop(L);
    if(n==2&&lua_isvector(L,1)&&lua_isvector(L,2))return cframe_look_at(L);
    if(n==7)return cframe_quaternion_new(L);
    auto*cf=new_cframe(L);
    if(n==0){}
    else if(n==1&&lua_isvector(L,1)){const auto*v=lua_tovector(L,1);for(int i=0;i<3;i++)cf->p[i]=v[i];}
    else if(n==3){for(int i=0;i<3;i++){double v=luaL_checknumber(L,i+1);if(!std::isfinite(v)){luaL_error(L,"CFrame position must be finite");return 0;}cf->p[i]=v;}}
    else if(n==12){for(int i=0;i<3;i++){double v=luaL_checknumber(L,i+1);if(!std::isfinite(v)){luaL_error(L,"CFrame component must be finite");return 0;}cf->p[i]=v;}for(int i=0;i<9;i++){double v=luaL_checknumber(L,i+4);if(!std::isfinite(v)){luaL_error(L,"CFrame component must be finite");return 0;}cf->r[i]=v;}}
    else {luaL_error(L,"CFrame.new expects zero arguments, one or two Vector3 values, or three, seven, or twelve numbers");return 0;}
    luaL_getmetatable(L,kCFrameMeta);lua_setmetatable(L,-2);return 1;
}
int cframe_from_matrix(lua_State*L){
    int n=lua_gettop(L);if(n<3||n>4){luaL_error(L,"CFrame.fromMatrix expects position, xAxis, yAxis, and optional zAxis");return 0;}
    if(!lua_isvector(L,1)||!lua_isvector(L,2)||!lua_isvector(L,3)){luaL_error(L,"CFrame.fromMatrix values must be Vector3");return 0;}
    const auto*p=lua_tovector(L,1);const auto*x=lua_tovector(L,2);const auto*y=lua_tovector(L,3);double z[3];
    if(n==4){if(!lua_isvector(L,4)){luaL_error(L,"CFrame.fromMatrix zAxis must be Vector3");return 0;}const auto*v=lua_tovector(L,4);for(int i=0;i<3;i++)z[i]=v[i];}
    else {z[0]=double(x[1])*y[2]-double(x[2])*y[1];z[1]=double(x[2])*y[0]-double(x[0])*y[2];z[2]=double(x[0])*y[1]-double(x[1])*y[0];
        double length=std::hypot(z[0],z[1],z[2]);if(!std::isfinite(length)||length==0){luaL_error(L,"CFrame.fromMatrix requires independent finite basis vectors");return 0;}for(double&v:z)v/=length;}
    auto*cf=new_cframe(L);for(int i=0;i<3;i++)cf->p[i]=p[i];
    cf->r={double(x[0]),double(y[0]),double(z[0]),double(x[1]),double(y[1]),double(z[1]),double(x[2]),double(y[2]),double(z[2])};
    luaL_getmetatable(L,kCFrameMeta);lua_setmetatable(L,-2);return 1;
}
#include "cframe_transforms.inc"

void set_vector_constant(lua_State*L,const char*name,double x,double y,double z){lua_pushvector(L,x,y,z);lua_setfield(L,-2,name);}
int cframe_index(lua_State* L) {
    auto* c=static_cast<CFrameValue*>(luaL_checkudata(L,1,kCFrameMeta)); const char* k=luaL_checkstring(L,2);
    if(std::strcmp(k,"Position")==0)lua_pushvector(L,c->p[0],c->p[1],c->p[2]);
    else if(std::strcmp(k,"X")==0)lua_pushnumber(L,c->p[0]);
    else if(std::strcmp(k,"Y")==0)lua_pushnumber(L,c->p[1]);
    else if(std::strcmp(k,"Z")==0)lua_pushnumber(L,c->p[2]);
    else if(std::strcmp(k,"Rotation")==0){checked_cframe(L,1);return push_frame_value(L,{},c->r);}
    else if(std::strcmp(k,"XVector")==0||std::strcmp(k,"RightVector")==0||std::strcmp(k,"YVector")==0||std::strcmp(k,"UpVector")==0||std::strcmp(k,"ZVector")==0||std::strcmp(k,"LookVector")==0){if(!c->rotation_known){luaL_error(L,"CFrame rotation metadata is unavailable");return 0;}if(std::strcmp(k,"XVector")==0||std::strcmp(k,"RightVector")==0)lua_pushvector(L,c->r[0],c->r[3],c->r[6]);else if(std::strcmp(k,"YVector")==0||std::strcmp(k,"UpVector")==0)lua_pushvector(L,c->r[1],c->r[4],c->r[7]);else if(std::strcmp(k,"ZVector")==0)lua_pushvector(L,c->r[2],c->r[5],c->r[8]);else lua_pushvector(L,-c->r[2],-c->r[5],-c->r[8]);}
    else if(std::strcmp(k,"GetComponents")==0)lua_pushcfunction(L,cframe_components,"GetComponents");
    else {luaL_getmetatable(L,kCFrameMeta);lua_pushvalue(L,2);lua_rawget(L,-2);lua_remove(L,-2);} return 1;
}
int run_service_true(lua_State* L) {
    auto* h = checked_handle(L,1);
    if (h->node->class_name != "RunService") { luaL_error(L,"RunService receiver required"); return 0; }
    lua_pushboolean(L,true); return 1;
}
int run_service_false(lua_State* L) {
    auto* h = checked_handle(L,1);
    if (h->node->class_name != "RunService") { luaL_error(L,"RunService receiver required"); return 0; }
    lua_pushboolean(L,false); return 1;
}
int clone_instance(lua_State* L);
bool client_service_index(lua_State* L,World& world,Node& node,const std::string& key);
int destroy_instance(lua_State* L);
int clear_children(lua_State* L);
int get_attribute(lua_State* L);
int get_attributes(lua_State* L);
int set_attribute(lua_State* L);
int property_changed_signal(lua_State* L);
int world_root_bulk_move(lua_State* L);
int attribute_changed_signal(lua_State* L);
int fire_bindable(lua_State* L);
int fire_remote_server(lua_State* L);
int invoke_remote_server(lua_State* L);
int invoke_remote_server_continue(lua_State* L,int status);
int invoke_bindable(lua_State* L);
int invoke_bindable_continue(lua_State* L,int status);
int wait_for_child(lua_State* L);
int send_chat(lua_State* L);
int send_chat_continue(lua_State* L,int status);
int send_universe_chat(lua_State* L);
int send_universe_chat_continue(lua_State* L,int status);
Node* ensure_current_camera(World& w,Node& workspace);
void set_current_camera(World& w,Node& workspace,Node* camera);
void refresh_current_camera(World& w);
void check_mutation_budget(World& w,const std::vector<Json>& entries);
void append_mutations(World& w,std::vector<Json> entries);
Signal& signal_for(World& w,Node& node,const std::string& key) {
    if(key=="ChildAdded"||key=="ChildRemoved"||key=="DescendantAdded"||key=="DescendantRemoving"||key=="AncestryChanged")w.hierarchy_events_observed=true;
    std::string id=Json::array({node.id,key}).dump();
    auto it=w.signals.find(id);if(it!=w.signals.end())return *it->second;
    if(w.signals.size()>=4096)throw std::runtime_error("local signal count limit exceeded");
    auto signal=std::make_unique<Signal>();signal->world=&w;signal->node=&node;signal->key=key;
    auto& ref=*signal;
    w.signal_nodes.insert(node.id);
    w.signals.emplace(id,std::move(signal));
    // Player.Idled has one engine-owned subscription in the client World.
    // Actor VMs have independent Lua worlds and never own client close policy.
    if(key=="Idled"&&node.class_name=="Player"&&node.id==w.local_player&&
       !w.actor_runtime&&w.actor_id.empty()&&w.connections.size()<2048) {
        auto connection=std::make_shared<Connection>();connection->signal=&ref;
        connection->id=w.next_connection_id++;
        connection->generation=w.connection_generation;
        connection->native_kind=Connection::NativeKind::ClientIdle;
        w.client_idle_control_enabled=true;
        w.client_idle_player_id=node.id;
        w.client_idle.start(std::chrono::duration_cast<std::chrono::milliseconds>(
            std::chrono::steady_clock::now().time_since_epoch()).count());
        w.connections.push_back(std::move(connection));
    }
    return ref;
}
void push_signal(lua_State* L,Signal& signal) {
    lua_pushlightuserdata(L,&signal);lua_rawget(L,LUA_REGISTRYINDEX);
    if(lua_isuserdata(L,-1))return;lua_pop(L,1);
    auto* handle=static_cast<SignalHandle*>(lua_newuserdatatagged(L,sizeof(SignalHandle),0));handle->signal=&signal;
    luaL_getmetatable(L,kSignalMeta);lua_setmetatable(L,-2);
    lua_pushlightuserdata(L,&signal);lua_pushvalue(L,-2);lua_rawset(L,LUA_REGISTRYINDEX);
}
void emit(World& w,Node& node,const std::string& key,Json args=Json::array(),int argument_ref=LUA_NOREF,int argument_count=0,bool response_ready=false,bool engine_idle_event=false) {
    if(!w.signal_nodes.count(node.id))return;
    std::string id=Json::array({node.id,key}).dump();auto signal=w.signals.find(id);if(signal==w.signals.end())return;
    prune_signal_waiters(w,signal->second.get());
    if(w.events.size()>=10000){w.event_overflow=true;return;}
    for(auto& connection:w.connections) {
        auto& c=*connection;
        if(c.signal==signal->second.get() && !c.signal_waiter && c.connected && c.enabled && (!c.once||!c.once_pending)) {
            if(w.events.size()>=10000){w.event_overflow=true;return;}
            w.events.push_back(PendingEvent{&c,nullptr,args,argument_ref,argument_count,LUA_NOREF,response_ready,engine_idle_event});if(c.once)c.once_pending=true;
        }
    }
    auto waiters=w.waiters.find(signal->second.get());
    if(waiters!=w.waiters.end()) {
        std::vector<SignalWaiter> retained;
        retained.reserve(waiters->second.size());
        for(auto waiter:waiters->second) {
            auto* connection=waiter.connection;
            if(!connection||!connection->connected||connection->manually_disconnected
               ||connection->auto_disconnected) {
                if(waiter.reference!=LUA_NOREF)lua_unref(waiter.thread,waiter.reference);
                continue;
            }
            // A disabled Wait remains pending for the next event after Enable.
            if(!connection->enabled) {retained.push_back(waiter);continue;}
            if(w.events.size()>=10000) {
                w.event_overflow=true;
                retained.push_back(waiter);
                continue;
            }
            w.events.push_back(PendingEvent{connection,waiter.thread,args,argument_ref,
                                            argument_count,waiter.reference,response_ready});
        }
        waiters->second=std::move(retained);
    }
}
void queue_context_input(World&,const std::string&,int,int,int,bool);
void context_reserve(World&,size_t,size_t);
void virtual_input_record_host(World&,const Json&);
#include "input_bindings.inc"
#include "context_action_bindings.inc"
#include "render_step_bindings.inc"
#include "collection_bindings.inc"
#include "instance_queries.inc"
#include "service_lookup_events.inc"
#include "player_movement_modes.inc"
std::vector<Node*> ancestors(World& w,const std::string& parent) {
    std::vector<Node*> result;std::string id=parent;
    while(!id.empty()&&result.size()<=w.nodes.size()) {auto* p=find_node(w,id);if(!p)break;result.push_back(p);id=parent_id(w,*p);}
    return result;
}
struct HierarchyEventBatch {
    World& world;
    HierarchyEventIndex index;
    HierarchyEventIndex* previous;
    explicit HierarchyEventBatch(World& value):world(value),previous(value.hierarchy_event_index) {
        world.hierarchy_event_index=&index;
    }
    ~HierarchyEventBatch(){world.hierarchy_event_index=previous;}
};
std::vector<Node*> hierarchy_event_subtree(World& w,Node& node,const std::string& new_parent) {
    std::vector<Node*> subtree{&node};
    if(!w.hierarchy_event_index) {
        for(const auto& id:w.order)if(is_descendant_of(w,w.nodes.at(id),node.id))subtree.push_back(&w.nodes.at(id));
        return subtree;
    }
    auto& index=*w.hierarchy_event_index;
    // Build once, lazily after a hierarchy observer exists. Later creates only
    // add the new suffix; nodes can arrive before their parent in the stream.
    for(;index.indexed_nodes<w.order.size();++index.indexed_nodes) {
        auto& child=w.nodes.at(w.order[index.indexed_nodes]);
        auto parent=parent_id(w,child);
        index.links.emplace(&child,HierarchyEventIndex::Link{parent,index.indexed_nodes});
        if(!parent.empty())index.children[parent].insert(&child);
    }
    auto& link=index.links.at(&node);
    if(link.parent!=new_parent) {
        if(!link.parent.empty()) {
            auto previous=index.children.find(link.parent);
            if(previous!=index.children.end())previous->second.erase(&node);
        }
        if(!new_parent.empty())index.children[new_parent].insert(&node);
        link.parent=new_parent;
    }
    std::unordered_set<Node*> seen{&node};
    for(size_t i=0;i<subtree.size();++i) {
        auto children=index.children.find(subtree[i]->id);
        if(children==index.children.end())continue;
        for(auto* child:children->second)if(seen.insert(child).second)subtree.push_back(child);
    }
    // Preserve the previous world insertion order, including descendants in
    // separate branches. Hash traversal order must not change signal delivery.
    std::sort(subtree.begin()+1,subtree.end(),[&](Node* a,Node* b){return index.links.at(a).order<index.links.at(b).order;});
    return subtree;
}
void parent_events(World& w,Node& node,const std::string& old_parent,const std::string& new_parent) {
    if(old_parent==new_parent)return;
    if(node.class_name=="ScrollingFrame")w.active_scroll_layouts.insert(node.id);
    if(class_is_a(w,node.class_name,"GuiObject")) {
        if(new_parent.empty())w.gui_parent_order.erase(node.id);
        else w.gui_parent_order[node.id]=w.next_gui_parent_order++;
    }
    collection_parent_events(w,node,old_parent,new_parent);
    player_membership_events(w,node,old_parent,new_parent);
    invalidate_joint_topology(w,node,"Parent");
    if(!w.hierarchy_events_observed) {
        emit(w,node,"Changed",Json::array({"Parent"}));emit(w,node,"property:Parent");
        refresh_humanoid_root_part_events(w);return;
    }
    auto subtree=hierarchy_event_subtree(w,node,new_parent);
    auto old=ancestors(w,old_parent),now=ancestors(w,new_parent);
    for(auto* p:old)if(std::find(now.begin(),now.end(),p)==now.end())for(auto* n:subtree)emit(w,*p,"DescendantRemoving",Json::array({Json{{"$ref",n->id}}}));
    if(auto* p=find_node(w,old_parent))emit(w,*p,"ChildRemoved",Json::array({Json{{"$ref",node.id}}}));
    emit(w,node,"Changed",Json::array({"Parent"}));emit(w,node,"property:Parent");
    for(auto* n:subtree) {
        auto parent=n==&node?new_parent:parent_id(w,*n);
        emit(w,*n,"AncestryChanged",Json::array({Json{{"$ref",n->id}},parent.empty()?Json(nullptr):Json{{"$ref",parent}}}));
    }
    if(auto* p=find_node(w,new_parent))emit(w,*p,"ChildAdded",Json::array({Json{{"$ref",node.id}}}));
    for(auto* p:now)if(std::find(old.begin(),old.end(),p)==old.end())for(auto* n:subtree)emit(w,*p,"DescendantAdded",Json::array({Json{{"$ref",n->id}}}));
    refresh_humanoid_root_part_events(w);
}
void property_events(World& w,Node& n,const std::string& key) {
    if(n.class_name=="Humanoid"&&key=="RootPart")sync_humanoid_root_part_watch(w,n);
    bool image_unloaded=false;
    if(key=="Image"&&gui_image(n)) {
        n.image_load_token.clear();
        image_unloaded=std::exchange(n.image_loaded,false);
    }
    invalidate_joint_topology(w,n,key);
    // A direct received mass update already emits its signal here. Keep the
    // derived-value watch in sync so the next scheduler tick does not emit it twice.
    auto mass_watch=w.part_mass_watches.find(n.id);
    if(mass_watch!=w.part_mass_watches.end()&&mass_watch->second.contains(key))
        mass_watch->second[key]=observed_part_mass(w,n,key);
    std::string canonical=key=="archivable"?"Archivable":key;
    if(class_is_a(w,n.class_name,"ValueBase")) {
        if(canonical=="Value"){auto* v=property_value(w,n,key);emit(w,n,"Changed",Json::array({v?*v:Json(nullptr)}));}
    } else emit(w,n,"Changed",Json::array({canonical}));
    emit(w,n,"property:"+canonical);
    if(key=="Name"||key=="Parent")refresh_humanoid_root_part_events(w);
    if(image_unloaded)property_events(w,n,"IsLoaded");
}
#include "camera_projection.inc"
#include "model_pivots.inc"
#include "collision_groups.inc"
#include "no_collision_constraints.inc"
#include "world_ray_query.inc"
#include "world_raycast_bindings.inc"
#include "world_overlap_bindings.inc"
#include "world_region_bindings.inc"
#include "world_legacy_raycast_bindings.inc"
#include "mouse_projection.inc"
#include "gui_layout.inc"
#include "textbox_focus.inc"
#include "virtual_input_bindings.inc"
#include "animation_bindings.inc"
// State helpers are authored by the character-state workstream.
#include "humanoid_states.inc"
int local_move(lua_State* L) {
    auto* h=checked_handle(L,1);const char* method=lua_tostring(L,lua_upvalueindex(1));
    if(h->node->destroyed){luaL_error(L,"movement receiver is destroyed");return 0;}
    if(!class_is_a(*h->world,h->node->class_name,"Humanoid")&&
       !(h->node->class_name=="Player"&&std::strcmp(method,"Move")==0)) {
        luaL_error(L,"movement method requires a Humanoid or Player receiver");return 0;
    }
    movement_bindings::enqueue(L,method,h->node->id,[L,h]() {
        World& w=*h->world;
        auto service=w.services.find("Workspace");
        if(service==w.services.end())return std::string("{}");
        auto* workspace=find_node(w,service->second);
        if(!workspace||workspace->destroyed)return std::string("{}");
        auto* camera=ensure_current_camera(w,*workspace);
        if(!camera)return std::string("{}");
        const auto* value=property_value(w,*camera,"CFrame");
        if(!value)return std::string("{}");
        push_json(L,w,*value);
        auto* frame=checked_cframe(L,-1,false);
        const bool known=frame->rotation_known;
        const auto rotation=frame->r;
        lua_pop(L,1);
        if(!known)return std::string("{}");
        for(double v:rotation)if(!std::isfinite(v))
            return std::string("{\"error\":\"camera_transform_invalid\"}");
        return Json{{"instance_id",camera->id},{"rotation",rotation},
                    {"origin",origin(w,*camera,"CFrame")}}.dump();
    });movement_bindings::tag_latest_mutation_cursor(L,static_cast<int>(h->world->mutations.size()));return 0;
}
int local_json(lua_State* L) {
    auto* h=checked_handle(L,1);
    if(h->node->class_name!="HttpService"){luaL_error(L,"JSON method requires HttpService");return 0;}
    const int count=lua_gettop(L);
    if(!json_bindings::push_method(L,lua_tostring(L,lua_upvalueindex(1)))){luaL_error(L,"unknown JSON method");return 0;}
    lua_insert(L,1);lua_call(L,count,1);return 1;
}
bool read_guid_entropy(std::array<unsigned char,16>& bytes) {
    // Match Random's host-entropy policy: never substitute a clock or PRNG seed.
    // The stream must be destroyed before raising any Luau error.
    return host_entropy(bytes.data(),bytes.size());
}
int generate_guid(lua_State* L) {
    auto* h=checked_handle(L,1);
    if(h->node->class_name!="HttpService") {luaL_error(L,"GenerateGUID requires HttpService");return 0;}
    bool braces=true;
    const int type=lua_type(L,2);
    if(type==LUA_TBOOLEAN)braces=lua_toboolean(L,2)!=0;
    else if(type==LUA_TNUMBER)braces=lua_tonumber(L,2)!=0;
    else if(type==LUA_TSTRING) {
        size_t length=0;const char* text=lua_tolstring(L,2,&length);
        char lowered[5]{};
        if(length==4||length==5)for(size_t i=0;i<length;++i)
            lowered[i]=text[i]>='A'&&text[i]<='Z'?text[i]-'A'+'a':text[i];
        if(length==4&&std::memcmp(lowered,"true",4)==0)braces=true;
        else if(length==5&&std::memcmp(lowered,"false",5)==0)braces=false;
        else {luaL_argerror(L,2,"unable to cast string to bool");return 0;}
    } else if(type!=LUA_TNONE&&type!=LUA_TNIL) {
        luaL_argerror(L,2,"unable to cast value to bool");return 0;
    }
    std::array<unsigned char,16> bytes{};
    if(!read_guid_entropy(bytes)){luaL_error(L,"GenerateGUID could not read host entropy");return 0;}
    bytes[6]=(bytes[6]&0x0f)|0x40;
    bytes[8]=(bytes[8]&0x3f)|0x80;
    static constexpr char hex[]="0123456789abcdef";
    char result[38];size_t cursor=0;
    if(braces)result[cursor++]='{';
    for(size_t i=0;i<bytes.size();++i) {
        if(i==4||i==6||i==8||i==10)result[cursor++]='-';
        result[cursor++]=hex[bytes[i]>>4];result[cursor++]=hex[bytes[i]&0xf];
    }
    if(braces)result[cursor++]='}';
    lua_pushlstring(L,result,cursor);return 1;
}
int is_loaded(lua_State* L) {
    Handle* h=checked_handle(L,1);
    if(h->node->id!=h->world->game_id){luaL_error(L,"IsLoaded requires DataModel");return 0;}
    lua_pushboolean(L,h->world->initial_replication_complete);return 1;
}
int humanoid_take_damage(lua_State* L);
int part_is_grounded(lua_State* L);
int part_closest_point_on_surface(lua_State* L);
int world_root_spherecast(lua_State* L);
int world_root_blockcast(lua_State* L);
int part_velocity_at_position(lua_State* L);
int instance_newindex(lua_State* L);
int is_property_modified(lua_State* L);
int reset_property_to_default(lua_State* L);
int remove_instance(lua_State* L);
int instance_get_actor(lua_State* L) {
    auto* handle=checked_handle(L,1);auto& world=*handle->world;Node* node=handle->node;
    for(size_t depth=0;node&&depth<=world.nodes.size();++depth) {
        if(class_is_a(world,node->class_name,"Actor")){push_node(L,world,node);return 1;}
        node=find_node(world,parent_id(world,*node));
    }
    lua_pushnil(L);return 1;
}
// Unveil resolves deprecated Instance spellings before member dispatch. Keep
// these aliases on the same implementations and property-change signals.
std::string canonical_instance_member(std::string key,const Node* node=nullptr) {
    if(node) {
        const auto& c=node->class_name;
        if(c=="Player"&&key=="character")return "Character";
        if((c=="BodyGyro"||c=="BodyAngularVelocity")&&key=="maxTorque")return "MaxTorque";
        if(c=="BodyGyro"&&key=="cframe")return "CFrame";
        if(c=="BodyAngularVelocity"&&key=="angularvelocity")return "AngularVelocity";
        if(c=="BodyThrust"&&key=="force")return "Force";
        if(c=="BodyThrust"&&key=="location")return "Location";
    }
    if(node&&(node->class_name=="Mouse"||node->class_name=="PlayerMouse"||node->class_name=="PluginMouse")) {
        if(key=="hit")return "Hit";
        if(key=="target")return "Target";
    }
    if(node&&key=="TextWrap"&&(node->class_name=="TextLabel"||node->class_name=="TextButton"||node->class_name=="TextBox"))return "TextWrapped";
    static const std::unordered_map<std::string,std::string> aliases={
        {"className","ClassName"},{"archivable","Archivable"},
        {"children","GetChildren"},{"getChildren","GetChildren"},
        {"clone","Clone"},{"destroy","Destroy"},{"remove","Remove"},
        {"findFirstChild","FindFirstChild"},{"waitForChild","WaitForChild"},
        {"isA","IsA"},{"isDescendantOf","IsDescendantOf"},
        {"childAdded","ChildAdded"},{"getFullName","GetFullName"}
    };
    auto found=aliases.find(key);return found==aliases.end()?key:found->second;
}
#include "http_instance.inc"
#include "body_position_bindings.inc"
#include "linear_velocity_bindings.inc"
#include "sound_playback.inc"
int body_velocity_last_force(lua_State* L) {
    auto* h=checked_handle(L,1);
    if(h->node->class_name!="BodyVelocity") {luaL_error(L,"GetLastForce requires BodyVelocity");return 0;}
    // Roblox documents this legacy query as unimplemented, returning zero.
    lua_pushvector(L,0,0,0);return 1;
}
bool tween_playback_index(lua_State*,World&,Node&,const std::string&);
int text_service_request(lua_State*);
int text_service_continue(lua_State*,int);
int text_service_get_text_size(lua_State*);
bool text_font_index(lua_State*,World&,Node&,const std::string&);
int instance_index(lua_State* L) {
    Handle* h=checked_handle(L,1); World& w=*h->world; Node& n=*h->node; std::string key=canonical_instance_member(luaL_checkstring(L,2),&n);
    if(w.lightweight_assets&&omitted_lightweight_asset(key)
       &&!local_value(w,n,key)&&!n.properties.contains(key))
        return luaL_error(L,"property '%s' unavailable in lightweight asset mode",key.c_str()),0;
    if(n.class_name=="VirtualInputManager"&&virtual_input_index(L,key))return 1;
    if(n.class_name=="UserInputService"&&virtual_input_device_index(L,w,n,key))return 1;
    if(key=="DataCost"&&!SourceScheduler::has_capability(L,"LocalUser"))return luaL_error(L,"DataCost: lacking capability LocalUser"),0;
    if(n.id==kSettingsId&&key=="IsUserFeatureEnabled") {
        lua_pushcfunction(L,[](lua_State* S)->int{checked_handle(S,1);luaL_error(S,"User feature '%s' is not configured in this client",luaL_checkstring(S,2));return 0;},"IsUserFeatureEnabled");return 1;
    }
    if(n.class_name=="UserGameSettings") {
        if(key=="GetCameraYInvertValue") {
            lua_pushcfunction(L,[](lua_State* S)->int{auto* h=checked_handle(S,1);if(h->node->class_name!="UserGameSettings"||h->node->destroyed){luaL_error(S,"live UserGameSettings receiver required");return 0;}const auto* value=property_value(*h->world,*h->node,"CameraYInverted");lua_pushinteger(S,value&&*value==true?-1:1);return 1;},"GetCameraYInvertValue");return 1;
        }
        if(key=="SetCameraYInvertVisible"||key=="SetGamepadCameraSensitivityVisible") {
            lua_pushstring(L,key=="SetCameraYInvertVisible"?"IsUsingCameraYInverted":"IsUsingGamepadCameraSensitivity");
            lua_pushcclosure(L,[](lua_State* S)->int{auto* h=checked_handle(S,1);if(h->node->class_name!="UserGameSettings"||h->node->destroyed){luaL_error(S,"live UserGameSettings receiver required");return 0;}std::string key=lua_tostring(S,lua_upvalueindex(1));if(h->node->properties[key]!=true){h->node->properties[key]=true;property_events(*h->world,*h->node,key);}return 0;},key.c_str(),1);return 1;
        }
    }
    if(n.class_name=="VRService"&&key=="VREnabled") {
        auto value=w.input_capabilities.find("VREnabled");lua_pushboolean(L,value!=w.input_capabilities.end()&&*value==true);return 1;
    }
    if(n.class_name=="DataModel"&&(key=="workspace"||key=="Workspace")) {
        auto it=w.services.find("Workspace");
        if(it!=w.services.end()){push_node(L,w,find_node(w,it->second));return 1;}
    }
    if(n.class_name=="BodyForce"&&key=="force")key="Force";
    if(n.class_name=="BodyPosition") {
        if(key=="position")key="Position";
        if(key=="maxForce")key="MaxForce";
        if(body_position_index(L,w,n,key))return 1;
    }
    if(n.class_name=="BodyVelocity") {
        if(key=="velocity")key="Velocity";
        if(key=="maxForce")key="MaxForce";
        if(key=="GetLastForce"||key=="lastForce") {
            lua_pushcfunction(L,body_velocity_last_force,"GetLastForce");return 1;
        }
    }
    if(lua_toboolean(L,lua_upvalueindex(1)))key=hidden_property_name(n,key);else enforce_scriptability(L,w,n,key);
    if(key=="Tags") {
        try{auto tags=collection_tags(n);std::string bytes;for(const auto& tag:tags){if(!bytes.empty())bytes+='\0';bytes+=tag;}lua_pushlstring(L,bytes.data(),bytes.size());return 1;}
        catch(const std::exception& e){return luaL_error(L,"Tags unavailable: %s",e.what()),0;}
    }
    try{if(linear_velocity_index(L,w,n,key))return 1;}catch(const std::exception& e){luaL_error(L,"%s",e.what());return 0;}
    if(n.class_name=="VectorForce"&&key=="RelativeTo") {
        try {
            const Json* value=property_value(w,n,key);
            if(value&&value->is_object()&&value->contains("$enum")) {
                const auto& item=(*value)["$enum"];
                if(!item.is_object()||item.value("type","")!="ActuatorRelativeTo"||!item.contains("value"))throw std::runtime_error("invalid RelativeTo enum");
                value=&item.at("value");
            }
            if(!value||!value->is_number_integer()||value->get<int>()<0||value->get<int>()>2)throw std::runtime_error("RelativeTo is unavailable");
            push_geometry_item(L,kActuatorRelativeTo,value->get<int>());return 1;
        }catch(const std::exception& e){luaL_error(L,"VectorForce: %s",e.what());return 0;}
    }
    if(n.class_name=="RunService"&&key=="BindToRenderStep"){lua_pushcfunction(L,run_service_bind_render,"BindToRenderStep");return 1;}
    if(n.class_name=="RunService"&&key=="UnbindFromRenderStep"){lua_pushcfunction(L,run_service_unbind_render,"UnbindFromRenderStep");return 1;}
    if(n.class_name=="DataModel"&&(key=="HttpGet"||key=="HttpGetAsync")){lua_pushcclosurek(L,instance_http_get,"HttpGet",0,instance_http_get_continue);return 1;}
    try{if(context_action_index(L,w,n,key))return 1;}catch(const std::exception& e){luaL_error(L,"context action member: %s",e.what());return 0;}
    try{if(input_index(L,w,n,key))return 1;}catch(const std::exception& e){luaL_error(L,"input member: %s",e.what());return 0;}
    if(tween_playback_index(L,w,n,key))return 1;
    if(n.id==w.game_id&&(key=="HttpGet"||key=="HttpGetAsync")) {
        lua_getfield(L,LUA_REGISTRYINDEX,"Headless.HttpGet");
        if(lua_isfunction(L,-1))return 1;
        lua_pop(L,1);
    }
    if(key=="Source"&&(n.class_name=="LocalScript"||n.class_name=="ModuleScript"||n.class_name=="Script")) {
        // A bytecode-only script has no replicated Source text. Preserve the
        // constructor's empty text field; this does not decompile its bytecode.
        if(!n.script_state.code().is_object()||!n.script_state.code().contains("source")){lua_pushliteral(L,"");return 1;}
        auto& source=n.script_state.code()["source"].get_ref<const std::string&>();lua_pushlstring(L,source.data(),source.size());return 1;
    }
    if(humanoid_state_index(L,w,n,key))return 1;
    if(n.class_name=="Humanoid"&&(key=="Jumping"||key=="FreeFalling"||key=="Running"||key=="PlatformStanding"||key=="MoveToFinished")) {
        try{push_signal(L,signal_for(w,n,key));}catch(const std::exception& e){luaL_error(L,"Humanoid state signal unavailable: %s",e.what());return 0;}
        return 1;
    }
    if(n.class_name=="UserInputService"&&input_capability_key(key)) {
        lua_pushboolean(L,input_capability(w,key));return 1;
    }
    if(text_font_index(L,w,n,key))return 1;
    if(key=="ContentText"&&text_layout_node(n)) {
        auto content=text_content(w,n);
        if(!content){luaL_error(L,"ContentText is unavailable until Text is known");return 0;}
        lua_pushlstring(L,content->data(),content->size());return 1;
    }
    if(textbox_focus_index(L,w,n,key))return 1;
    if(n.class_name=="TextService"&&(key=="GetFamilyInfoAsync"||key=="GetTextBoundsAsync")) {
        lua_pushboolean(L,key=="GetTextBoundsAsync");lua_pushcclosurek(L,text_service_request,key.c_str(),1,text_service_continue);return 1;
    }
    if(n.class_name=="TextService"&&key=="GetTextSize") {
        lua_pushcfunction(L,text_service_get_text_size,"GetTextSize");return 1;
    }
    if(int kind=gui_enum_kind(w,n,key);kind>=0) {
        try{push_geometry_item(L,kind,gui_enum_value(w,n,key));return 1;}
        catch(const std::exception& e){luaL_error(L,"GUI enum unavailable: %s",e.what());return 0;}
    }
    if(display_property(w,n,key)) {
        try{GuiLayoutPass pass{w};push_json(L,w,pass.value(n,key));return 1;}
        catch(const std::exception& e){luaL_error(L,"display layout unavailable: %s",e.what());return 0;}
    }
    if(n.class_name=="GuiService"&&key=="GetGuiInset"){lua_pushcfunction(L,gui_get_inset,"GetGuiInset");return 1;}
    if((n.class_name=="UIListLayout"||n.class_name=="UIGridLayout")&&key=="ApplyLayout") {
        lua_pushcfunction(L,gui_apply_layout,"ApplyLayout");return 1;
    }
    try{if(animation_index(L,w,n,key))return 1;}catch(const std::exception& e){luaL_error(L,"animation member unavailable: %s",e.what());return 0;}
    if(key=="Name"){auto s=instance_name(w,n);lua_pushlstring(L,s.data(),s.size());return 1;}
    if(key=="ClassName"){lua_pushlstring(L,n.class_name.data(),n.class_name.size());return 1;}
    if(key=="Parent"){std::string p=parent_id(w,n);if(p.empty())lua_pushnil(L);else{Node* parent=find_node(w,p);if(!parent)lua_pushnil(L);else push_node(L,w,parent);}return 1;}
    if(camera_index(L,w,n,key))return 1;
    try{if(collision_groups_index(L,w,n,key))return 1;}catch(const std::exception& e){luaL_error(L,"collision groups: %s",e.what());return 0;}
    if(key=="GetNoCollisionConstraints"&&pivot_part(w,n)){lua_pushcfunction(L,get_no_collision_constraints,"GetNoCollisionConstraints");return 1;}
    if((n.class_name=="Workspace"||n.class_name=="WorldModel")&&key=="Raycast") {lua_pushcfunction(L,world_root_raycast,"WorldRoot.Raycast");return 1;}
    if(collision_world_root(n)&&key=="Spherecast") {lua_pushcfunction(L,world_root_spherecast,"WorldRoot.Spherecast");return 1;}
    if(collision_world_root(n)&&key=="Blockcast") {lua_pushcfunction(L,world_root_blockcast,"WorldRoot.Blockcast");return 1;}
    if(collision_world_root(n)&&(key=="GetPartBoundsInBox"||key=="GetPartBoundsInRadius")) {lua_pushlstring(L,key.data(),key.size());lua_pushcclosure(L,world_root_overlap,key.c_str(),1);return 1;}
    if(collision_world_root(n)&&world_region_method(key)) {lua_pushlstring(L,key.data(),key.size());lua_pushcclosure(L,world_root_region,key.c_str(),1);return 1;}
    if(collision_world_root(n)&&world_legacy_ray_method(key)) {lua_pushlstring(L,key.data(),key.size());lua_pushcclosure(L,world_root_legacy_ray,key.c_str(),1);return 1;}
    if(collision_world_root(n)&&key=="BulkMoveTo"){lua_pushcfunction(L,world_root_bulk_move,"WorldRoot.BulkMoveTo");return 1;}
    try{if(sound_index(L,w,n,key))return 1;}catch(const std::exception& e){luaL_error(L,"Sound playback: %s",e.what());return 0;}
    try{if(mouse_projection_index(L,w,n,key))return 1;}catch(const std::exception& e){luaL_error(L,"mouse projection: %s",e.what());return 0;}
    try{if(model_pivot_index(L,w,n,key))return 1;}catch(const std::exception& e){luaL_error(L,"model pivot unavailable: %s",e.what());return 0;}
    if(n.class_name=="SpecialMesh"&&key=="MeshType") {
        try {
            const auto* value=property_value(w,n,key);
            if(!value)throw std::runtime_error("MeshType is unavailable");
            Json item=value->is_number_integer()?Json{{"$enum",{{"type","MeshType"},{"value",*value}}}}:*value;
            if(!item.contains("$enum")||item["$enum"].value("type","")!="MeshType"||!push_ussi_export_enum_json(L,item))
                throw std::runtime_error("invalid MeshType");
            return 1;
        }catch(const std::exception& e){return luaL_error(L,"SpecialMesh: %s",e.what()),0;}
    }
    try{if(part_transform_index(L,w,n,key))return 1;}catch(const std::exception& e){luaL_error(L,"part transform unavailable: %s",e.what());return 0;}
    try{if(attachment_index(L,w,n,key))return 1;}catch(const std::exception& e){luaL_error(L,"attachment transform unavailable: %s",e.what());return 0;}
    try{if(joint_index(L,w,n,key))return 1;}catch(const std::exception& e){luaL_error(L,"joint state unavailable: %s",e.what());return 0;}
    try{if(assembly_root_index(L,w,n,key))return 1;}catch(const std::exception& e){luaL_error(L,"assembly root unavailable: %s",e.what());return 0;}
    try{if(part_mass_index(L,w,n,key))return 1;}catch(const std::exception& e){luaL_error(L,"part mass unavailable: %s",e.what());return 0;}
    if(n.class_name=="Workspace"&&key=="CurrentCamera") {
        try{push_node(L,w,ensure_current_camera(w,n));return 1;}
        catch(const std::exception& e){luaL_error(L,"CurrentCamera unavailable: %s",e.what());return 0;}
    }
    if(n.id==w.game_id && key=="JobId" && !n.properties.contains("JobId")){luaL_error(L,"JobId is unavailable in this snapshot");return 0;}
    if(n.id==w.game_id && key=="IsLoaded"){lua_pushcfunction(L,is_loaded,"IsLoaded");return 1;}
    if(n.id==w.game_id && key=="Loaded"){push_signal(L,signal_for(w,n,"Loaded"));return 1;}
    if(n.class_name=="Players"&&(key=="PlayerAdded"||key=="PlayerRemoving")){w.player_membership_events_observed=true;push_signal(L,signal_for(w,n,key));return 1;}
    if(n.class_name=="Players" && key=="LocalPlayer"){Node* player=find_node(w,w.local_player);if(player)push_node(L,w,player);else lua_pushnil(L);return 1;}
    if(client_service_index(L,w,n,key)||collection_instance_index(L,w,n,key)||player_movement_mode_index(L,w,n,key)
       ||missing_engine_global_index(L,w,n,key)||actor_message_index(L,w,n,key))return 1;
    if(n.class_name=="EncodingService"&&encoding_service_index(L,key))return 1;
    if(n.class_name=="TweenService"&&key=="GetValue"){lua_pushcfunction(L,tween_service_get_value,"GetValue");return 1;}
    if(key=="GetService"||key=="getService"||key=="service"){lua_pushcfunction(L,get_service,"GetService");return 1;}
    if(key=="FindService"){lua_pushcfunction(L,find_service,"FindService");return 1;}
    if(key=="GetChildren"){lua_pushcfunction(L,get_children,"GetChildren");return 1;}
    if(key=="GetDescendants"){lua_pushcfunction(L,get_descendants,"GetDescendants");return 1;}
    if(key=="QueryDescendants"){lua_pushcfunction(L,query_descendants,"QueryDescendants");return 1;}
    if(key=="IsPropertyModified"){lua_pushcfunction(L,is_property_modified,"IsPropertyModified");return 1;}
    if(key=="ResetPropertyToDefault"){lua_pushcfunction(L,reset_property_to_default,"ResetPropertyToDefault");return 1;}
    if(key=="FindFirstChild"){lua_pushcfunction(L,find_child,"FindFirstChild");return 1;}
    if(key=="WaitForChild"){lua_pushcfunction(L,wait_for_child,"WaitForChild");return 1;}
    if(key=="FindFirstChildOfClass"){lua_pushcfunction(L,find_child_class,"FindFirstChildOfClass");return 1;}
    if(key=="FindFirstChildWhichIsA"){lua_pushcfunction(L,find_child_which,"FindFirstChildWhichIsA");return 1;}
    if(key=="FindFirstAncestor"){lua_pushcfunction(L,find_ancestor_name,"FindFirstAncestor");return 1;}
    if(key=="FindFirstAncestorOfClass"){lua_pushcfunction(L,find_ancestor_class,"FindFirstAncestorOfClass");return 1;}
    if(key=="FindFirstAncestorWhichIsA"){lua_pushcfunction(L,find_ancestor_which,"FindFirstAncestorWhichIsA");return 1;}
    if(key=="IsAncestorOf"){lua_pushcfunction(L,is_ancestor,"IsAncestorOf");return 1;}
    if(key=="IsA"){lua_pushcfunction(L,is_a,"IsA");return 1;}
    if(key=="IsDescendantOf"){lua_pushcfunction(L,is_descendant,"IsDescendantOf");return 1;}
    if(key=="GetFullName"){lua_pushcfunction(L,get_full_name,"GetFullName");return 1;}
    if(key=="GetActor"){lua_pushcfunction(L,instance_get_actor,"GetActor");return 1;}
    if(key=="Remove"){lua_pushcfunction(L,remove_instance,"Remove");return 1;}
    if(key=="Clone"||key=="clone"){lua_pushcfunction(L,clone_instance,"Clone");return 1;}
    if(key=="Destroy"||key=="destroy"){lua_pushcfunction(L,destroy_instance,"Destroy");return 1;}
    if(key=="ClearAllChildren"){lua_pushcfunction(L,clear_children,"ClearAllChildren");return 1;}
    if(key=="GetAttribute"){lua_pushcfunction(L,get_attribute,"GetAttribute");return 1;}
    if(key=="GetAttributes"){lua_pushcfunction(L,get_attributes,"GetAttributes");return 1;}
    if(key=="SetAttribute"){lua_pushcfunction(L,set_attribute,"SetAttribute");return 1;}
    if(key=="GetPropertyChangedSignal"){lua_pushcfunction(L,property_changed_signal,"GetPropertyChangedSignal");return 1;}
    if(key=="GetAttributeChangedSignal"){lua_pushcfunction(L,attribute_changed_signal,"GetAttributeChangedSignal");return 1;}
    if((class_is_a(w,n.class_name,"Humanoid")&&(key=="Move"||key=="MoveTo"))||
       (n.class_name=="Player"&&key=="Move")) {
        lua_pushlstring(L,key.data(),key.size());lua_pushcclosurek(L,local_move,key.c_str(),1,nullptr);return 1;
    }
    if(class_is_a(w,n.class_name,"BasePart")&&
       (key=="ApplyImpulse"||key=="ApplyImpulseAtPosition")) {
        lua_pushlstring(L,key.data(),key.size());lua_pushcclosurek(L,local_apply_impulse,key.c_str(),1,nullptr);return 1;
    }
    if(class_is_a(w,n.class_name,"BasePart")&&key=="IsGrounded") {
        lua_pushcfunction(L,part_is_grounded,"IsGrounded");return 1;
    }
    if(class_is_a(w,n.class_name,"BasePart")&&key=="GetClosestPointOnSurface") {
        lua_pushcfunction(L,part_closest_point_on_surface,"GetClosestPointOnSurface");return 1;
    }
    if(class_is_a(w,n.class_name,"BasePart")&&key=="GetVelocityAtPosition") {
        lua_pushcfunction(L,part_velocity_at_position,"GetVelocityAtPosition");return 1;
    }
    if(n.class_name=="Humanoid"&&key=="RootPart") {
        Node* root=humanoid_root_part(w,n);
        if(root)push_node(L,w,root);else lua_pushnil(L);
        return 1;
    }
    if(n.class_name=="Humanoid"&&(key=="TakeDamage"||key=="takeDamage")) {
        lua_pushcfunction(L,humanoid_take_damage,"TakeDamage");return 1;
    }
    if(n.class_name=="Humanoid"&&(key=="HealthChanged"||key=="Died")) {
        try{push_signal(L,signal_for(w,n,key));}catch(const std::exception& e){luaL_error(L,"health signal unavailable: %s",e.what());}return 1;
    }
    if(n.class_name=="HttpService"&&(key=="JSONEncode"||key=="JSONDecode")) {
        lua_pushlstring(L,key.data(),key.size());lua_pushcclosurek(L,local_json,key.c_str(),1,nullptr);return 1;
    }
    if(n.class_name=="HttpService"&&key=="UrlEncode"){lua_pushcfunction(L,http_url_encode,"UrlEncode");return 1;}
    if(n.class_name=="HttpService"&&key=="GenerateGUID"){lua_pushcfunction(L,generate_guid,"GenerateGUID");return 1;}
    if(n.class_name=="GuiService"&&key=="IsTenFootInterface"){lua_pushcfunction(L,gui_ten_foot_interface,"IsTenFootInterface");return 1;}
    if(n.class_name=="RunService" && (key=="Heartbeat"||key=="RenderStepped"||key=="PreRender"||key=="Stepped"||key=="PreSimulation"||key=="PostSimulation")) {push_signal(L,signal_for(w,n,key));return 1;}
    if(n.class_name=="ClickDetector" && (key=="MouseClick"||key=="RightMouseClick"||key=="MouseHoverEnter"||key=="MouseHoverLeave")) {push_signal(L,signal_for(w,n,key));return 1;}
    if((n.class_name=="ProximityPrompt"&&(key=="Triggered"||key=="TriggerEnded"||key=="PromptButtonHoldBegan"||key=="PromptButtonHoldEnded"))
       ||(class_is_a(w,n.class_name,"BasePart")&&(key=="Touched"||key=="TouchEnded"))) {push_signal(L,signal_for(w,n,key));return 1;}
    if((class_is_a(w,n.class_name,"GuiButton")&&(key=="Activated"||key=="MouseButton1Click"||key=="MouseButton1Down"||key=="MouseButton1Up"))
            ||(class_is_a(w,n.class_name,"GuiObject")&&(key=="InputBegan"||key=="InputChanged"||key=="InputEnded"||key=="MouseEnter"||key=="MouseLeave"||key=="MouseWheelForward"||key=="MouseWheelBackward"))
            ||(n.class_name=="TextBox"&&(key=="Focused"||key=="FocusLost"))) {push_signal(L,signal_for(w,n,key));return 1;}
    if(n.class_name=="BindableEvent" && key=="Event") {push_signal(L,signal_for(w,n,"Event"));return 1;}
    if(n.class_name=="BindableEvent" && key=="Fire") {lua_pushcfunction(L,fire_bindable,"Fire");return 1;}
    if((n.class_name=="RemoteEvent"||n.class_name=="UnreliableRemoteEvent")&&key=="FireServer"){lua_pushcfunction(L,fire_remote_server,"FireServer");return 1;}
    if(n.class_name=="RemoteFunction"&&key=="InvokeServer"){lua_pushcclosurek(L,invoke_remote_server,"InvokeServer",0,invoke_remote_server_continue);return 1;}
    if(n.class_name=="BindableFunction"&&key=="Invoke"){lua_pushcclosurek(L,invoke_bindable,"Invoke",0,invoke_bindable_continue);return 1;}
    if(n.class_name=="BindableFunction"&&key=="OnInvoke"){luaL_error(L,"OnInvoke is a write-only callback; use Invoke");return 0;}
    if(n.class_name=="RemoteFunction"&&key=="OnClientInvoke"){luaL_error(L,"OnClientInvoke is a write-only callback");return 0;}
    if(n.class_name=="TextChannel" && key=="SendAsync") {lua_pushcclosurek(L,send_chat,"SendAsync",0,send_chat_continue);return 1;}
    if(n.class_name=="TextChatService" && key=="SendUniverseChatMessageAsync") {lua_pushcclosurek(L,send_universe_chat,"SendUniverseChatMessageAsync",0,send_universe_chat_continue);return 1;}
    if(key=="Changed")for(const char* property:{"AbsoluteSize","AbsolutePosition","AbsoluteRotation","AbsoluteContentSize","AbsoluteCellSize","AbsoluteCellCount","AbsoluteCanvasSize","AbsoluteWindowSize","CanvasPosition"})watch_display_property(w,n,property);
    if(key=="Changed")for(const char* property:{"WorldCFrame","WorldPosition","WorldOrientation","WorldRotation","WorldAxis","WorldSecondaryAxis","TransformedCFrame","TransformedWorldCFrame"})watch_attachment_property(w,n,property);
    if(key=="Changed")watch_joint_property(w,n,"Active");
    if(key=="Changed"&&n.class_name=="Humanoid")watch_humanoid_root_part(w,n,key);
    if(key=="Changed")watch_assembly_root(w,n,"AssemblyRootPart");
    if(key=="Changed")watch_part_mass(w,n,"Changed");
    if(n.class_name=="Player"&&n.id==w.local_player&&key=="Idled") {
        try{push_signal(L,signal_for(w,n,key));}catch(const std::exception& e){luaL_error(L,"Player.Idled unavailable: %s",e.what());}return 1;
    }
    bool schema_event=false;
    auto class_events=w.class_events.find(n.class_name);
    if(class_events!=w.class_events.end())for(const auto& event:*class_events)if(event.is_string()&&event.get<std::string>()==key)schema_event=true;
    if((n.class_name=="Player"&&(key=="CharacterAdded"||key=="CharacterRemoving"))
        ||((n.class_name=="TextChatService"||n.class_name=="TextChannel")&&key=="MessageReceived")
        ||(n.class_name=="TextChatService"&&key=="UniverseChatMessageReceived")
        ||((n.class_name=="RemoteEvent"||n.class_name=="UnreliableRemoteEvent")&&key=="OnClientEvent")||schema_event) {
        try{push_signal(L,signal_for(w,n,key));}catch(const std::exception& e){luaL_error(L,"signal unavailable: %s",e.what());}return 1;
    }
    if(key=="Changed"||key=="AttributeChanged"||key=="ChildAdded"||key=="ChildRemoved"||key=="DescendantAdded"||key=="DescendantRemoving"||key=="AncestryChanged"||key=="Destroying") {
        try{push_signal(L,signal_for(w,n,key));}catch(const std::exception& e){luaL_error(L,"signal unavailable: %s",e.what());}return 1;
    }
    if(n.class_name=="RunService" && (key=="IsStudio" || key=="IsServer" || key=="IsEdit" || key=="IsRunMode")){lua_pushcfunction(L,run_service_false,"RunService mode");return 1;}
    if(n.class_name=="RunService" && (key=="IsRunning" || key=="IsClient")){lua_pushcfunction(L,run_service_true,"RunService mode");return 1;}
    const Json* value=property_value(w,n,key);
    if(!value&&n.class_name=="TextSource"&&key=="UserId")value=property_value(w,n,"UserIdReplicated");
    if(!value)for(const auto& id:w.order){Node& child=w.nodes.at(id);if(parent_id(w,child)==n.id&&instance_name(w,child)==key){push_node(L,w,&child);return 1;}}
    // Roblox reads Player.Character as nil until a character is loaded; scripts
    // (e.g. PlayerModule's Popper) rely on that instead of an error.
    if(!value&&n.class_name=="Player"&&key=="Character"){lua_pushnil(L);return 1;}
    if(!value){luaL_error(L,"property '%s' unavailable in snapshot for %s (%s)",key.c_str(),n.name.c_str(),n.class_name.c_str());return 0;}
    // The replication reader retains PhysicalProperties as a 5/6-scalar
    // array. Expose the typed source value without altering received data.
    if(const Json* descriptor=property_descriptor(n,key)) {
        if(descriptor->contains("enum_type")&&value->is_number_integer()) {
            push_json(L,w,Json{{"$enum",{{"type",descriptor->at("enum_type")},{"value",*value}}}});return 1;
        }
        if(descriptor->value("type","")=="PhysicalProperties"&&value->is_array()) {
            try {
                Json fields=*value;
                if(fields.size()==5)fields.push_back(0.0);
                if(push_authored_value(L,Json{{"$physical_properties",std::move(fields)}}))return 1;
            } catch(const std::exception& error) {
                luaL_error(L,"invalid PhysicalProperties snapshot value: %s",error.what());return 0;
            }
        }
    }
    try{push_json(L,w,*value);}
    catch(const std::exception& error){return luaL_error(L,"property value unavailable: %s",error.what()),0;}
    return 1;
}

Json vector_to_json(lua_State* L,int i){const LUA_VECTOR_TYPE* v=lua_tovector(L,i);if(!v)throw std::runtime_error("invalid vector");return Json{{"$vector3",Json::array({encode_number(v[0]),encode_number(v[1]),encode_number(v[2])})}};}
std::string bytes_hex(const char* data,size_t size){static const char*hex="0123456789abcdef";std::string out;out.reserve(size*2);for(size_t i=0;i<size;i++){unsigned char c=data[i];out.push_back(hex[c>>4]);out.push_back(hex[c&15]);}return out;}
Json convert_lua_value(lua_State* L,World& w,int i,int depth=0,std::unordered_set<const void*>* seen=nullptr,size_t* budget=nullptr){
    if(depth>kMaxJsonDepth)throw std::runtime_error("mutation nesting limit exceeded");
    size_t local_budget=kMaxMutationValue;if(!budget)budget=&local_budget;
    switch(lua_type(L,i)){
    case LUA_TNIL:return nullptr;case LUA_TBOOLEAN:return lua_toboolean(L,i)!=0;
    case LUA_TNUMBER:{double n=lua_tonumber(L,i);return encode_number(n);}
    case LUA_TSTRING:{size_t n;const char*s=lua_tolstring(L,i,&n);if(n>*budget)throw std::runtime_error("local value exceeds JSON size limit");*budget-=n;if(valid_utf8(s,n))return std::string(s,n);return Json{{"$bytes_hex",bytes_hex(s,n)}};}
    case LUA_TVECTOR:return vector_to_json(L,i);
    case LUA_TUSERDATA:
        {Json raycast;if(capture_raycast_result(L,w,i,raycast))return raycast;}
        {Json geometry;if(capture_ussi_export_enum(L,i,geometry)||capture_ussi_security_capabilities(L,i,geometry)||capture_geometry_enum(L,i,geometry))return geometry;}
        {Json authored;if(capture_authored_value(L,i,authored))return authored;}
        if(has_meta(L,i,kPriorityMeta)){int value=*static_cast<int*>(lua_touserdata(L,i));return Json{{"$enum",Json{{"type","AnimationPriority"},{"name",kPriorityNames[priority_index(value)]},{"value",value}}}};}
        if(has_meta(L,i,kChatStatusMeta)){int value=*static_cast<int*>(lua_touserdata(L,i));return Json{{"$enum",Json{{"type","TextChatMessageStatus"},{"name",kChatStatusNames[value-1]},{"value",value}}}};}
        if(has_meta(L,i,kInstanceHandleMeta)){auto* h=static_cast<InstanceHandleValue*>(lua_touserdata(L,i));if(h->world!=&w)throw std::runtime_error("InstanceHandle belongs to a different snapshot");return Json{{"$instance_handle",h->id.empty()?Json(nullptr):Json(h->id)}};}
        if(has_meta(L,i,kInstanceMeta)){auto*h=static_cast<Handle*>(lua_touserdata(L,i));if(h->world!=&w)throw std::runtime_error("instance belongs to a different snapshot");return Json{{"$ref",h->node->id}};}
        if(has_meta(L,i,kCFrameMeta)){auto*c=static_cast<CFrameValue*>(lua_touserdata(L,i));Json frame;if(c->rotation_known)frame={{"position",{encode_number(c->p[0]),encode_number(c->p[1]),encode_number(c->p[2])}},{"rotation",{encode_number(c->r[0]),encode_number(c->r[1]),encode_number(c->r[2]),encode_number(c->r[3]),encode_number(c->r[4]),encode_number(c->r[5]),encode_number(c->r[6]),encode_number(c->r[7]),encode_number(c->r[8])}}};else frame=c->source;return Json{{"$cframe",frame}};}
        throw std::runtime_error("unsupported userdata mutation");
    case LUA_TTABLE:{
        int table_index=lua_absindex(L,i);const void* ptr=lua_topointer(L,table_index);std::unordered_set<const void*> local_seen;if(!seen)seen=&local_seen;if(!seen->insert(ptr).second)throw std::runtime_error("cyclic table mutation unsupported");
        Json out=Json::object();size_t count=0;lua_pushnil(L);while(lua_next(L,table_index)){if(++count>5000){lua_pop(L,2);throw std::runtime_error("table mutation item limit exceeded");}std::string k;if(lua_type(L,-2)==LUA_TSTRING){size_t n;const char*s=lua_tolstring(L,-2,&n);if(n>*budget){lua_pop(L,2);throw std::runtime_error("table key exceeds JSON size limit");}*budget-=n;k.assign(s,n);}else if(lua_type(L,-2)==LUA_TNUMBER){double n=lua_tonumber(L,-2);k=std::to_string(n);}else{lua_pop(L,2);throw std::runtime_error("table mutation keys must be strings or numbers");}out[k]=convert_lua_value(L,w,-1,depth+1,seen,budget);lua_pop(L,1);}seen->erase(ptr);return out;
    }
    default:throw std::runtime_error("unsupported local property value");
    }
}
#include "property_defaults.inc"
void check_mutation_budget(World& w, const std::vector<Json>& entries) {
    size_t bytes=0;
    for(const auto& entry:entries)bytes+=entry.dump().size();
    if(w.mutations.size()+entries.size()>kMaxMutations || bytes>kMaxMutationBytes-w.mutation_bytes)
        throw std::runtime_error("local mutation output limit exceeded");
}
void append_mutations(World& w, std::vector<Json> entries) {
    for(auto& entry:entries){w.mutation_bytes+=entry.dump().size();w.mutations.push_back(std::move(entry));}
}
#include "text_fonts.inc"
#include "text_layout.inc"
#include "rich_text.inc"
#include "text_service.inc"
#include "text_measurement.inc"
#include "humanoid_health.inc"
#include "humanoid_root_part.inc"
#include "humanoid_jump.inc"
#include "part_transforms.inc"
#include "engine_pose.inc"
#include "engine_owned_parts.inc"
#include "attachment_transforms.inc"
#include "joint_graph.inc"
#include "joint_poses.inc"
#include "world_bulk_move.inc"
#include "model_movement.inc"
#include "model_scaling.inc"
#include "part_mass_properties.inc"
#include "part_motion_queries.inc"
#include "closest_point_on_surface.inc"
#include "world_spherecast.inc"
#include "convex_cast.inc"
#include "world_blockcast.inc"
Json node_record(const Node& n) {
    return Json{{"id",n.id},{"class_name",n.class_name},{"name",n.name},
        {"parent",n.parent.empty()?Json(nullptr):Json(n.parent)},
        {"properties",n.properties},{"origins",n.origins},
        {"attributes",n.attributes.snapshot()},{"attributes_known",n.attributes.known()},{"destroyed",n.destroyed},
        {"collection_tags",n.collection_tags},{"collection_tags_known",n.collection_tags_known},
        {"server_owned",n.server_owned},{"constructor_defaults_known",n.constructor_defaults_known},
        {"property_revisions",n.revisions}};
}
Node* ensure_client_service(World& w,const std::string& service) {
    // Client-local services do not require a server-replicated service object.
    // Preserve their identity in the usual local overlay.
    for(auto& [id,node]:w.nodes)if(!node.server_owned&&!node.destroyed&&node.class_name==service) {
        w.services[service]=id;return &node;
    }
    if(w.nodes.size()>=kMaxNodes)throw std::runtime_error("world node limit exceeded");
    Node node;node.id=w.allocate_local_id();node.class_name=service;
    node.name=service;node.parent=w.game_id;node.server_owned=false;node.attributes.set_known(true);node.collection_tags_known=true;
    node.properties["Archivable"]=true;node.origins["Archivable"]="class_default";
    node.origins=Json{{"Name","local"},{"ClassName","local"},{"Parent","local"}};
    std::vector<Json> entries{Json{{"kind","create"},{"node",node_record(node)}}};check_mutation_budget(w,entries);
    auto inserted=w.nodes.emplace(node.id,std::move(node));if(!inserted.second)throw std::runtime_error("local service ID collision");
    auto& created=inserted.first->second;w.order.push_back(created.id);w.services[service]=created.id;
    append_mutations(w,std::move(entries));parent_events(w,created,"",w.game_id);return &created;
}
Node* ensure_http_service(World& w){return ensure_client_service(w,"HttpService");}
using Children = std::unordered_map<std::string,std::vector<std::string>>;
Children children_index(World& w) {
    Children children;
    for(const auto& id:w.order) {const auto& node=w.nodes.at(id);auto p=parent_id(w,node);if(!p.empty())children[p].push_back(id);}
    return children;
}
bool archivable(World& w,const Node& n) {
    auto* value=property_value(w,n,"Archivable");
    if(!value || !value->is_boolean())throw std::runtime_error("Archivable is unavailable; cannot faithfully clone this instance");
    return value->get<bool>();
}
void remap_references(Json& value,const std::unordered_map<std::string,std::string>& ids,int depth=0) {
    if(depth>kMaxJsonDepth)throw std::runtime_error("clone reference nesting limit exceeded");
    if(value.is_object() && value.size()==1 && value.contains("$ref") && value["$ref"].is_string()) {
        auto it=ids.find(value["$ref"].get<std::string>());
        if(it!=ids.end())value["$ref"]=it->second;
    } else if(value.is_object() && value.size()==1 && value.contains("$instance_handle") && value["$instance_handle"].is_string()) {
        auto it=ids.find(value["$instance_handle"].get<std::string>());if(it!=ids.end())value["$instance_handle"]=it->second;
    } else if(value.is_object()||value.is_array()) for(auto& item:value)remap_references(item,ids,depth+1);
}
Node clone_node_copy(World& w,const Node& original,const std::string& clone_id,
                     const std::string& source_root,
                     const std::unordered_map<std::string,std::string>& ids) {
    Node copy=original;
    copy.image_load_token.clear();copy.image_loaded=false;
    if(copy.class_name=="LocalScript"||copy.class_name=="ModuleScript"||copy.class_name=="Script")
        copy.script_state.set_origin(original.id);
    copy.id=clone_id;copy.name=instance_name(w,original);copy.destroyed=false;copy.server_owned=false;
    copy.script_destroy_cleanup_complete=false;
    const auto parent=parent_id(w,original);
    copy.parent=original.id==source_root?"":ids.at(parent);
    auto overrides=w.local_properties.find(original.id);
    if(overrides!=w.local_properties.end())
        for(auto it=overrides->second.begin();it!=overrides->second.end();++it)copy.properties[it.key()]=it.value();
    copy.properties.erase("Name");copy.properties.erase("Parent");
    remap_references(copy.properties,ids);
    // Instance-valued attributes are InstanceHandle values, not raw
    // Instance userdata; their internal targets are remapped too.
    if(copy.attributes.size()!=0) {
        Json attributes=copy.attributes.snapshot();
        remap_references(attributes,ids);
        copy.attributes.replace(attributes,copy.attributes.known());
    }
    copy.origins=Json::object();
    for(auto it=copy.properties.begin();it!=copy.properties.end();++it)copy.origins[it.key()]="local";
    if(pivot_model(w,original)&&!local_value(w,original,"WorldPivot")
       &&origin(w,original,"WorldPivot")=="class_default")copy.origins["WorldPivot"]="class_default";
    copy.origins["Name"]="local";copy.origins["Parent"]="local";
    return copy;
}
thread_local bool cloning_client_template=false;
void actor_mutation_clone_graph(World&,const Json&,const std::string*);
int clone_instance(lua_State* L) {
    Handle* h=checked_handle(L,1);World& w=*h->world;
    try {
        if(!archivable(w,*h->node)){lua_pushnil(L);return 1;}
        // A freshly-created, empty Instance is commonly cloned immediately
        // (for example by an authored effects script). Building a complete
        // parent-to-children hash table for the entire replicated world in
        // that case adds tens of thousands of allocations to a one-node copy.
        // Check for a cloneable direct child with an allocation-free scan
        // first; only build the subtree index when traversal can use it.
        bool has_cloneable_child=false;
        for(const auto& candidate_id:w.order) {
            if(candidate_id==h->node->id)continue;
            const Node& candidate=w.nodes.at(candidate_id);
            bool direct_child=false;
            if(const Json* override_parent=local_value(w,candidate,"Parent")) {
                direct_child=override_parent->is_object()
                    &&override_parent->contains("$ref")
                    &&(*override_parent)["$ref"].is_string()
                    &&(*override_parent)["$ref"].get_ref<const std::string&>()==h->node->id;
            } else {
                direct_child=candidate.parent==h->node->id;
            }
            if(direct_child&&archivable(w,candidate)) {
                has_cloneable_child=true;
                break;
            }
        }
        std::optional<Children> children;
        if(has_cloneable_child)children.emplace(children_index(w));
        std::deque<std::string> pending{h->node->id};std::vector<std::string> selected;
        while(!pending.empty()) {
            auto id=pending.front();pending.pop_front();const auto& node=w.nodes.at(id);
            if(!archivable(w,node))continue;
            selected.push_back(id);
            if(children) {
                auto child=children->find(id);
                if(child!=children->end())for(const auto& value:child->second)pending.push_back(value);
            }
        }
        if(w.nodes.size()>kMaxNodes+1000||selected.size()>kMaxNodes+1000-w.nodes.size())
            throw std::runtime_error("local instance limit exceeded");
        std::unordered_map<std::string,std::string> ids;
        uint64_t next=w.next_local_id;
        for(const auto& id:selected) {std::string target;do{target=w.local_id_prefix+std::to_string(next++);}while(w.nodes.count(target));ids[id]=target;}
        std::vector<Node> copies;std::vector<Json> mutations;
        for(const auto& id:selected) {
            const Node& original=w.nodes.at(id);
            Node copy=clone_node_copy(w,original,ids.at(id),h->node->id,ids);
            copies.push_back(std::move(copy));
        }
        Json source_ids=Json::array(),clone_ids=Json::array(),birth_hashes=Json::array();
        for(size_t i=0;i<selected.size();++i) {
            source_ids.push_back(selected[i]);
            clone_ids.push_back(copies[i].id);
            birth_hashes.push_back(source_crypto::hex(source_crypto::sha256(node_record(copies[i]).dump())));
        }
        Json clone_action{{"source_root",h->node->id},{"source_ids",std::move(source_ids)},
            {"clone_ids",std::move(clone_ids)},{"node_hashes",std::move(birth_hashes)}};
        if(cloning_client_template) {
            clone_action["kind"]="template_clone";
            clone_action["id"]=ids.at(h->node->id);
            clone_action["nodes"]=selected.size();
        } else {
            clone_action["kind"]="clone_graph";
        }
        mutations.push_back(std::move(clone_action));
        if(!cloning_client_template)
            for(const auto& copy:copies)mutations.push_back(Json{{"kind","create"},{"node",node_record(copy)}});
        check_mutation_budget(w,mutations);
        clone_model_scale_caches(w,ids);
        // Clone traversal uses global creation order. Preserve the original
        // siblings' parent-insertion order even after they were reparented.
        std::vector<std::pair<uint64_t,std::string>> gui_copies;
        for(const auto& id:selected)if(id!=h->node->id&&class_is_a(w,w.nodes.at(id).class_name,"GuiObject")) {
            auto order=w.gui_parent_order.find(id);
            gui_copies.emplace_back(order==w.gui_parent_order.end()?0:order->second,ids.at(id));
        }
        std::stable_sort(gui_copies.begin(),gui_copies.end(),[](const auto& a,const auto& b){return a.first<b.first;});
        for(const auto& entry:gui_copies)w.gui_parent_order[entry.second]=w.next_gui_parent_order++;
        for(auto& copy:copies){auto id=copy.id;
            if(copy.class_name=="ScrollingFrame")w.active_scroll_layouts.insert(id);
            w.nodes.emplace(id,std::move(copy));w.order.push_back(id);}
        w.next_local_id=next;append_mutations(w,std::move(mutations));
        push_node(L,w,find_node(w,ids.at(h->node->id)));return 1;
    }catch(const std::exception& e){luaL_error(L,"Clone failed: %s",e.what());return 0;}
}
void destroy_roots(World& w,const std::vector<std::string>& roots,
                   lua_State* from=nullptr,bool local_destroy_boundary=false) {
    const auto children=children_index(w);
    std::deque<std::string> pending(roots.begin(),roots.end());std::vector<std::string> selected;
    std::unordered_set<std::string> seen;
    while(!pending.empty()) {
        auto id=pending.front();pending.pop_front();if(!seen.insert(id).second)continue;
        selected.push_back(id);auto child=children.find(id);
        if(child!=children.end())for(const auto& value:child->second)pending.push_back(value);
    }
    if(selected.empty())return;
    std::vector<Json> mutations{Json{{"kind","destroy"},{"ids",selected}}};check_mutation_budget(w,mutations);
    for(const auto& id:selected) {
        Node& node=w.nodes.at(id);emit(w,node,"Destroying");
        if(local_destroy_boundary&&from&&!node.destroyed&&!node.script_destroy_cleanup_complete
           &&w.script_destroy_transition
           &&(node.class_name=="LocalScript"||node.class_name=="Script")) {
            // Native ScriptContext removal invalidates the current owner list
            // once at Destroy. Do this synchronously so already queued or
            // waiting owner work is removed, while a currently executing
            // stack can finish and register later work after this boundary.
            w.script_destroy_transition(from,id);
            node.script_destroy_cleanup_complete=true;
        }
        if(node.class_name=="VirtualInputManager") {
            virtual_playback_stop(w,false);
            w.input.tape.recording=false;
        }
        // AnimationTrack instances are VM handles outside the Instance tree.
        // Preserve a final own-character Stop before destroying either the
        // track handle or its Animator invalidates the ownership check.
        for(auto& [track_id,track]:w.animation.tracks) {
            if(track_id!=id&&track.animator!=id)continue;
            auto* track_node=find_node(w,track_id);
            if(track_node&&!track_node->destroyed&&track.playing)track_stop(w,*track_node,track,0);
        }
        w.locally_destroyed.insert(id);
        parent_events(w,node,parent_id(w,node),"");
        node.destroyed=true;node.parent.clear();node.origins["Parent"]="local";
        if(id==w.local_player) {
            w.client_idle_control_enabled=false;
            w.client_idle_event_due=false;
            w.client_idle_disconnect_requested=false;
            w.client_idle_player_id.clear();
        }
        w.pivot_caches.erase(id);
        w.model_scale_caches.erase(id);
        w.pending_motor_transforms.erase(id);w.motor_applied_transforms.erase(id);
        w.local_properties[id]["Parent"]=nullptr;
        for(auto& connection:w.connections)if(connection->signal->node==&node) {connection->connected=false;connection->auto_disconnected=true;}
    }
    refresh_humanoid_root_part_events(w);
    prune_signal_waiters(w);
    // Keep tombstones alive for retained userdata/references. Erasing their
    // unordered_map records here would leave dangling VM handles.
    append_mutations(w,std::move(mutations));
}
#include "client_service_bindings.inc"
void set_current_camera(World& w,Node& workspace,Node* camera) {
    if(workspace.destroyed)throw std::runtime_error("Workspace is destroyed");
    if(camera&&camera->class_name!="Camera")throw std::runtime_error("CurrentCamera must be a Camera or nil");
    // CurrentCamera is client-owned, never a guessed server reference. A nil,
    // destroyed, or detached camera requires a fresh local camera.
    bool create=!camera||camera->destroyed||!is_descendant_of(w,*camera,workspace.id);
    Node fresh;
    std::vector<Json> entries;
    if(create) {
        if(w.nodes.size()>=kMaxNodes+1000)throw std::runtime_error("local instance limit exceeded");
        fresh.class_name="Camera";fresh.name="Camera";fresh.parent=workspace.id;
        fresh.server_owned=false;fresh.attributes.set_known(true);fresh.collection_tags_known=true;
        fresh.id=w.allocate_local_id();
        const Json* camera=constructor_class("Camera");
        if(!camera)throw std::runtime_error("Camera constructor metadata unavailable");
        const auto& properties=camera->at("properties");
        for(auto p=properties.begin();p!=properties.end();++p) {
            fresh.properties[p.key()]=p.value().at("value");fresh.origins[p.key()]="class_default";
        }
        // The client-created active camera starts in the core scripts'
        // documented Custom mode. Existing cameras retain their own mode.
        // https://create.roblox.com/docs/reference/engine/enums/CameraType
        fresh.properties["CameraType"]=5;fresh.origins["CameraType"]="local";
        fresh.origins["Name"]="local";fresh.origins["Parent"]="local";
        entries.push_back(Json{{"kind","create"},{"node",node_record(fresh)}});
    }
    const std::string selected=create?fresh.id:camera->id;
    const Json value{{"$ref",selected}};
    const Json* previous=property_value(w,workspace,"CurrentCamera");
    const bool changed=!previous||*previous!=value;
    if(!changed&&!create)return;
    entries.push_back(Json{{"kind","set"},{"id",workspace.id},{"property","CurrentCamera"},{"value",value}});
    std::vector<std::string> obsolete;
    bool reparent_selected=false;
    for(const auto& id:w.order) {
        const auto& node=w.nodes.at(id);
        // Only direct Workspace cameras are discarded. Cameras stored in a
        // Folder/Model remain available for subsequent selection.
        if(id!=selected&&!node.destroyed&&node.class_name=="Camera"&&parent_id(w,node)==workspace.id) {
            obsolete.push_back(id);
            if(!create&&is_descendant_of(w,*camera,id))reparent_selected=true;
        }
    }
    if(reparent_selected)entries.push_back(Json{{"kind","set"},{"id",selected},{"property","Parent"},{"value",{{"$ref",workspace.id}}}});
    // Reserve the full replacement before changing the active reference.
    auto budget_entries=entries;
    if(!obsolete.empty()) {
        auto children=children_index(w);
        std::deque<std::string> pending(obsolete.begin(),obsolete.end());
        std::vector<std::string> removed;std::unordered_set<std::string> seen;
        while(!pending.empty()) {
            auto id=pending.front();pending.pop_front();
            if(id==selected||!seen.insert(id).second)continue;
            removed.push_back(id);
            auto child=children.find(id);if(child!=children.end())for(const auto& value:child->second)pending.push_back(value);
        }
        budget_entries.push_back(Json{{"kind","destroy"},{"ids",removed}});
    }
    check_mutation_budget(w,budget_entries);
    if(create) {
        w.nodes.emplace(selected,std::move(fresh));w.order.push_back(selected);
        camera=find_node(w,selected);
    }
    // Publish the new reference before callbacks observe the old camera's
    // destruction. Keep the same userdata identity on subsequent reads.
    w.local_properties[workspace.id]["CurrentCamera"]=value;
    sync_camera_viewport(w,*camera);
    append_mutations(w,std::move(entries));
    if(create)parent_events(w,*camera,"",workspace.id);
    if(reparent_selected) {
        auto parent=parent_id(w,*camera);
        w.local_properties[selected]["Parent"]=Json{{"$ref",workspace.id}};
        parent_events(w,*camera,parent,workspace.id);
    }
    destroy_roots(w,obsolete);
    if(changed)property_events(w,workspace,"CurrentCamera");
}
Node* ensure_current_camera(World& w,Node& workspace) {
    const Json* value=property_value(w,workspace,"CurrentCamera");
    Node* camera=nullptr;
    if(value&&value->is_object()&&value->contains("$ref")&&(*value)["$ref"].is_string())
        camera=find_node(w,(*value)["$ref"].get<std::string>());
    if(camera&&!camera->destroyed&&camera->class_name=="Camera"&&is_descendant_of(w,*camera,workspace.id)) {
        sync_camera_viewport(w,*camera);return camera;
    }
    set_current_camera(w,workspace,nullptr);
    return find_node(w,w.local_properties.at(workspace.id).at("CurrentCamera").at("$ref").get<std::string>());
}
void refresh_current_camera(World& w) {
    auto service=w.services.find("Workspace");if(service==w.services.end())return;
    auto* workspace=find_node(w,service->second);
    if(workspace&&!workspace->destroyed)ensure_current_camera(w,*workspace);
}
int destroy_instance(lua_State* L) {
    Handle* h=checked_handle(L,1);
    try{destroy_roots(*h->world,{h->node->id},L,true);refresh_current_camera(*h->world);}catch(const std::exception& e){luaL_error(L,"Destroy failed: %s",e.what());}
    return 0;
}
int clear_children(lua_State* L) {
    Handle* h=checked_handle(L,1);std::vector<std::string> roots;
    for(const auto& id:h->world->order)if(parent_id(*h->world,h->world->nodes.at(id))==h->node->id)roots.push_back(id);
    try{destroy_roots(*h->world,roots,L,true);refresh_current_camera(*h->world);}catch(const std::exception& e){luaL_error(L,"ClearAllChildren failed: %s",e.what());}
    return 0;
}
int remove_instance(lua_State* L) {
    auto* h=checked_handle(L,1);auto& world=*h->world;if(h->node->destroyed)return 0;
    auto children=children_index(world);std::vector<std::string> pending{h->node->id},order;
    while(!pending.empty()) {
        auto id=pending.back();pending.pop_back();order.push_back(id);
        for(const auto& child:children[id])pending.push_back(child);
    }
    // Unveil's legacy Remove detaches the entire tree and preserves listeners,
    // attributes and reparentability. It is deliberately distinct from Destroy.
    for(const auto& id:order) {
        auto* node=find_node(world,id);if(!node||node->destroyed)continue;
        lua_pushcfunction(L,instance_newindex,"Instance.__newindex");push_node(L,world,node);
        lua_pushliteral(L,"Parent");lua_pushnil(L);lua_call(L,3,0);
    }
    return 0;
}
int get_attribute(lua_State* L) {
    Handle* h=checked_handle(L,1);size_t length;const char* raw=luaL_checklstring(L,2,&length);std::string name(raw,length);
    try {
    auto value=h->node->attributes.lookup(name);
    if(h->world->trace_attribute_reads) {
        auto& world=*h->world;
        const bool present=value.has_value();
        Json read{{"instance_id",h->node->id.substr(0,128)},
                  {"attribute",name.substr(0,128)},{"attribute_truncated",name.size()>128},
                  {"attributes_known",h->node->attributes.known()},{"present",present},
                  {"result_type",present?value->type_name():h->node->attributes.known()?"nil":"unavailable"},
                  {"sequence",++world.attribute_read_sequence}};
        world.attribute_reads.emplace_back(L,std::move(read));
        if(world.attribute_reads.size()>128)world.attribute_reads.pop_front();
    }
    if(value)push_json(L,*h->world,*value);
    else if(h->node->attributes.known())lua_pushnil(L);
    else throw std::runtime_error("attribute data is unavailable in this received snapshot");
    return 1;
    }catch(const std::exception& e){luaL_error(L,"GetAttribute failed: %s",e.what());return 0;}
}
int get_attributes(lua_State* L) {
    Handle* h=checked_handle(L,1);
    try {
    if(!h->node->attributes.known())throw std::runtime_error("complete attribute data is unavailable in this received snapshot");
    push_json(L,*h->world,h->node->attributes.snapshot());return 1;
    }catch(const std::exception& e){luaL_error(L,"GetAttributes failed: %s",e.what());return 0;}
}
int set_attribute(lua_State* L) {
    Handle* h=checked_handle(L,1);World& w=*h->world;size_t length;const char* raw=luaL_checklstring(L,2,&length);std::string name(raw,length);
    try {
        if(name.empty()||name.size()>100||name.rfind("RBX",0)==0)throw std::runtime_error("invalid or reserved attribute name");
        for(unsigned char c:name)if(!((c>='a'&&c<='z')||(c>='A'&&c<='Z')||(c>='0'&&c<='9')||c=='_'||c=='.'||c=='/'||c=='-'))throw std::runtime_error("invalid attribute name");
        int type=lua_type(L,3);
        if(type==LUA_TUSERDATA&&std::strcmp(luaL_typename(L,3),"CatalogSearchParams")==0)
            throw std::runtime_error("CatalogSearchParams cannot be stored as an Instance attribute");
        if(type==LUA_TUSERDATA&&is_rotation_curve_key(L,3))
            throw std::runtime_error("RotationCurveKey cannot be stored as an Instance attribute");
        if(type==LUA_TUSERDATA&&legacy_value_bindings::is_value(L,3))
            throw std::runtime_error("legacy value types cannot be stored as an Instance attribute");
        if(type!=LUA_TNIL && type!=LUA_TBOOLEAN && type!=LUA_TNUMBER && type!=LUA_TSTRING && type!=LUA_TVECTOR
            && !(type==LUA_TUSERDATA&&(has_meta(L,3,kCFrameMeta)||has_meta(L,3,kInstanceMeta)||has_meta(L,3,kInstanceHandleMeta)||
                (is_authored_value(L,3)&&std::string(luaL_typename(L,3))!="Axes"&&std::string(luaL_typename(L,3))!="Faces"&&std::string(luaL_typename(L,3))!="DateTime"&&std::string(luaL_typename(L,3))!="Ray"&&std::string(luaL_typename(L,3))!="TweenInfo"&&std::string(luaL_typename(L,3))!="Region3"&&std::string(luaL_typename(L,3))!="PhysicalProperties"&&std::string(luaL_typename(L,3)).find("Keypoint")==std::string::npos&&std::string(luaL_typename(L,3)).find("int16")==std::string::npos))))throw std::runtime_error("unsupported attribute value type");
        if(!h->node->attributes.contains(name)&&h->node->attributes.size()>=1024)throw std::runtime_error("local attribute count limit exceeded");
        Json value=convert_lua_value(L,w,3);
        if(type==LUA_TUSERDATA&&has_meta(L,3,kInstanceMeta))value=Json{{"$instance_handle",value.at("$ref")}};
        std::vector<Json> mutations{Json{{"kind","attribute"},{"id",h->node->id},{"name",name},{"value",value}}};
        check_mutation_budget(w,mutations);
        const bool changed=h->node->attributes.set(name,value);
        append_mutations(w,std::move(mutations));
        if(changed){emit(w,*h->node,"AttributeChanged",Json::array({name}));emit(w,*h->node,"attribute:"+name);}
    }catch(const std::exception& e){luaL_error(L,"SetAttribute failed: %s",e.what());return 0;}
    return 0;
}
bool reflected_property_known(const World& world,const Node& node,const std::string& name) {
    std::string cls=node.class_name;
    for(size_t depth=0;depth<128&&!cls.empty();++depth) {
        const auto properties=world.class_property_types.find(cls);
        if(properties!=world.class_property_types.end()&&properties->is_object()&&properties->contains(name))return true;
        const auto parent=world.class_parents.find(cls);
        if(parent==world.class_parents.end()||!parent->is_string())break;
        cls=parent->get<std::string>();
    }
    return false;
}
int property_changed_signal(lua_State* L) {
    auto* h=checked_handle(L,1);std::string name=canonical_instance_member(luaL_checkstring(L,2),h->node);
    if(h->node->class_name=="BodyForce"&&name=="force")name="Force";
    if(h->node->class_name=="BodyPosition") {
        if(name=="position")name="Position";
        if(name=="maxForce")name="MaxForce";
    }
    if(h->node->class_name=="BodyVelocity") {
        if(name=="velocity")name="Velocity";
        if(name=="maxForce")name="MaxForce";
    }
    // Replication can declare a property before its first value arrives. The
    // declaration is sufficient to subscribe; reads still require real state.
    if(name!="Name"&&name!="Parent"&&name!="ClassName"&&!(h->node->class_name=="Humanoid"&&name=="RootPart")&&!(h->node->class_name=="Workspace"&&name=="CurrentCamera")&&!property_descriptor(*h->node,name)&&!reflected_property_known(*h->world,*h->node,name)&&origin(*h->world,*h->node,name)=="unavailable"&&gui_property_type(*h->world,*h->node,name).empty()) {
        luaL_error(L,"unknown property for change signal: %s.%s",h->node->class_name.c_str(),name.c_str());return 0;
    }
    watch_display_property(*h->world,*h->node,name);
    watch_attachment_property(*h->world,*h->node,name);
    watch_joint_property(*h->world,*h->node,name);
    watch_assembly_root(*h->world,*h->node,name);
    watch_humanoid_root_part(*h->world,*h->node,name);
    watch_part_mass(*h->world,*h->node,name);
    try{push_signal(L,signal_for(*h->world,*h->node,"property:"+name));}catch(const std::exception& e){luaL_error(L,"signal unavailable: %s",e.what());}return 1;
}
int attribute_changed_signal(lua_State* L) {
    auto* h=checked_handle(L,1);size_t n;const char* name=luaL_checklstring(L,2,&n);
    try{push_signal(L,signal_for(*h->world,*h->node,"attribute:"+std::string(name,n)));}catch(const std::exception& e){luaL_error(L,"signal unavailable: %s",e.what());}return 1;
}
int disconnect_connection(lua_State* L) {
    auto* handle=static_cast<ConnectionHandle*>(luaL_checkudata(L,1,kConnectionMeta));
    if(handle->foreign_state) {
        if(handle->actor_runtime)
            handle->actor_runtime->disconnect_connection(handle->owner_actor_id,handle->connection_id,
                                                        handle->generation);
        return 0;
    }
    auto* connection=handle->waiter_proxy?resolve_wait_connection(*handle):handle->connection;
    if(!connection)return 0;
    connection->connected=false;connection->manually_disconnected=true;
    if(connection->signal_waiter&&connection->signal) {
        auto& world=*connection->signal->world;
        prune_signal_waiters(world,connection->signal);
    }
    return 0;
}
int connection_index(lua_State* L) {
    auto* handle=static_cast<ConnectionHandle*>(luaL_checkudata(L,1,kConnectionMeta));const char* key=luaL_checkstring(L,2);
    if(handle->foreign_state) {
        ActorRuntime::ConnectionView state;
        const bool live=handle->actor_runtime&&handle->actor_runtime->connection_state(
            handle->owner_actor_id,handle->connection_id,handle->generation,state);
        if(std::strcmp(key,"Connected")==0){lua_pushboolean(L,live&&state.connected);return 1;}
        if(connection_handle_compat_index(L,*handle,key))return 1;
    } else {
        auto* connection=handle->waiter_proxy?resolve_wait_connection(*handle):handle->connection;
        if(std::strcmp(key,"Connected")==0){lua_pushboolean(L,connection&&connection->connected);return 1;}
        if(connection&&connection_compat_index(L,*connection,key))return 1;
        if(handle->waiter_proxy&&connection_handle_compat_index(L,*handle,key))return 1;
    }
    if(std::strcmp(key,"Disconnect")==0||std::strcmp(key,"disconnect")==0){lua_pushcfunction(L,disconnect_connection,"Disconnect");return 1;}
    luaL_error(L,"unknown RBXScriptConnection member");return 0;
}
int connect_signal(lua_State* L) {
    auto* h=static_cast<SignalHandle*>(luaL_checkudata(L,1,kSignalMeta));luaL_checktype(L,2,LUA_TFUNCTION);
    auto& w=*h->signal->world;
    if(w.connections.size()>=2048)prune_signal_waiters(w);
    prune_disconnected_connections(L,w);
    if(active_connection_count(w)>=2048){luaL_error(L,"local connection count limit exceeded");return 0;}
    auto connection=std::make_shared<Connection>();connection->signal=h->signal;
    connection->id=w.next_connection_id++;
    connection->generation=w.connection_generation;
    lua_pushthread(L);connection->creator_ref=lua_ref(L,-1);lua_pop(L,1);
    connection->callback=lua_ref(L,2);connection->once=lua_toboolean(L,lua_upvalueindex(1));
    if(w.scheduler)connection->owner=w.scheduler->owner(L);
    connection->identity=SourceScheduler::identity(L);
    auto* pointer=connection.get();w.connections.push_back(connection);
    auto* handle=new_connection_handle(L);handle->ownership=std::move(connection);
    handle->connection=pointer;handle->connection_id=pointer->id;handle->generation=pointer->generation;
    return 1;
}
int wait_signal(lua_State* L) {
    auto* h=static_cast<SignalHandle*>(luaL_checkudata(L,1,kSignalMeta));auto& w=*h->signal->world;
    if(!w.scheduler||!lua_isyieldable(L)){luaL_error(L,"signal Wait requires a scheduled yieldable source thread");return 0;}
    // Manual task.spawn can resume a coroutine whose previous signal waiter
    // remains armed. Mark its new wait before pruning the old registrations.
    try{w.scheduler->wait_signal(L);}catch(const std::exception& e){luaL_error(L,"signal wait failed: %s",e.what());return 0;}
    // Cancellation need not be followed by another event on the old signal.
    // Reclaim those registrations before they can exhaust the shared cap.
    prune_signal_waiters(w);
    prune_disconnected_connections(L,w);
    auto& waiters=w.waiters[h->signal];if(waiters.size()>=2048||active_connection_count(w)>=2048){luaL_error(L,"signal waiter limit exceeded");return 0;}
    auto connection=std::make_shared<Connection>();connection->signal=h->signal;
    connection->id=w.next_connection_id++;
    connection->generation=w.connection_generation;
    connection->signal_waiter=true;
    connection->creator_thread=L;
    if(w.scheduler)connection->owner=w.scheduler->owner(L);
    connection->identity=SourceScheduler::identity(L);
    lua_pushthread(L);connection->creator_ref=lua_ref(L,-1);lua_pop(L,1);
    auto* pointer=connection.get();w.connections.push_back(std::move(connection));
    lua_pushthread(L);int reference=lua_ref(L,-1);lua_pop(L,1);
    waiters.push_back(SignalWaiter{L,reference,pointer});
    return lua_yield(L,0);
}
int wait_for_child(lua_State* L) {
    auto* h=checked_handle(L,1);World& w=*h->world;std::string name=luaL_checkstring(L,2);
    using Clock=std::chrono::steady_clock;
    auto deadline=Clock::time_point::max();
    if(!lua_isnoneornil(L,3)) {
        double duration=luaL_checknumber(L,3);
        if(std::isnan(duration)||duration<=0){luaL_error(L,"timeOut must be greater than 0");return 0;}
        auto remaining=std::chrono::duration<double>(Clock::time_point::max()-Clock::now()).count();
        if(duration<remaining)deadline=Clock::now()+std::chrono::duration_cast<Clock::duration>(std::chrono::duration<double>(duration));
    }
    for(const auto& id:w.order){Node& child=w.nodes.at(id);if(parent_id(w,child)==h->node->id&&instance_name(w,child)==name){push_node(L,w,&child);return 1;}}
    if(!w.scheduler||!lua_isyieldable(L)){luaL_error(L,"WaitForChild requires a scheduled yieldable thread");return 0;}
    if(w.child_waiters.size()>=2048){luaL_error(L,"WaitForChild waiter limit exceeded");return 0;}
    try{w.scheduler->wait_signal(L);}catch(const std::exception& e){luaL_error(L,"WaitForChild failed: %s",e.what());return 0;}
    lua_pushthread(L);int reference=lua_ref(L,-1);lua_pop(L,1);
    w.child_waiters.push_back(ChildWaiter{L,reference,h->node->id,name,deadline});return lua_yield(L,0);
}
int chat_continue(lua_State* L,const char* method) {
    if(lua_gettop(L)==1&&has_meta(L,1,kInstanceMeta)) {
        auto* h=checked_handle(L,1);if(h->node->class_name=="TextChatMessage")return 1;
    }
    if(lua_gettop(L)==2&&lua_isnil(L,1)&&lua_type(L,2)==LUA_TSTRING){luaL_error(L,"%s failed: %s",method,lua_tostring(L,2));return 0;}
    luaL_error(L,"%s resumed without a matching chat response",method);return 0;
}
int send_chat_continue(lua_State* L,int){return chat_continue(L,"SendAsync");}
int send_universe_chat_continue(lua_State* L,int){return chat_continue(L,"SendUniverseChatMessageAsync");}
int send_chat_request(lua_State* L,bool universe) {
    auto* h=checked_handle(L,1);World& w=*h->world;
    if(universe) {
        if(h->node->class_name!="TextChatService"){luaL_error(L,"SendUniverseChatMessageAsync requires TextChatService");return 0;}
    } else if(h->node->class_name!="TextChannel"){luaL_error(L,"SendAsync requires TextChannel");return 0;}
    const char* method=universe?"SendUniverseChatMessageAsync":"SendAsync";
    if(!w.persistent||!w.chat_enabled||!w.scheduler){luaL_error(L,"%s requires the persistent runtime with --chat-console enabled",method);return 0;}
    if(!lua_isyieldable(L)){luaL_error(L,"%s requires a yieldable source thread",method);return 0;}
    if(lua_gettop(L)<2||lua_gettop(L)>3){luaL_error(L,"%s expects message and optional metadata",method);return 0;}
    try {
        auto service=w.services.find("TextChatService");
        if(universe) {
            if(h->node->destroyed||!h->node->server_owned||service==w.services.end()
               ||service->second!=h->node->id||find_node(w,h->node->id)!=h->node)
                throw std::runtime_error("receiver must be the live server-replicated TextChatService");
        } else {
            if(h->node->destroyed||!h->node->server_owned)throw std::runtime_error("channel must be a live server-replicated TextChannel");
            if(service==w.services.end()||!is_descendant_of(w,*h->node,service->second))throw std::runtime_error("TextChannel is not a descendant of TextChatService");
        }
        std::string values[2];
        for(int i=0;i<2;i++) {
            int arg=i+2;if(i==1&&lua_isnoneornil(L,arg))continue;
            if(lua_type(L,arg)!=LUA_TSTRING)throw std::runtime_error("message and metadata must be strings");
            size_t length=0;const char* data=lua_tolstring(L,arg,&length);size_t characters=0;
            if(length>800||!valid_utf8(data,length)||std::memchr(data,0,length))throw std::runtime_error("message or metadata is not supported UTF-8 text");
            for(size_t j=0;j<length;j++)if((static_cast<unsigned char>(data[j])&0xc0)!=0x80)++characters;
            if(characters>200)throw std::runtime_error("message and metadata support at most 200 characters");
            values[i].assign(data,length);
        }
        if(values[0].empty())throw std::runtime_error("message must not be empty");
        if(w.chat_waiters.size()>=8||w.chat_messages+w.chat_waiters.size()>=256)throw std::runtime_error("chat request or message quota exceeded");
        uint64_t request=w.next_chat_request++;
        Json message{{"request_id",request},{"channel_id",h->node->id},{"text",values[0]},{"metadata",values[1]}};
        if(universe)message["route"]="universe";
        w.scheduler->wait_signal(L);
        lua_pushthread(L);int reference=lua_ref(L,-1);lua_pop(L,1);
        w.chat_waiters.push_back(ChatWaiter{L,reference,request,h->node->id,std::chrono::steady_clock::now()+std::chrono::seconds(20),universe});
        w.chat_requests.push_back(std::move(message));
    }catch(const std::exception& e){luaL_error(L,"%s failed: %s",method,e.what());return 0;}
    lua_settop(L,0);return lua_yield(L,0);
}
int send_chat(lua_State* L){return send_chat_request(L,false);}
int send_universe_chat(lua_State* L){return send_chat_request(L,true);}
int signal_index(lua_State* L) {
    luaL_checkudata(L,1,kSignalMeta);const char* key=luaL_checkstring(L,2);
    if(std::strcmp(key,"Connect")==0||std::strcmp(key,"connect")==0||std::strcmp(key,"Once")==0) {
        lua_pushboolean(L,std::strcmp(key,"Once")==0);lua_pushcclosurek(L,connect_signal,"Connect",1,nullptr);return 1;
    }
    if(std::strcmp(key,"Wait")==0||std::strcmp(key,"wait")==0){lua_pushcfunction(L,wait_signal,"Wait");return 1;}
    luaL_error(L,"unknown RBXScriptSignal member");return 0;
}
void copy_bindable_value(lua_State* L,int index,std::unordered_set<const void*>& path,size_t& budget,int depth=0) {
    if(depth>32||budget==0)throw std::runtime_error("BindableEvent table copy limit exceeded");
    --budget;index=lua_absindex(L,index);int type=lua_type(L,index);
    if(type==LUA_TTABLE) {
        const void* identity=lua_topointer(L,index);
        if(!path.insert(identity).second)throw std::runtime_error("cyclic BindableEvent tables are unsupported");
        lua_newtable(L);int output=lua_gettop(L);
        lua_rawgeti(L,index,1);bool array=!lua_isnil(L,-1);lua_pop(L,1);
        if(array) {
            for(int i=1;;i++) {
                lua_rawgeti(L,index,i);if(lua_isnil(L,-1)){lua_pop(L,1);break;}
                copy_bindable_value(L,-1,path,budget,depth+1);lua_rawseti(L,output,i);lua_pop(L,1);
            }
        } else {
            lua_pushnil(L);
            while(lua_next(L,index)) {
                int value=lua_gettop(L);
                if(lua_type(L,-2)==LUA_TSTRING)lua_pushvalue(L,-2);
                else if(lua_type(L,-2)==LUA_TNUMBER) {
                    double key=lua_tonumber(L,-2);
                    if(!std::isfinite(key)||key<INT32_MIN||key>INT32_MAX)throw std::runtime_error("BindableEvent numeric key exceeds supported range");
                    std::string text=std::to_string(static_cast<int32_t>(key));lua_pushlstring(L,text.data(),text.size());
                } else throw std::runtime_error("invalid BindableEvent table key type");
                copy_bindable_value(L,value,path,budget,depth+1);lua_rawset(L,output);lua_pop(L,1);
            }
        }
        path.erase(identity);
    } else if(type==LUA_TBUFFER) {
        size_t size=0;const void* input=lua_tobuffer(L,index,&size);void* output=lua_newbuffer(L,size);
        if(size)std::memcpy(output,input,size);
    } else if(type==LUA_TUSERDATA&&is_authored_value(L,index)&&std::strcmp(luaL_typename(L,index),"Font")==0) {
        Json value;capture_authored_value(L,index,value);push_authored_value(L,value);
    } else if(type==LUA_TTHREAD || (type==LUA_TUSERDATA&&!has_meta(L,index,kInstanceMeta)&&!has_meta(L,index,kCFrameMeta)&&!has_meta(L,index,kInstanceHandleMeta)&&!has_meta(L,index,kChatStatusMeta)&&!has_meta(L,index,kPriorityMeta)&&!has_meta(L,index,kGeometryItemMeta)&&!has_meta(L,index,kUsiExportItemMeta)&&!has_meta(L,index,kUsiSecurityCapabilitiesMeta)&&!has_meta(L,index,kHumanoidStateItemMeta)&&!has_meta(L,index,kRaycastResultMeta)&&!has_meta(L,index,kRaycastParamsMeta)&&!has_meta(L,index,kOverlapParamsMeta)&&!is_authored_value(L,index)))lua_pushnil(L);
    else lua_pushvalue(L,index);
}
#include "bindable_functions.inc"
#include "remote_client_callbacks.inc"
int fire_bindable(lua_State* L) {
    auto* h=checked_handle(L,1);World& w=*h->world;
    if(h->node->class_name!="BindableEvent"){luaL_error(L,"BindableEvent receiver required");return 0;}
    int count=lua_gettop(L)-1;
    if(count>256||w.event_argument_refs.size()>=10000){luaL_error(L,"local BindableEvent argument limit exceeded");return 0;}
    // Table arguments are copied at Fire time, metatables are removed, and
    // cycles are rejected. Functions and trailing nils remain valid values.
    lua_createtable(L,count,0);
    try {
        size_t budget=10000;std::unordered_set<const void*> path;
        for(int i=0;i<count;i++){copy_bindable_value(L,i+2,path,budget);lua_rawseti(L,-2,i+1);}
    }catch(const std::exception& e){luaL_error(L,"BindableEvent Fire failed: %s",e.what());return 0;}
    auto found=w.signals.find(Json::array({h->node->id,"Event"}).dump());if(found==w.signals.end())return 0;
    auto* signal=found->second.get();bool listening=false;
    for(const auto& c:w.connections)if(c->signal==signal&&c->connected&&c->enabled&&(!c->once||!c->once_pending))listening=true;
    auto waiters=w.waiters.find(signal);if(waiters!=w.waiters.end()&&!waiters->second.empty())listening=true;
    if(!listening)return 0;
    int reference=lua_ref(L,-1);lua_pop(L,1);w.event_argument_refs.push_back(reference);
    const bool response_ready=w.scheduler&&w.scheduler->in_external_response_continuation(L);
    emit(w,*h->node,"Event",Json::array(),reference,count,response_ready);return 0;
}
int instance_handle_get(lua_State* L) {
    auto* h=static_cast<InstanceHandleValue*>(luaL_checkudata(L,1,kInstanceHandleMeta));
    auto* node=find_node(*h->world,h->id);if(node)push_node(L,*h->world,node);else lua_pushnil(L);return 1;
}
int instance_handle_wait(lua_State* L) {
    auto* h=static_cast<InstanceHandleValue*>(luaL_checkudata(L,1,kInstanceHandleMeta));
    using Clock=std::chrono::steady_clock;
    auto deadline=Clock::time_point::max();
    if(!lua_isnoneornil(L,2)) {
        double timeout=luaL_checknumber(L,2);
        if(!std::isfinite(timeout)||timeout<=0){luaL_error(L,"timeOut must be greater than 0");return 0;}
        auto remaining=std::chrono::duration<double>(Clock::time_point::max()-Clock::now()).count();
        if(timeout<remaining)deadline=Clock::now()+std::chrono::duration_cast<Clock::duration>(std::chrono::duration<double>(timeout));
    }
    if(h->id.empty()||find_node(*h->world,h->id))return instance_handle_get(L);
    World& w=*h->world;
    if(!w.scheduler||!lua_isyieldable(L)){luaL_error(L,"waiting for an unresolved InstanceHandle requires a scheduled yieldable thread");return 0;}
    if(w.handle_waiters.size()>=2048){luaL_error(L,"InstanceHandle waiter limit exceeded");return 0;}
    try{w.scheduler->wait_signal(L);}catch(const std::exception& e){luaL_error(L,"InstanceHandle wait failed: %s",e.what());return 0;}
    lua_pushthread(L);int reference=lua_ref(L,-1);lua_pop(L,1);
    w.handle_waiters.push_back(HandleWaiter{L,reference,h->id,deadline});return lua_yield(L,0);
}
int instance_handle_index(lua_State* L) {
    luaL_checkudata(L,1,kInstanceHandleMeta);const char* key=luaL_checkstring(L,2);
    if(std::strcmp(key,"Get")==0){lua_pushcfunction(L,instance_handle_get,"Get");return 1;}
    if(std::strcmp(key,"Wait")==0){lua_pushcfunction(L,instance_handle_wait,"Wait");return 1;}
    luaL_error(L,"unknown InstanceHandle member");return 0;
}
int instance_handle_new(lua_State* L) {
    auto* w=static_cast<World*>(lua_touserdata(L,lua_upvalueindex(1)));
    if(lua_isnoneornil(L,1))push_instance_handle(L,*w,"");
    else{auto* h=checked_handle(L,1);if(h->world!=w){luaL_error(L,"Instance belongs to a different snapshot");return 0;}push_instance_handle(L,*w,h->node->id);}
    return 1;
}
int instance_handle_equal(lua_State* L) {
    bool a=has_meta(L,1,kInstanceHandleMeta),b=has_meta(L,2,kInstanceHandleMeta);
    auto* x=static_cast<InstanceHandleValue*>(lua_touserdata(L,1));auto* y=static_cast<InstanceHandleValue*>(lua_touserdata(L,2));
    lua_pushboolean(L,a&&b&&x->world==y->world&&x->id==y->id);return 1;
}
int instance_handle_tostring(lua_State* L) {
    auto* h=static_cast<InstanceHandleValue*>(luaL_checkudata(L,1,kInstanceHandleMeta));auto* node=find_node(*h->world,h->id);
    std::string text=node?instance_name(*h->world,*node):"nil";lua_pushlstring(L,text.data(),text.size());return 1;
}
int instance_newindex(lua_State* L){
    Handle*h=checked_handle(L,1);World&w=*h->world;Node&n=*h->node;std::string key=canonical_instance_member(luaL_checkstring(L,2),&n);Json value;
    if(n.class_name=="Player") {
        if(key=="GameplayPaused"&&!SourceScheduler::has_capability(L,"RobloxScript"))return luaL_error(L,"GameplayPaused: lacking capability RobloxScript"),0;
        if((key=="Name"||key=="UserId")&&!SourceScheduler::has_capability(L,"WritePlayer"))return luaL_error(L,"Player identity: lacking capability WritePlayer"),0;
    }
    if(n.class_name=="BodyForce"&&key=="force")key="Force";
    if(n.class_name=="BodyPosition") {
        if(key=="position")key="Position";
        if(key=="maxForce")key="MaxForce";
    }
    if(n.class_name=="BodyVelocity") {
        if(key=="velocity")key="Velocity";
        if(key=="maxForce")key="MaxForce";
    }
    if(lua_toboolean(L,lua_upvalueindex(1)))key=hidden_property_name(n,key);else enforce_scriptability(L,w,n,key);
    if(n.class_name=="Humanoid"&&key=="RootPart")return luaL_error(L,"Humanoid.RootPart is read-only"),0;
    if(pivot_model(w,n)&&key=="Scale") {luaL_error(L,"Scale is not scriptable; use ScaleTo");return 0;}
    if((collision_world_root(n)||n.class_name=="PhysicsService")&&(key=="CollisionGroupData"||key=="CollisionGroups")) {
        luaL_error(L,"collision group definitions can only be changed by the server");return 0;
    }
    if(n.class_name=="Tween"&&(key=="PlaybackState"||key=="Instance"||key=="TweenInfo")) {
        luaL_error(L,"Tween property is read-only");return 0;
    }
    if(key=="Source"&&(n.class_name=="LocalScript"||n.class_name=="ModuleScript"||n.class_name=="Script")) {
        size_t length=0;const char* source=luaL_checklstring(L,3,&length);
        if(length>128*1024||n.destroyed){luaL_error(L,"invalid script source write");return 0;}
        n.script_state.code_mut()=Json{{"source",std::string(source,length)},{"authored_override",true}};property_events(w,n,"Source");return 0;
    }
    if(display_readonly_property(w,n,key)){luaL_error(L,"display property is read-only");return 0;}
    if(mouse_projection_property(w,n,key)){luaL_error(L,"Mouse property is read-only");return 0;}
    if(n.class_name=="UserInputService"&&(input_capability_key(key)||key=="PreferredInput")){luaL_error(L,"input capability is read-only");return 0;}
    if(key=="ClassName" || key=="JobId" || (n.class_name=="TextChatMessage" && key!="Name" && key!="Parent")) {luaL_error(L,"property is read-only");return 0;}
    try{
        validate_text_assignment(L,w,n,key);
        if(n.class_name=="SpecialMesh"&&key=="MeshType") {
            Json item;
            if(!capture_ussi_export_enum(L,3,item)||item["$enum"]["type"]!="MeshType")
                throw std::runtime_error("MeshType requires Enum.MeshType");
        }
        if(textbox_selection_write(L,w,n,key))return 0;
        if(sound_write(L,w,n,key))return 0;
        if(text_font_write(L,w,n,key))return 0;
        if(n.class_name=="VectorForce"&&key=="RelativeTo")checked_geometry_value(L,3,kActuatorRelativeTo);
        if(n.class_name=="LinearVelocity") {
            int kind=linear_velocity_enum_kind(key);if(kind>=0)checked_geometry_value(L,3,kind);
        }
        if(n.class_name=="BindableFunction"&&key=="OnInvoke"){set_bindable_callback(L,w,n);return 0;}
        if(n.class_name=="RemoteFunction"&&key=="OnClientInvoke"){set_bindable_callback(L,w,n);return 0;}
        if(input_write(L,w,n,key))return 0;
        if(virtual_input_write(L,w,n,key))return 0;
        if(collision_group_write(L,w,n,key))return 0;
        if(humanoid_state_write(L,w,n,key))return 0;
        if(humanoid_health_write(L,w,n,key))return 0;
        if(humanoid_jump_write(L,w,n,key))return 0;
        if(joint_pose_write(L,w,n,key))return 0;
        if(joint_part_write(L,w,n,key))return 0;
        validate_joint_assignment(L,w,n,key);
        validate_no_collision_assignment(L,w,n,key);
        if(part_transform_write(L,w,n,key))return 0;
        if(part_mass_enum_write(L,w,n,key))return 0;
        if(attachment_write(L,w,n,key))return 0;
        if(n.class_name=="Camera"&&key=="ViewportSize")throw std::runtime_error("ViewportSize is read-only");
        if(pivot_model(w,n)&&key=="PrimaryPart"&&!lua_isnil(L,3)) {
            auto* primary=checked_handle(L,3);
            if(primary->world!=&w||!pivot_part(w,*primary->node))throw std::runtime_error("PrimaryPart must be a BasePart");
        }
        if(animation_newindex(L,w,n,key))return 0;
        if(n.class_name=="Workspace"&&key=="CurrentCamera") {
            Node* camera=nullptr;
            if(!lua_isnil(L,3)) {
                auto* selected=checked_handle(L,3);
                if(selected->world!=&w)throw std::runtime_error("camera belongs to a different snapshot");
                camera=selected->node;
            }
            set_current_camera(w,n,camera);return 0;
        }
        if(key=="Parent"){
            if(n.destroyed) {
                if(lua_isnil(L,3)&&parent_id(w,n).empty())return 0;
                throw std::runtime_error("Parent property is locked after Destroy");
            }
            if(lua_isnil(L,3))value=nullptr;
            else {Handle*p=checked_handle(L,3);if(p->world!=&w)throw std::runtime_error("parent belongs to a different snapshot");if(p->node==&n||is_descendant_of(w,*p->node,n.id))throw std::runtime_error("parent change would create a cycle");value=Json{{"$ref",p->node->id}};}
        } else if(key=="Name") {size_t len;const char*s=luaL_checklstring(L,3,&len);if(len>256)throw std::runtime_error("Name exceeds 256 bytes");value=std::string(s,len);}
        else {
            const bool gui_assignment=validate_gui_assignment(L,w,n,key);
            const bool explicit_pivot=(pivot_model(w,n)&&(key=="WorldPivot"||key=="PrimaryPart"))||(pivot_part(w,n)&&key=="PivotOffset");
            // A missing received value does not make a known engine property
            // unwritable. Validate against the pinned descriptor below and
            // store only a local override; unknown properties remain errors.
            if(!property_descriptor(n,key)&&!gui_assignment&&!explicit_pivot&&!humanoid_jump_setting(n,key)&&!(n.class_name=="Humanoid"&&(key=="WalkSpeed"||key=="AutoRotate"))&&origin(w,n,key)=="unavailable")throw std::runtime_error("property absent from snapshot; local creation is unsupported");
            if(const Json* descriptor=property_descriptor(n,key=="archivable"?"Archivable":key)) {
                if(!descriptor->value("writable",false))throw std::runtime_error("property is read-only");
                const std::string type=descriptor->value("type","");
                if(type=="boolean" && lua_type(L,3)!=LUA_TBOOLEAN)throw std::runtime_error("property must be a boolean");
                if(type=="string" && lua_type(L,3)!=LUA_TSTRING)throw std::runtime_error("property must be a string");
                if(type=="number" && lua_type(L,3)!=LUA_TNUMBER)throw std::runtime_error("property must be a number");
                if(type=="Enum"&&descriptor->contains("enum_type")) {
                    Json item;
                    if(!capture_ussi_export_enum(L,3,item)||item["$enum"]["type"]!=descriptor->at("enum_type"))throw std::runtime_error("property has incorrect Enum type");
                }
                if(type=="Vector3" && !lua_isvector(L,3))throw std::runtime_error("property must be a Vector3");
                if(type=="CFrame" && !has_meta(L,3,kCFrameMeta))throw std::runtime_error("property must be a CFrame");
                if((type=="Font"||type=="Axes"||type=="Faces"||type=="Color3"||type=="Vector2"||type=="UDim"||type=="UDim2"||type=="NumberRange"||type=="Rect"||type=="BrickColor"||type=="NumberSequence"||type=="ColorSequence")&&
                    (!is_authored_value(L,3)||type!=luaL_typename(L,3)))throw std::runtime_error("property has incorrect datatype");
                if(type=="PhysicalProperties"&&!lua_isnil(L,3)&&(!is_authored_value(L,3)||std::string(luaL_typename(L,3))!="PhysicalProperties"))throw std::runtime_error("property must be PhysicalProperties or nil");
                if(type=="Instance" && !lua_isnil(L,3) && !has_meta(L,3,kInstanceMeta))throw std::runtime_error("property must be an Instance or nil");
            }
            if(key=="Anchored"&&lua_type(L,3)!=LUA_TBOOLEAN)throw std::runtime_error("Anchored must be a boolean");
            if((key=="Position"||key=="CFrame")&&class_is_a(w,n.class_name,"BasePart")) {
                if(key=="Position"&&!lua_isvector(L,3))throw std::runtime_error("Position must be a Vector3");
                if(key=="CFrame"&&!has_meta(L,3,kCFrameMeta))throw std::runtime_error("CFrame must be a CFrame");
            }
            value=convert_lua_value(L,w,3);
            if(n.class_name=="ScrollingFrame"&&key=="CanvasPosition") {
                GuiLayoutPass pass{w};auto metrics=pass.scroll(n,pass.box(n),&value);
                value=display_vector(metrics.x,metrics.y);
            }
            if(humanoid_jump_setting(n,key)&&key!="UseJumpPower") {
                if(!value.is_number()||!std::isfinite(value.get<double>()))throw std::runtime_error("jump setting must be finite");
                value=std::max(0.0,value.get<double>());
                if(key=="JumpPower")value=std::min(1000.0,value.get<double>());
            }
            if(n.class_name=="Camera"&&key=="FieldOfView") {
                if(!value.is_number()||!std::isfinite(value.get<double>()))throw std::runtime_error("FieldOfView must be finite");
                value=std::clamp(value.get<double>(),1.0,120.0);
            }
        }
        auto old_parent=parent_id(w,n);
        Json old_value=key=="Name"?Json(instance_name(w,n)):property_value(w,n,key)?*property_value(w,n,key):Json(nullptr);
        const bool old_script_disabled=script_disabled_state(w,n);
        const bool changes_content=text_layout_node(n)&&(key=="Text"||key=="RichText");
        const auto old_content=changes_content?text_content(w,n):std::nullopt;
        if(key=="WorldPivot"&&pivot_model(w,n)) {
            try{old_value=pivot_encode(model_world_pivot(w,n));}catch(const std::exception&){} // Old geometry may be unavailable.
        }
        std::vector<Json> entries;
        entries.push_back(Json{{"kind","set"},{"id",n.id},{"property",key},{"value",value}});
        std::vector<std::pair<Node*,Json>> motion_members;
        std::string motion_root_id;
        if(key=="AssemblyLinearVelocity"&&pivot_part(w,n)) {
            auto targets=character_velocity_write_targets(w,n);
            if(!targets.empty())motion_root_id=targets.front()->id;
            for(auto* member:targets)if(member!=&n) {
                const auto* previous=property_value(w,*member,key);
                motion_members.emplace_back(member,previous?*previous:Json(nullptr));
                entries.push_back(Json{{"kind","set"},{"id",member->id},{"property",key},{"value",value}});
            }
        }
        std::vector<std::pair<std::string,Json>> derived;
        if(n.class_name=="ScreenGui"&&key=="IgnoreGuiInset") {
            if(!value.is_boolean())throw std::runtime_error("IgnoreGuiInset must be a boolean");
            auto* current=property_value(w,n,"ScreenInsets");int mode=current&&current->is_number_integer()?current->get<int>():2;
            if(!value.get<bool>()||mode==2) {
                Json inset=value.get<bool>()?1:2;
                derived.emplace_back("ScreenInsets",inset);entries.push_back(Json{{"kind","set"},{"id",n.id},{"property","ScreenInsets"},{"value",inset}});
            }
        }
        if(n.class_name=="Camera"&&(key=="CFrame"||key=="CoordinateFrame"||key=="Focus"||key=="focus")) {
            std::string alias=key=="CFrame"?"CoordinateFrame":key=="CoordinateFrame"?"CFrame":key=="Focus"?"focus":"Focus";
            derived.emplace_back(alias,value);entries.push_back(Json{{"kind","set"},{"id",n.id},{"property",alias},{"value",value}});
        }
        if((n.class_name=="LocalScript"||n.class_name=="Script")&&(key=="Enabled"||key=="Disabled")) {
            std::string alias=key=="Enabled"?"Disabled":"Enabled";Json inverse=!value.get<bool>();
            derived.emplace_back(alias,inverse);entries.push_back(Json{{"kind","set"},{"id",n.id},{"property",alias},{"value",inverse}});
        }
        if(key=="Archivable"||key=="archivable") {
            std::string alias=key=="Archivable"?"archivable":"Archivable";
            derived.emplace_back(alias,value);entries.push_back(Json{{"kind","set"},{"id",n.id},{"property",alias},{"value",value}});
        }
        size_t bytes=0;for(const auto&entry:entries)bytes+=entry.dump().size();
        if(w.mutations.size()+entries.size()>kMaxMutations||bytes>kMaxMutationBytes-w.mutation_bytes)throw std::runtime_error("local mutation output limit exceeded");
        // Actor writes are applied to the root through their ordered mutation
        // journal; that root application queues the physical command once.
        // Replica replay must not enqueue a second command in the Actor VM,
        // whose private movement queue has no host consumer.
        if(key=="AssemblyLinearVelocity"&&class_is_a(w,n.class_name,"BasePart")&&w.actor_id.empty()) {
            movement_bindings::enqueue_linear_velocity(L,n.id,3,motion_root_id);
            movement_bindings::tag_latest_mutation_cursor(L,static_cast<int>(w.mutations.size()));
        }
        w.local_properties[n.id][key]=value;
        for(const auto& [member,previous]:motion_members)w.local_properties[member->id][key]=value;
        body_position_written(w,n,key);
        for(auto&[derived_key,derived_value]:derived)w.local_properties[n.id][derived_key]=derived_value;
        if((key=="Disabled"||key=="Enabled")&&(n.class_name=="Script"||n.class_name=="LocalScript"))
            notify_script_disabled_transition(L,w,n,old_script_disabled);
        for(auto&entry:entries){w.mutation_bytes+=entry.dump().size();w.mutations.push_back(std::move(entry));}
        if(key=="Parent")parent_events(w,n,old_parent,parent_id(w,n));
        else if(old_value!=value&&!(n.class_name=="ScrollingFrame"&&key=="CanvasPosition"))property_events(w,n,key);
        if(changes_content&&old_content!=text_content(w,n))property_events(w,n,"ContentText");
        if(key=="Text"&&n.class_name=="TextBox"&&old_value!=value)authored_textbox_text_changed(w,n);
        for(const auto& [member,previous]:motion_members)if(previous!=value)property_events(w,*member,key);
        for(auto&[derived_key,derived_value]:derived)if(derived_key!="archivable"&&derived_key!="Archivable")property_events(w,n,derived_key);
        if(key=="Parent")refresh_current_camera(w);
    }catch(const std::exception&e){luaL_error(L,"local property write failed for %s.%s: %s",n.class_name.c_str(),key.c_str(),e.what());return 0;}
    return 0;
}
#include "smooth_damp.inc"
#include "tween_playback.inc"
int instance_new(lua_State*L){
    auto*w=static_cast<World*>(lua_touserdata(L,lua_upvalueindex(1)));
    if(lua_gettop(L)<1||lua_gettop(L)>2){luaL_error(L,"Instance.new expects a class name and optional Parent");return 0;}
    size_t class_len=0;const char*raw=luaL_checklstring(L,1,&class_len);std::string cls(raw,class_len);
    const Json* constructor=cached_constructor_class(cls);
    if(cls=="Player") {
        if(!SourceScheduler::has_capability(L,"WritePlayer"))return luaL_error(L,"Player construction: lacking capability WritePlayer"),0;
        // A local Player has no server identity or membership until explicitly
        // parented; creating it never changes the authenticated LocalPlayer.
        static const Json player{{"name","Player"},{"creatable",true},{"properties",{
            {"Archivable",{{"value",true}}},{"UserId",{{"value",0}}},
            {"Character",{{"value",nullptr}}},{"GameplayPaused",{{"value",false}}}}}};
        constructor=&player;
    }
    if(!constructor || !constructor->value("creatable",false)){luaL_error(L,"class '%s' is not a supported creatable class",cls.c_str());return 0;}
    Handle*parent=nullptr;
    if(lua_gettop(L)==2&&!lua_isnil(L,2)){parent=checked_handle(L,2);if(parent->world!=w){luaL_error(L,"Parent belongs to a different world snapshot");return 0;}}
    if(w->nodes.size()>=kMaxNodes+1000){luaL_error(L,"local instance limit exceeded");return 0;}
    Node node;node.attributes.set_known(true);node.collection_tags_known=true;node.server_owned=false;node.id=w->allocate_local_id();node.class_name=cls;node.name=constructor->at("name").get<std::string>();
    for(auto property=constructor->at("properties").begin();property!=constructor->at("properties").end();++property) {
        node.properties[property.key()]=property.value().at("value");
        node.origins[property.key()]="class_default";
    }
    if(const auto* field=property_descriptor(node,"formFactorRaw");field&&field->contains("value")) {
        node.properties["formFactorRaw"]=field->at("value");
        node.origins["formFactorRaw"]="class_default";
    }
    if(cls=="Player")for(const char* key:{"CameraMinZoomDistance","CameraMaxZoomDistance","DevEnableMouseLock",
            "CameraMode","DevCameraOcclusionMode","DevComputerCameraMode","DevTouchCameraMode",
            "DevComputerMovementMode","DevTouchMovementMode"}) {
        node.properties[key]=player_constructor_property(node,key)->at("value");
        node.origins[key]="class_default";
    }
    std::string id=node.id;
    if(cls=="VectorForce"){node.properties["RelativeTo"]=0;node.origins["RelativeTo"]="class_default";}
    if(cls=="Script") {
        const auto* run_context=property_descriptor(node,"RunContext");
        node.properties["RunContext"]=run_context->at("value");
        node.origins["RunContext"]="class_default";
    }
    if(cls=="LinearVelocity") {
        Json defaults={{"RelativeTo",2},{"VelocityConstraintMode",2},{"ForceLimitMode",0},
            {"PlaneVelocity",{{"$vector2",{0,0}}}},{"MaxPlanarAxesForce",{{"$vector2",{1000,1000}}}}};
        for(auto it=defaults.begin();it!=defaults.end();++it){node.properties[it.key()]=it.value();node.origins[it.key()]="class_default";}
    }
    auto [it,ok]=w->nodes.emplace(id,std::move(node));if(!ok){luaL_error(L,"local instance id collision");return 0;}
    w->order.push_back(id);w->local_properties[id]["Name"]=it->second.name;w->local_properties[id]["Parent"]=nullptr;
    Json created{{"kind","create"},{"node",node_record(it->second)}};
    size_t create_bytes=created.dump().size(),parent_bytes=0;Json parent_entry;
    if(parent){parent_entry=Json{{"kind","set"},{"id",id},{"property","Parent"},{"value",{{"$ref",parent->node->id}}}};parent_bytes=parent_entry.dump().size();}
    size_t total_bytes=create_bytes+parent_bytes;
    if(w->mutations.size()+(parent?2:1)>kMaxMutations||total_bytes>kMaxMutationBytes-w->mutation_bytes){w->nodes.erase(id);w->local_properties.erase(id);w->order.pop_back();luaL_error(L,"local mutation output limit exceeded");return 0;}
    w->mutations.push_back(std::move(created));w->mutation_bytes+=create_bytes;
    push_node(L,*w,&it->second);
    if(parent){Json value{{"$ref",parent->node->id}};w->local_properties[id]["Parent"]=value;w->mutations.push_back(std::move(parent_entry));w->mutation_bytes+=parent_bytes;parent_events(*w,it->second,"",parent->node->id);}
    return 1;
}
int instance_tostring(lua_State*L){Handle*h=checked_handle(L,1);auto s=instance_name(*h->world,*h->node);lua_pushlstring(L,s.data(),s.size());return 1;}
int instance_equal(lua_State*L){auto*a=static_cast<Handle*>(lua_touserdata(L,1));auto*b=static_cast<Handle*>(lua_touserdata(L,2));bool aa=has_meta(L,1,kInstanceMeta),bb=has_meta(L,2,kInstanceMeta);lua_pushboolean(L,aa&&bb&&a&&b&&a==b);return 1;}
int cframe_tostring(lua_State*L){auto*c=static_cast<CFrameValue*>(luaL_checkudata(L,1,kCFrameMeta));lua_pushfstring(L,"%f, %f, %f",c->p[0],c->p[1],c->p[2]);return 1;}
int cframe_equal(lua_State*L){auto*a=static_cast<CFrameValue*>(lua_touserdata(L,1));auto*b=static_cast<CFrameValue*>(lua_touserdata(L,2));bool aa=has_meta(L,1,kCFrameMeta),bb=has_meta(L,2,kCFrameMeta);bool eq=aa&&bb&&a&&b&&a->rotation_known==b->rotation_known&&a->p==b->p&&a->r==b->r;lua_pushboolean(L,eq);return 1;}
int print_capture(lua_State*L){World*w=static_cast<World*>(lua_touserdata(L,lua_upvalueindex(1)));bool warn=lua_toboolean(L,lua_upvalueindex(2));size_t room=w->output.size()<kMaxPrinted?kMaxPrinted-w->output.size():0;auto append=[&](const char*s,size_t n){size_t take=std::min(n,room);if(take)w->output.append(s,take);room-=take;if(take<n)w->output_truncated=true;};if(warn)append("WARN: ",6);for(int i=1;i<=lua_gettop(L)&&room>0;i++){if(i>1)append("\t",1);size_t n=0;const char*s=luaL_tolstring(L,i,&n);append(s,n);lua_pop(L,1);}if(room>0)append("\n",1);else w->output_truncated=true;return 0;}
#include "replicated_signal_compat.inc"
#include "instance_compat.inc"
#include "instance_extended_compat.inc"
int property_origin(lua_State*L){Handle*h=checked_handle(L,1);std::string k=canonical_instance_member(luaL_checkstring(L,2),h->node);std::string s=origin(*h->world,*h->node,k);lua_pushlstring(L,s.data(),s.size());return 1;}
int client_index(lua_State*L){World*w=static_cast<World*>(lua_touserdata(L,lua_upvalueindex(1)));const char*k=luaL_checkstring(L,2);if(std::strcmp(k,"snapshot")==0)lua_pushboolean(L,!w->persistent);else if(std::strcmp(k,"persistent")==0)lua_pushboolean(L,w->persistent);else if(std::strcmp(k,"sequence")==0){if(w->sequence>0)lua_pushnumber(L,w->sequence);else lua_pushnil(L);}else if(std::strcmp(k,"propertyOrigin")==0)lua_pushcfunction(L,property_origin,"propertyOrigin");else lua_pushnil(L);return 1;}

} // namespace

const Json* constructor_class(std::string_view class_name) {
    return cached_constructor_class(class_name);
}
std::size_t constructor_class_cache_size() {
    return constructor_cache_size_internal();
}
void apply_constructor_class_parents(Json& class_parents) {
    if(!class_parents.is_object())throw std::runtime_error("class parent metadata must be an object");
    apply_constructor_class_parents_internal(class_parents);
}

void prune_signal_waiters(World& world,const Signal* only_signal) {
    // Ordinary Connect-only signals have no waiter lifetime to maintain.
    // Avoid rescanning the pending event queue for every event in a burst.
    if(std::none_of(world.connections.begin(),world.connections.end(),[only_signal](const auto& connection) {
        return connection&&connection->signal_waiter
            &&(!only_signal||connection->signal==only_signal);
    }))return;
    auto task_waiting=[&](lua_State* thread) {
        return world.scheduler&&std::strcmp(world.scheduler->task_state(thread),"waiting")==0;
    };
    for(auto map_it=world.waiters.begin();map_it!=world.waiters.end();) {
        Signal* signal=map_it->first;
        if(only_signal&&signal!=only_signal){++map_it;continue;}
        auto& waiters=map_it->second;
        for(auto it=waiters.begin();it!=waiters.end();) {
            Connection* connection=it->connection;
            const bool active=connection&&connection->connected
                &&!connection->manually_disconnected&&!connection->auto_disconnected
                &&task_waiting(it->thread);
            if(active){++it;continue;}
            if(connection&&connection->connected) {
                connection->connected=false;
                connection->auto_disconnected=true;
            }
            if(it->reference!=LUA_NOREF)lua_unref(it->thread,it->reference);
            it=waiters.erase(it);
        }
        if(waiters.empty())map_it=world.waiters.erase(map_it);
        else ++map_it;
    }
    for(auto& event:world.events) {
        Connection* connection=event.connection;
        if(!event.waiter||!connection||!connection->signal_waiter
           ||(only_signal&&connection->signal!=only_signal))continue;
        const bool active=(connection->connected||connection->auto_disconnected)
            &&!connection->manually_disconnected&&task_waiting(event.waiter);
        if(active)continue;
        if(connection->connected) {
            connection->connected=false;
            connection->auto_disconnected=true;
        }
        if(event.waiter_ref!=LUA_NOREF)lua_unref(event.waiter,event.waiter_ref);
        event.waiter=nullptr;
        event.waiter_ref=LUA_NOREF;
        event.connection=nullptr;
    }
    std::unordered_set<Connection*> active;
    for(const auto& [signal,waiters]:world.waiters)for(const auto& waiter:waiters)
        if(waiter.connection)active.insert(waiter.connection);
    for(const auto& event:world.events)
        if(event.waiter&&event.connection&&event.connection->signal_waiter)
            active.insert(event.connection);
    for(auto it=world.connections.begin();it!=world.connections.end();) {
        Connection* connection=it->get();
        if(!connection->signal_waiter||active.count(connection)
           ||(only_signal&&connection->signal!=only_signal)) {++it;continue;}
        if(connection->creator_ref!=LUA_NOREF&&connection->creator_thread)
            lua_unref(connection->creator_thread,connection->creator_ref);
        connection->creator_ref=LUA_NOREF;
        it=world.connections.erase(it);
    }
}

void refresh_camera(World& world) { refresh_current_camera(world); }
void complete_animation_responses(World& world,const Json& responses) {
    complete_animations(world,responses);
}

// Validate the derived Color once at the received-state boundary, then keep
// its property/origin/revision as lazy views of the immutable Rust record.
// Ordered action-time writes still use SharedJson's private sparse overlay.
static void bind_rust_node_properties(
    Node& node, const std::shared_ptr<RustNodeProviderBundle>& bundle) {
    bool color_alias = false;
    const Json* descriptor = property_descriptor(node, "Color");
    if (descriptor && descriptor->value("type", "") == "Color3"
        && !node.rust_world_node->property("Color")) {
        auto wire_color = node.rust_world_node->property("Color3uint8");
        if (wire_color) {
            Json fields = Json::object();
            fields["Color3uint8"] = std::move(*wire_color);
            color_alias = received_color_alias(node, fields) != nullptr;
        }
    }
    bundle->set_color_alias(color_alias);
    bool collision_group_alias = false;
    const Json* group_descriptor = property_descriptor(node, "CollisionGroup");
    if (group_descriptor && group_descriptor->value("type", "") == "string"
        && !node.rust_world_node->property("CollisionGroup")) {
        if (auto wire_group = node.rust_world_node->property("CollisionGroupReplicate")) {
            (void)received_collision_group_value(*wire_group);
            collision_group_alias = true;
        }
    }
    bundle->set_collision_group_alias(collision_group_alias);
    node.properties.reset_to(RustNodeProviderBundle::properties_provider(bundle));
    node.origins.reset_to(RustNodeProviderBundle::origins_provider(bundle));
    node.revisions.reset_to(RustNodeProviderBundle::revisions_provider(bundle));
}

Node parse_node_record(const Json& record,
                       const std::shared_ptr<const Json>& snapshot_owner = {},
                       const void* shared_world_view = nullptr);
#include "actor_mutations.inc"
Json snapshot_tweens(const World& world) { return actor_tween_snapshot_internal(world); }
void restore_tweens(World& world,const Json& snapshot) { actor_tween_restore_internal(world,snapshot); }
#include "actor_world_bindings.inc"

int push_translation_cframe(lua_State* L,double x,double y,double z) {
    luaL_getmetatable(L,kCFrameMeta);
    const bool installed=lua_istable(L,-1);lua_pop(L,1);
    if(!installed){luaL_error(L,"Region3.CFrame requires world-source mode");return 0;}
    auto* cf=new_cframe(L);cf->p={x,y,z};
    luaL_getmetatable(L,kCFrameMeta);lua_setmetatable(L,-2);return 1;
}

bool is_tween_info(lua_State* L,int index) {
    return lua_isuserdata(L,index)&&tween_info_is_value(L,index);
}
bool capture_tween_info(lua_State* L,int index,Json& value) {
    if(!is_tween_info(L,index))return false;
    double time,delay;int style,direction;lua_Integer repeats;bool reverses;
    tween_info_capture_fields(L,index,time,style,direction,repeats,reverses,delay);
    value=Json{{"$tween_info",Json::array({time,style,direction,repeats,reverses,delay})}};
    return true;
}
bool push_tween_info_json(lua_State* L,const Json& value) {
    if(!value.is_object()||value.size()!=1||!value.contains("$tween_info"))return false;
    const auto& v=value.at("$tween_info");
    if(!v.is_array()||v.size()!=6||!v[0].is_number()||!v[1].is_number_integer()
        ||!v[2].is_number_integer()||!v[3].is_number_integer()||!v[4].is_boolean()||!v[5].is_number())
        throw std::runtime_error("invalid TweenInfo components");
    if(!std::isfinite(v[0].get<double>())||v[0]<0||!std::isfinite(v[5].get<double>())||v[5]<0
        ||v[1]<0||v[1]>10||v[2]<0||v[2]>2||v[3]<-2147483648LL||v[3]>2147483647LL)
        throw std::runtime_error("invalid TweenInfo component range");
    luaL_getmetatable(L,kTweenInfoMeta);bool installed=lua_istable(L,-1);lua_pop(L,1);
    if(!installed)throw std::runtime_error("TweenInfo requires the world runtime");
    tween_info_push_fields(L,v[0].get<double>(),v[1].get<int>(),v[2].get<int>(),
                          v[3].get<lua_Integer>(),v[4].get<bool>(),v[5].get<double>());
    return true;
}

Json apply_engine_poses(World& world,const Json& poses) {
    return apply_engine_poses_impl(world,poses);
}
Json apply_engine_owned_parts(World& world,const Json& updates) {
    return apply_engine_owned_parts_impl(world,updates);
}

namespace {
void emit_humanoid_state_changed(lua_State* L, World& w, Node& humanoid,
                                 int previous, int current) {
    if (previous == current || !humanoid_state_name(previous) || !humanoid_state_name(current)) return;
    const std::string signal_key = Json::array({humanoid.id, "StateChanged"}).dump();
    auto signal = w.signals.find(signal_key);
    if (signal == w.signals.end()) return;
    bool listening = false;
    for (const auto& connection : w.connections)
        if (connection->signal == signal->second.get() && connection->connected && connection->enabled &&
            (!connection->once || !connection->once_pending)) { listening = true; break; }
    auto waiters = w.waiters.find(signal->second.get());
    if (!listening && waiters != w.waiters.end() && !waiters->second.empty()) listening = true;
    if (!listening) return;
    lua_createtable(L, 2, 0);
    push_humanoid_state_item(L, previous); lua_rawseti(L, -2, 1);
    push_humanoid_state_item(L, current); lua_rawseti(L, -2, 2);
    const int reference = lua_ref(L, -1); lua_pop(L, 1);
    w.event_argument_refs.push_back(reference);
    emit(w, humanoid, "StateChanged", Json::array(), reference, 2);
}

int received_humanoid_state(World& w, Node& humanoid) {
    const Json* received = property_value(w, humanoid, "HumanoidStateType");
    if (!received) return -1;
    if (received->is_number_integer()) {
        const int value = received->get<int>();
        return humanoid_state_name(value) ? value : -1;
    }
    if (received->is_object() && received->contains("$enum") &&
        (*received)["$enum"].is_object() && (*received)["$enum"].contains("value") &&
        (*received)["$enum"]["value"].is_number_integer()) {
        const int value = (*received)["$enum"]["value"].get<int>();
        return humanoid_state_name(value) ? value : -1;
    }
    return -1;
}
}

bool SessionCustomAssetRegistry::register_asset(const std::string& scope,const std::string& path,
                                                const std::string& uri,const char* bytes,size_t size) {
    std::lock_guard<std::mutex> lock(mutex_);
    std::string path_key=scope;path_key.push_back('\0');path_key+=path;
    auto registered=assets_.find(uri);
    if(registered==assets_.end()&&assets_.size()>=kMaxRegisteredAssets)return false;
    if(registered!=assets_.end()&&registered->second!=path_key)return false;
    auto found=files_.find(path_key);
    const size_t old_size=found!=files_.end()&&found->second?found->second->size():0;
    const size_t base=retained_bytes_-old_size;
    if(size>kMaxRegisteredAssetBytes-base) {
        if(found!=files_.end()){found->second.reset();retained_bytes_=base;}
        return false;
    }
    std::shared_ptr<const std::string> next;
    try {
        next=std::make_shared<const std::string>(bytes?bytes:"",size);
    } catch(...) {
        if(found!=files_.end()){found->second.reset();retained_bytes_=base;}
        throw;
    }
    const bool new_file=found==files_.end();
    if(new_file)found=files_.emplace(path_key,nullptr).first;
    if(registered==assets_.end())try {
        assets_.emplace(uri,path_key);
    } catch(...) {
        if(new_file)files_.erase(found);
        throw;
    }
    found->second=std::move(next);
    retained_bytes_=base+size;
    return true;
}

void SessionCustomAssetRegistry::update_file(const std::string& scope,const std::string& path,
                                              const char* bytes,size_t size,bool present) {
    std::lock_guard<std::mutex> lock(mutex_);
    std::string path_key=scope;path_key.push_back('\0');path_key+=path;
    auto found=files_.find(path_key);
    if(found==files_.end())return;
    const size_t old_size=found->second?found->second->size():0;
    const size_t base=retained_bytes_-old_size;
    if(!present) {
        for(auto it=assets_.begin();it!=assets_.end();) {
            if(it->second==path_key)it=assets_.erase(it);else ++it;
        }
        files_.erase(found);
        retained_bytes_=base;
        return;
    }
    std::shared_ptr<const std::string> next;
    if(size<=kMaxRegisteredAssetBytes-base)try {
        next=std::make_shared<const std::string>(bytes?bytes:"",size);
    } catch(...) {
        found->second.reset();retained_bytes_=base;throw;
    }
    const bool retained=bool(next);
    found->second=std::move(next);
    retained_bytes_=base+(retained?size:0);
}

std::shared_ptr<const std::string> SessionCustomAssetRegistry::resolve(const std::string& uri) const {
    std::lock_guard<std::mutex> lock(mutex_);
    auto registered=assets_.find(uri);
    if(registered==assets_.end())return {};
    auto found=files_.find(registered->second);
    return found==files_.end()?std::shared_ptr<const std::string>{}:found->second;
}

bool capture_geometry_enum_item(lua_State* L,int index,Json& value) {
    return capture_geometry_enum(L,index,value);
}
void install_content_values(lua_State* L){install_content_impl(L);}
void install_rotation_curve_key(lua_State* L){rotation_curve_key::install_rotation_curve_key(L);}
bool is_rotation_curve_key(lua_State* L,int index){return rotation_curve_key::read(L,index)!=nullptr;}
bool capture_rotation_curve_key(lua_State* L,int index,Json& value){auto* key=rotation_curve_key::read(L,index);if(!key)return false;value=rotation_curve_key::encode(*key);return true;}
bool push_rotation_curve_key(lua_State* L,const Json& value){return rotation_curve_key::decode(L,value);}
bool is_content_value(lua_State* L,int index){return read_content(L,index)!=nullptr;}
bool capture_content_value(lua_State* L,int index,Json& value){auto* item=read_content(L,index);if(!item)return false;value=encode_content(*item);return true;}
bool push_content_value(lua_State* L,const Json& value,World* world){return decode_content(L,value,world);}
bool push_geometry_enum_item(lua_State* L,const char* type,int value) {
    for(int kind=0;kind<int(std::size(kGeometryEnums));++kind)if(std::strcmp(type,kGeometryEnums[kind].name)==0)
        for(int i=0;i<kGeometryEnums[kind].count;++i)if(geometry_numeric(kind,i)==value){push_geometry_item(L,kind,i);return true;}
    return push_ussi_export_enum_json(L,Json{{"$enum",{{"type",type},{"value",value}}}});
}
bool read_geometry_enum_item(lua_State* L,int index,const char* type,int& value) {
    if(has_meta(L,index,kUsiExportItemMeta)) {
        auto* item=static_cast<UsiExportItemRef*>(lua_touserdata(L,index));
        const auto& enumeration=kUsiExportEnums[item->kind];
        if(std::strcmp(enumeration.name,type)!=0)return false;
        value=enumeration.items[item->item].value;return true;
    }
    if(!has_meta(L,index,kGeometryItemMeta))return false;
    auto* item=static_cast<GeometryEnumItem*>(lua_touserdata(L,index));
    if(std::strcmp(kGeometryEnums[item->kind].name,type)!=0)return false;
    value=geometry_numeric(item->kind,item->value);return true;
}
void install_geometry_value_enums(lua_State* L) {
    lua_newtable(L);install_geometry_enums(L);install_enum_root(L);lua_setreadonly(L,-1,true);lua_setglobal(L,"Enum");
}

void update_physics_feedback(lua_State* L, World& w, const Json& feedback) {
    if (!feedback.is_object()) throw std::runtime_error("physics feedback must be an object");
    const Json requested_id = feedback.value("humanoid_id", Json(nullptr));
    if (requested_id.is_null()) {
        if (feedback.contains("running_speed"))
            throw std::runtime_error("physics feedback running_speed requires a Humanoid ID");
        return;
    }
    if (!requested_id.is_string()) throw std::runtime_error("physics feedback Humanoid ID must be a string");
    Node* player = find_node(w, w.local_player);
    if (!player) return;
    const Json* character_ref = property_value(w, *player, "Character");
    if (!character_ref || !character_ref->is_object() || !character_ref->contains("$ref") ||
        !(*character_ref)["$ref"].is_string()) return;
    const std::string character_id = (*character_ref)["$ref"].get<std::string>();
    Node* humanoid = find_node(w, requested_id.get<std::string>());
    if (!humanoid || humanoid->class_name != "Humanoid" || parent_id(w, *humanoid) != character_id || humanoid->destroyed)
        throw std::runtime_error("physics feedback Humanoid is not the current local character");
    const Json move_to = feedback.value("move_to", Json::array());
    if(!move_to.is_array()||move_to.size()>256)
        throw std::runtime_error("MoveTo feedback must be a bounded array");
    int64_t prior_request=0;
    for(const Json& item:move_to) {
        if(!item.is_object()||!item.contains("sequence")||!item["sequence"].is_number_integer()||
           !item.contains("reached")||!item["reached"].is_boolean()||
           item["sequence"].get<int64_t>()<=prior_request)
            throw std::runtime_error("MoveTo feedback requires increasing positive sequences and boolean results");
        prior_request=item["sequence"].get<int64_t>();
    }
    auto publish_move_to=[&]() {
        const Json* previous=local_value(w,*humanoid,"__MoveToFeedbackSequence");
        int64_t delivered=previous&&previous->is_number_integer()?previous->get<int64_t>():0;
        for(const Json& item:move_to) {
            const int64_t sequence=item["sequence"].get<int64_t>();
            if(sequence<=delivered)continue;
            emit(w,*humanoid,"MoveToFinished",Json::array({item["reached"]}));
            w.local_properties[humanoid->id]["__MoveToFeedbackSequence"]=sequence;
            delivered=sequence;
        }
    };
    // Validate motion before publishing any feedback. Only the current local
    // root is simulated; this is not authority to update other world bodies.
    Node* motion_root = nullptr;
    Json motion_velocity;
    if(feedback.contains("motion")) {
        const Json& motion=feedback["motion"];
        if(!motion.is_object()||motion.size()!=2||!motion.contains("root_id")||
           !motion["root_id"].is_string()||!motion.contains("linear_velocity")||
           !motion["linear_velocity"].is_array()||motion["linear_velocity"].size()!=3)
            throw std::runtime_error("invalid physics motion feedback");
        motion_root=find_node(w,motion["root_id"].get<std::string>());
        Node* character=find_node(w,character_id);
        bool valid_root=motion_root&&instance_name(w,*motion_root)=="HumanoidRootPart";
        if(character)if(const Json* primary=property_value(w,*character,"PrimaryPart"))
            valid_root=valid_root||(motion_root&&primary->is_object()&&primary->contains("$ref")&&
                (*primary)["$ref"].is_string()&&(*primary)["$ref"].get<std::string>()==motion_root->id);
        if(!motion_root||motion_root->destroyed||!pivot_part(w,*motion_root)||
           parent_id(w,*motion_root)!=character_id||!valid_root)
            throw std::runtime_error("physics feedback root is not the current local character");
        for(const Json& component:motion["linear_velocity"])
            if(!component.is_number()||!std::isfinite(component.get<double>())||std::abs(component.get<double>())>1.0e6)
                throw std::runtime_error("physics feedback velocity must be finite and bounded");
        motion_velocity=Json{{"$vector3",motion["linear_velocity"]}};
    }
    if (feedback.contains("running_speed")) {
        const Json& speed_value = feedback["running_speed"];
        if (!speed_value.is_number())
            throw std::runtime_error("physics feedback running_speed must be a number");
        const double speed = speed_value.get<double>();
        if (!std::isfinite(speed) || speed < 0)
            throw std::runtime_error("physics feedback running_speed must be finite and nonnegative");
        const Json* previous_speed = local_value(w, *humanoid, "__HumanoidRunningFeedback");
        if (!previous_speed || !previous_speed->is_number() || previous_speed->get<double>() != speed) {
            emit(w, *humanoid, "Running", Json::array({speed}));
            w.local_properties[humanoid->id]["__HumanoidRunningFeedback"] = speed;
        }
    }
    const Json state = feedback.value("state", Json(nullptr));
    if (state.is_null()) {
        clear_local_property(w, humanoid->id, "__HumanoidStateFeedback");
        update_body_position_feedback(w,Json::array(),character_id);
        publish_move_to();
        return;
    }
    if (!state.is_object() || !state.contains("name") || !state["name"].is_string() ||
        !state.contains("value") || !state["value"].is_number_integer())
        throw std::runtime_error("physics feedback state must contain a name and integer value");
    const int value = state["value"].get<int>();
    const std::string name = state["name"].get<std::string>();
    if (value < 0 || !humanoid_state_name(value) || humanoid_state_from_name(name) != value)
        throw std::runtime_error("physics feedback state name/value mismatch");
    const Json* old = local_value(w, *humanoid, "__HumanoidStateFeedback");
    int previous = old && old->is_number_integer() && humanoid_state_name(old->get<int>())
                   ? old->get<int>() : received_humanoid_state(w, *humanoid);
    Json transitions = feedback.value("transitions", Json::array());
    if (!transitions.is_array() || transitions.size() > 16)
        throw std::runtime_error("physics feedback transitions must be a bounded array");
    std::vector<int> transition_values;
    for (const Json& transition : transitions) {
        if (!transition.is_object() || !transition.contains("name") || !transition["name"].is_string() ||
            !transition.contains("value") || !transition["value"].is_number_integer())
            throw std::runtime_error("physics feedback transitions need name and integer value");
        const int transition_value = transition["value"].get<int>();
        if (!humanoid_state_name(transition_value) ||
            humanoid_state_from_name(transition["name"].get<std::string>()) != transition_value)
            throw std::runtime_error("physics feedback transition name/value mismatch");
        transition_values.push_back(transition_value);
    }
    if (transition_values.empty() && previous != value) transition_values.push_back(value);
    const bool platform_transition=previous==14||value==14||
        std::find(transition_values.begin(),transition_values.end(),14)!=transition_values.end();
    for (int transition_value : transition_values) {
        if (previous >= 0) emit_humanoid_state_changed(L, w, *humanoid, previous, transition_value);
        previous = transition_value;
    }
    if (!transition_values.empty() && transition_values.back() != value)
        throw std::runtime_error("physics feedback transitions do not end at stable state");
    w.local_properties[humanoid->id]["__HumanoidStateFeedback"] = value;
    if(platform_transition) {
        const bool active=value==14;
        const Json* old_platform=property_value(w,*humanoid,"PlatformStand");
        const bool changed=!old_platform||*old_platform!=Json(active);
        w.local_properties[humanoid->id]["PlatformStand"]=active;
        if(changed)property_events(w,*humanoid,"PlatformStand");
    }
    Json signals = feedback.value("signals", Json::array());
    if (!signals.is_array() || signals.size() > 8)
        throw std::runtime_error("physics feedback signals must be a bounded array");
    for (const Json& item : signals) {
        if (!item.is_object() || !item.contains("name") || !item["name"].is_string() ||
            !item.contains("active") || !item["active"].is_boolean())
            throw std::runtime_error("physics feedback signals need name and boolean active");
        const std::string signal = item["name"].get<std::string>();
        if (signal != "Jumping" && signal != "FreeFalling" && signal != "PlatformStanding")
            throw std::runtime_error("unsupported physics feedback signal");
        emit(w, *humanoid, signal, Json::array({item["active"]}));
    }
    if(feedback.contains("movers"))update_body_position_feedback(w,feedback["movers"],character_id);
    if(motion_root) {
        auto members=received_character_motion_parts(w,*motion_root);
        // The validated Model.PrimaryPart fallback can lack the standard name.
        if(members.empty())members.push_back(motion_root);
        std::vector<Node*> changed;
        for(auto* member:members) {
            const Json* previous=property_value(w,*member,"AssemblyLinearVelocity");
            if(!previous||*previous!=motion_velocity)changed.push_back(member);
            w.local_properties[member->id]["AssemblyLinearVelocity"]=motion_velocity;
        }
        for(auto* member:changed)property_events(w,*member,"AssemblyLinearVelocity");
    }
    publish_move_to();
    (void)L;
}

Node parse_node_record(const Json& record,
                       const std::shared_ptr<const Json>& snapshot_owner,
                       const void* shared_world_view) {
    if(!record.is_object()||!record.contains("id")||!record["id"].is_string()||!record.contains("class_name")||!record["class_name"].is_string()||!record.contains("name")||!record["name"].is_string())throw std::runtime_error("node requires string id, class_name, and name");
    Node n;n.id=record["id"].get<std::string>();n.class_name=record["class_name"].get<std::string>();n.name=record["name"].get<std::string>();
    Json shared_header;
    std::shared_ptr<RustNodeProviderBundle> rust_bundle;
    if(record.value("_rust_shared_node",false)) {
        if(!shared_world_view)throw std::runtime_error("compact Rust node header has no borrowed world view");
        rust_bundle=rust_world_lookup_bundle(shared_world_view,n.id);
        if(!rust_bundle)throw std::runtime_error("compact Rust node header is absent from its world view");
        n.rust_world_node=RustNodeProviderBundle::node_view(rust_bundle);
        shared_header=n.rust_world_node->header();
        if(!shared_header.is_object()||!shared_header.contains("id")||!shared_header["id"].is_string()||shared_header["id"]!=n.id
           ||!shared_header.contains("class_name")||!shared_header["class_name"].is_string()
           ||!shared_header.contains("name")||!shared_header["name"].is_string())throw std::runtime_error("invalid Rust node header");
        n.class_name=shared_header["class_name"].get<std::string>();n.name=shared_header["name"].get<std::string>();
        n.parent.clear();if(shared_header.contains("parent")&&!shared_header["parent"].is_null()){if(!shared_header["parent"].is_string())throw std::runtime_error("invalid Rust node parent");n.parent=shared_header["parent"].get<std::string>();}
        bind_rust_node_properties(n,rust_bundle);
    }
    if(n.id.empty()||n.id.size()>256||n.class_name.empty()||n.class_name.size()>256||n.name.size()>256)throw std::runtime_error("invalid node metadata");
    if(!n.rust_world_node&&record.contains("parent")&&!record["parent"].is_null()){if(!record["parent"].is_string())throw std::runtime_error("node parent must be a node id or null");n.parent=record["parent"].get<std::string>();}
    if(!n.rust_world_node&&record.contains("properties")){if(!record["properties"].is_object())throw std::runtime_error("node properties must be an object");if(snapshot_owner)n.properties.reset_to(std::shared_ptr<const Json>(snapshot_owner,&record.at("properties")));else n.properties=record["properties"];}
    if(!n.rust_world_node&&record.contains("origins")){if(!record["origins"].is_object())throw std::runtime_error("node origins must be an object");if(snapshot_owner)n.origins.reset_to(std::shared_ptr<const Json>(snapshot_owner,&record.at("origins")));else n.origins=record["origins"];}
    if(n.rust_world_node) {
        if(record.contains("attributes")&&!record.at("attributes").is_object())
            throw std::runtime_error("node attributes must be an object");
        auto attrs=n.rust_world_node->section(kRustWorldSectionAttributes);
        n.attributes.replace(attrs?*attrs:Json::object(),shared_header.value("attributes_known",attrs.has_value()));
    } else if(record.contains("attributes")) {
        if(!record["attributes"].is_object())throw std::runtime_error("node attributes must be an object");
        n.attributes.replace(record.at("attributes"),record.value("attributes_known",true));
    } else n.attributes.set_known(record.value("attributes_known",false));
    if(!n.rust_world_node&&record.contains("property_revisions")){if(!record["property_revisions"].is_object())throw std::runtime_error("property revisions must be an object");if(snapshot_owner)n.revisions.reset_to(std::shared_ptr<const Json>(snapshot_owner,&record.at("property_revisions")));else n.revisions=record["property_revisions"];}
    if(text_layout_node(n)&&n.properties.contains("TextWrap")&&!n.properties.contains("TextWrapped")) {
        n.properties["TextWrapped"]=*n.properties.lookup("TextWrap");n.origins["TextWrapped"]="derived_received";
        if(n.revisions.contains("TextWrap"))n.revisions["TextWrapped"]=n.revisions["TextWrap"];
    }
    Json alias_fields=Json::object();
    for(const char* key:{"Color3uint8","CollisionGroupReplicate"})if(const Json* v=n.properties.lookup(key))alias_fields[key]=*v;
    if(!n.properties.contains("Color"))if(const Json* color=received_color_alias(n,alias_fields)) {
        n.properties["Color"]=*color;n.origins["Color"]="derived_received";
        if(n.revisions.contains("Color3uint8"))n.revisions["Color"]=n.revisions["Color3uint8"];
    }
    if(!n.properties.contains("CollisionGroup"))if(Json group=received_collision_group_alias(n,alias_fields);!group.is_null()) {
        n.properties["CollisionGroup"]=std::move(group);n.origins["CollisionGroup"]="derived_received";
        if(n.revisions.contains("CollisionGroupReplicate"))n.revisions["CollisionGroup"]=n.revisions["CollisionGroupReplicate"];
    }
    const Json& metadata=record;
    n.destroyed=metadata.value("destroyed",false);n.server_owned=metadata.value("server_owned",true);
    n.constructor_defaults_known=metadata.value("constructor_defaults_known",false);
    collection_read_state(n,metadata);
    if(n.destroyed&&!n.parent.empty())throw std::runtime_error("destroyed instance must have nil Parent");
    // A bulk snapshot may visit tens of thousands of nodes inside one outer
    // SharedJsonScope. Release this node's sparse-read cache before moving on.
    n.properties.clear_transient_cache();
    n.origins.clear_transient_cache();
    n.revisions.clear_transient_cache();
    return n;
}
World parse_snapshot(const std::string& text) {
    return parse_snapshot(Json::parse(text));
}
namespace {
World parse_snapshot_impl(const Json& root,const std::shared_ptr<const Json>& snapshot_owner,const void* shared_world_view=nullptr) {
    SharedJsonScope json_scope;
    if(!root.is_object()||root.value("version",0)!=1)throw std::runtime_error("unsupported world snapshot version");
    if(!root.contains("nodes")||!root["nodes"].is_array()||root["nodes"].size()>kMaxNodes)throw std::runtime_error("snapshot nodes must be an array of at most "+std::to_string(kMaxNodes)+" records");
    if(!root.contains("services")||!root["services"].is_object()||root["services"].size()>128)throw std::runtime_error("snapshot services must be an object");
    World w;
    w.lightweight_assets=root.value("lightweight_assets",false);
    if(root.contains("session_stats"))w.session_stats=validated_session_stats(root.at("session_stats"));
    if(root.contains("input_capabilities"))w.input_capabilities=validated_input_capabilities(root.at("input_capabilities"));
    if(root.contains("viewport"))w.viewport=validated_viewport(root.at("viewport"));
    if(!w.viewport.empty())w.display=display_from_viewport(w.viewport);
    if(root.contains("display"))update_display(w,root.at("display"));
    w.chat_enabled=root.value("chat_enabled",false);
    w.initial_replication_complete=root.value("initial_replication_complete",false);
    if(root.contains("local_player")&&root["local_player"].is_string())w.local_player=root["local_player"].get<std::string>();
    if(root.contains("job_id")&&root["job_id"].is_string())w.job_id=root["job_id"].get<std::string>();
    if(root.contains("sequence")&&root["sequence"].is_number())w.sequence=root["sequence"].get<double>();
    if(root.contains("class_parents"))w.class_parents=root["class_parents"];
    if(root.contains("defaults"))w.defaults=root["defaults"];
    if(root.contains("default_origins"))w.default_origins=root["default_origins"];
    if(root.contains("class_events"))w.class_events=root["class_events"];
    if(root.contains("class_event_signatures"))w.class_event_signatures=root["class_event_signatures"];
    if(!w.class_event_signatures.is_object())throw std::runtime_error("class event signatures must be an object");
    for(const auto& cls:w.class_event_signatures) {
        if(!cls.is_object())throw std::runtime_error("class event signatures must contain objects");
        for(const auto& types:cls) {
            if(!types.is_array()||types.size()>256)throw std::runtime_error("invalid event argument signature");
            for(const auto& type:types)if(!type.is_number_integer()||type.get<int>()<0||type.get<int>()>255)
                throw std::runtime_error("invalid event argument type");
        }
    }
    if(root.contains("class_property_types"))w.class_property_types=root["class_property_types"];
    if(!w.class_property_types.is_object())throw std::runtime_error("class property types must be an object");
    if(!w.class_events.is_object())throw std::runtime_error("class events must be an object");
    for(const auto& list:w.class_events)if(!list.is_array())throw std::runtime_error("class events must contain arrays");
    if(!w.class_parents.is_object()||!w.defaults.is_object()||!w.default_origins.is_object())throw std::runtime_error("invalid class metadata/defaults");
    apply_constructor_class_parents(w.class_parents);
    // Non-creatable client singleton classes are absent from the constructor
    // registry. Fill their pinned API ancestry without requiring replication.
    for(const auto& link:std::initializer_list<std::pair<const char*,const char*>>{
        {"GlobalSettings","GenericSettings"},{"UserSettings","GenericSettings"},
        {"GenericSettings","ServiceProvider"},{"ServiceProvider","Instance"},
        {"PluginManager","Instance"}})
        if(!w.class_parents.contains(link.first))w.class_parents[link.first]=link.second;
    for(const Json& record:root["nodes"]){
        Node n=parse_node_record(record,snapshot_owner,shared_world_view);
        if(!snapshot_owner&&!n.rust_world_node)n.origins=w.received_origin_pool.intern(n.origins.read());
        if(n.class_name=="ScrollingFrame")w.active_scroll_layouts.insert(n.id);
        if(class_is_a(w,n.class_name,"GuiObject")&&!n.parent.empty())w.gui_parent_order[n.id]=w.next_gui_parent_order++;
        if(!w.nodes.emplace(n.id,std::move(n)).second)throw std::runtime_error("duplicate node id");w.order.push_back(record["id"].get<std::string>());
    }
    for(const auto&[id,node]:w.nodes)if(node.class_name=="DataModel"){if(!w.game_id.empty())throw std::runtime_error("snapshot has multiple DataModel nodes");w.game_id=id;}
    if(w.game_id.empty()){Node game;game.id="__world_snapshot_datamodel__";game.class_name="DataModel";game.name="Game";w.game_id=game.id;w.nodes.emplace(game.id,std::move(game));}
    Node settings;settings.id=kSettingsId;settings.class_name="UserSettings";settings.name="GlobalBasicSettings";settings.server_owned=false;settings.attributes.set_known(true);settings.collection_tags_known=true;w.nodes.emplace(settings.id,std::move(settings));w.order.push_back(kSettingsId);
    Node user_settings;user_settings.id=kUserGameSettingsId;user_settings.class_name="UserGameSettings";user_settings.name="GameSettings";user_settings.parent=kSettingsId;user_settings.server_owned=false;user_settings.attributes.set_known(true);user_settings.collection_tags_known=true;user_settings.properties=client_setting_defaults();
    for(auto it=user_settings.properties.begin();it!=user_settings.properties.end();++it)user_settings.origins[it.key()]="local_settings";
    w.nodes.emplace(user_settings.id,std::move(user_settings));w.order.push_back(kUserGameSettingsId);
    // Parent references to streamed-out instances remain unresolved; getters
    // produce nil while propertyOrigin returns `unresolved_reference`.
    for(auto it=root["services"].begin();it!=root["services"].end();++it){std::string id;if(it.value().is_string())id=it.value().get<std::string>();else if(it.value().is_object()&&it.value().contains("$ref")&&it.value()["$ref"].is_string())id=it.value()["$ref"].get<std::string>();else throw std::runtime_error("service values must be node ids or $ref records");if(!w.nodes.count(id)||id==w.game_id)throw std::runtime_error("service references unknown node");w.services.emplace(it.key(),id);Node& node=w.nodes.at(id);if(node.parent.empty()&&!node.destroyed)node.parent=w.game_id;}
    if(w.local_player.empty()){
        auto pi=w.services.find("Players");if(pi!=w.services.end()){Node&p=w.nodes.at(pi->second);const Json& player_properties=p.properties.read();auto li=player_properties.find("LocalPlayer");if(li!=player_properties.end()&&li->is_object()&&li->contains("$ref")&&(*li)["$ref"].is_string())w.local_player=(*li)["$ref"].get<std::string>();}
    }
    // Restore only the explicitly declared LocalPlayer mouse singleton.
    // Scanning by class/name is ambiguous because PlayerMouse can be cloned.
    if(root.contains("client_service_state")) {
        const Json& state=root.at("client_service_state");
        if(!state.is_object()||state.size()>1||!state.contains("player_mice")
           ||!state.at("player_mice").is_object()||state.at("player_mice").size()>1)
            throw std::runtime_error("invalid client service state in snapshot");
        const auto& mice=state.at("player_mice");
        for(auto it=mice.begin();it!=mice.end();++it) {
            if(it.key()!=w.local_player||!it.value().is_string())
                throw std::runtime_error("PlayerMouse cache must identify LocalPlayer");
            const std::string& id=it.value().get_ref<const std::string&>();
            auto mouse=w.nodes.find(id);
            const auto player=w.nodes.find(w.local_player);
            if(player==w.nodes.end()||player->second.destroyed
               ||player->second.class_name!="Player"
               ||mouse==w.nodes.end()||mouse->second.destroyed||mouse->second.server_owned
               ||mouse->second.class_name!="PlayerMouse")
                throw std::runtime_error("PlayerMouse cache references a missing LocalPlayer instance");
            // The explicit cache entry carries singleton identity. Properties,
            // Name, Parent, attributes, and tags may all have changed since
            // GetMouse created it, so do not require the constructor snapshot.
            w.client_services.player_mice.emplace(it.key(),id);
        }
    }
    // An unresolved local-player reference is retained and reads as nil.
    std::unordered_map<std::string,uint8_t> marks;
    for(const auto&[start,node]:w.nodes){(void)node;if(marks[start]==2)continue;std::vector<std::string>path;std::string cur=start;
        while(!cur.empty()&&w.nodes.count(cur)&&marks[cur]==0){marks[cur]=1;path.push_back(cur);cur=w.nodes.at(cur).parent;}
        if(!cur.empty()&&marks[cur]==1)throw std::runtime_error("snapshot parent cycle");
        for(const auto&id:path)marks[id]=2;
    }
    for(const auto& [id,node]:w.nodes)if(node.class_name=="Player") {
        auto value=node.properties.find("Character");
        if(value!=node.properties.end()&&value->is_object()&&value->contains("$ref")&&(*value)["$ref"].is_string()) {
            auto target=(*value)["$ref"].get<std::string>();if(!find_node(w,target))w.pending_characters[id]=target;
        }
    }
    refresh_current_camera(w);
    // Initial received part poses are authoritative. Seed connectivity without
    // snapping a captured world; later topology changes activate pose solving.
    seed_joint_activation(w);
    w.snapshot_owner=snapshot_owner;
    return w;
}
}
World parse_snapshot(const Json& root) { return parse_snapshot_impl(root,{}); }
World parse_snapshot(std::shared_ptr<const Json> snapshot) {
    if(!snapshot)throw std::runtime_error("null shared world snapshot");
    return parse_snapshot_impl(*snapshot,snapshot);
}
World parse_snapshot(const Json& root,const void* shared_world_view) {
    return parse_snapshot_impl(root,{},shared_world_view);
}

void install(lua_State*L,World&w){
    const int stack_base = lua_gettop(L);
    install_workspace_asset_registry(L,w);
    if(w.actor_id.empty()&&!w.actor_runtime)if(auto* player=find_node(w,w.local_player);player&&!player->destroyed)
        (void)signal_for(w,*player,"Idled");
    if(!w.services.count("UserInputService"))ensure_client_service(w,"UserInputService");
    if(!w.services.count("RunService"))ensure_client_service(w,"RunService");
    // CoreGui is a client-local container. Create its actual local identity
    // before scripts run so ordinary child lookup also finds the service.
    if(!w.services.count("CoreGui"))ensure_client_service(w,"CoreGui");
    // CorePackages is a client-local service; package contents are populated
    // only when authentic client assets have been installed.
    if(!w.services.count("CorePackages"))ensure_client_service(w,"CorePackages");
    install_vector3_methods(L);
    luaL_newmetatable(L,kChatStatusMeta);lua_pushcfunction(L,chat_status_index,"__index");lua_setfield(L,-2,"__index");lua_pushcfunction(L,chat_status_string,"__tostring");lua_setfield(L,-2,"__tostring");lua_pushcfunction(L,chat_status_equal,"__eq");lua_setfield(L,-2,"__eq");lua_pushcfunction(L,chat_readonly,"__newindex");lua_setfield(L,-2,"__newindex");lua_pushliteral(L,"EnumItem");lua_setfield(L,-2,"__type");lua_pushliteral(L,"chat EnumItem");lua_setfield(L,-2,"__metatable");lua_pop(L,1);
    luaL_newmetatable(L,kChatEnumMeta);lua_pushcfunction(L,chat_enum_index,"__index");lua_setfield(L,-2,"__index");lua_pushcfunction(L,chat_enum_string,"__tostring");lua_setfield(L,-2,"__tostring");lua_pushcfunction(L,chat_readonly,"__newindex");lua_setfield(L,-2,"__newindex");lua_pushliteral(L,"Enum");lua_setfield(L,-2,"__type");lua_pushliteral(L,"chat Enum");lua_setfield(L,-2,"__metatable");lua_pop(L,1);
    lua_newtable(L);lua_newuserdatatagged(L,1,0);luaL_getmetatable(L,kChatEnumMeta);lua_setmetatable(L,-2);lua_pushvalue(L,-1);lua_setfield(L,LUA_REGISTRYINDEX,"Headless.ChatStatusEnum");lua_setfield(L,-2,"TextChatMessageStatus");install_animation_enum(L);install_geometry_enums(L);install_humanoid_state_enum(L);install_ussi_export_enums(L,-1);install_enum_root(L);lua_setreadonly(L,-1,true);lua_setglobal(L,"Enum");
    install_tween_bindings(L);
    install_world_raycast(L,w);
    install_world_overlap(L,w);
    install_ussi_security_capabilities(L,LUA_GLOBALSINDEX);
    luaL_newmetatable(L,kSignalMeta);lua_pushcfunction(L,signal_index,"__index");lua_setfield(L,-2,"__index");lua_pushliteral(L,"RBXScriptSignal");lua_setfield(L,-2,"__type");lua_pushliteral(L,"headless signal");lua_setfield(L,-2,"__metatable");lua_pop(L,1);
    luaL_newmetatable(L,kConnectionMeta);lua_pushcfunction(L,connection_index,"__index");lua_setfield(L,-2,"__index");lua_pushliteral(L,"RBXScriptConnection");lua_setfield(L,-2,"__type");lua_pushliteral(L,"headless connection");lua_setfield(L,-2,"__metatable");lua_pop(L,1);
    luaL_newmetatable(L,kInstanceHandleMeta);lua_pushcfunction(L,instance_handle_index,"__index");lua_setfield(L,-2,"__index");lua_pushcfunction(L,instance_handle_equal,"__eq");lua_setfield(L,-2,"__eq");lua_pushcfunction(L,instance_handle_tostring,"__tostring");lua_setfield(L,-2,"__tostring");lua_pushliteral(L,"InstanceHandle");lua_setfield(L,-2,"__type");lua_pushliteral(L,"headless InstanceHandle");lua_setfield(L,-2,"__metatable");lua_pop(L,1);
    luaL_newmetatable(L,kInstanceMeta);
    lua_pushcfunction(L,instance_index,"__index");lua_setfield(L,-2,"__index");
    lua_pushcfunction(L,instance_newindex,"__newindex");lua_setfield(L,-2,"__newindex");
    lua_pushcfunction(L,instance_tostring,"__tostring");lua_setfield(L,-2,"__tostring");
    lua_pushcfunction(L,instance_equal,"__eq");lua_setfield(L,-2,"__eq");
    lua_pushcclosurek(L,compat_namecall,"__namecall",0,compat_namecall_continue);lua_setfield(L,-2,"__namecall");
    lua_pushliteral(L,"Instance");lua_setfield(L,-2,"__type");lua_pushliteral(L,"world snapshot Instance");lua_setfield(L,-2,"__metatable");lua_pop(L,1);
    luaL_newmetatable(L,kCFrameMeta);lua_pushcfunction(L,cframe_index,"__index");lua_setfield(L,-2,"__index");lua_pushcfunction(L,cframe_tostring,"__tostring");lua_setfield(L,-2,"__tostring");lua_pushcfunction(L,cframe_equal,"__eq");lua_setfield(L,-2,"__eq");lua_pushliteral(L,"CFrame");lua_setfield(L,-2,"__type");lua_pushliteral(L,"snapshot CFrame");lua_setfield(L,-2,"__metatable");install_frame_methods(L);lua_pop(L,1);
    lua_createtable(L,0,8);lua_pushcfunction(L,vector3_new,"new");lua_setfield(L,-2,"new");install_geometry_vector_constructors(L);set_vector_constant(L,"zero",0,0,0);set_vector_constant(L,"one",1,1,1);set_vector_constant(L,"xAxis",1,0,0);set_vector_constant(L,"yAxis",0,1,0);set_vector_constant(L,"zAxis",0,0,1);lua_setreadonly(L,-1,1);lua_setglobal(L,"Vector3");
    lua_createtable(L,0,8);install_frame_constructors(L);lua_pushcfunction(L,cframe_new,"new");lua_setfield(L,-2,"new");lua_pushcfunction(L,cframe_from_matrix,"fromMatrix");lua_setfield(L,-2,"fromMatrix");auto*identity=new_cframe(L);luaL_getmetatable(L,kCFrameMeta);lua_setmetatable(L,-2);lua_setfield(L,-2,"identity");lua_setreadonly(L,-1,1);lua_setglobal(L,"CFrame");
    Node*game=find_node(w,w.game_id);push_node(L,w,game);lua_setglobal(L,"game");
    auto ws=w.services.find("Workspace");if(ws!=w.services.end()){push_node(L,w,find_node(w,ws->second));lua_setglobal(L,"workspace");}
    auto players=w.services.find("Players");if(players!=w.services.end()){
        Node*p=find_node(w,players->second);if(p&&p->properties.contains("LocalPlayer")&&p->properties["LocalPlayer"].is_object()){
            const Json&lp=p->properties["LocalPlayer"];if(lp.contains("$ref")&&lp["$ref"].is_string())w.local_player=lp["$ref"].get<std::string>();
        }
    }
    lua_pushlightuserdata(L,&w);lua_pushcclosurek(L,print_capture,"print",1,nullptr);lua_setglobal(L,"print");
    lua_pushlightuserdata(L,&w);lua_pushboolean(L,1);lua_pushcclosurek(L,print_capture,"warn",2,nullptr);lua_setglobal(L,"warn");
    lua_getglobal(L,"print");
    lua_pushcclosure(L,[](lua_State* S)->int{
        const char* prefix=luaL_optstring(S,1,"Current identity is");
        lua_pushvalue(S,lua_upvalueindex(1));
        lua_pushfstring(S,"%s %d",prefix,SourceScheduler::identity(S));
        lua_call(S,1,0);return 0;
    },"printidentity",1);lua_setglobal(L,"printidentity");
    lua_pushlightuserdata(L,&w);lua_pushcclosure(L,global_stats,"stats",1);
    lua_pushvalue(L,-1);lua_setglobal(L,"Stats");lua_setglobal(L,"stats");
    lua_pushlightuserdata(L,&w);lua_pushcclosurek(L,make_user_settings,"UserSettings",1,nullptr);lua_setglobal(L,"UserSettings");
    lua_pushlightuserdata(L,&w);lua_pushcclosurek(L,make_global_settings,"settings",1,nullptr);lua_setglobal(L,"settings");
    lua_pushlightuserdata(L,&w);lua_pushcclosurek(L,make_plugin_manager,"PluginManager",1,nullptr);lua_setglobal(L,"PluginManager");
    lua_createtable(L,0,0);
    lua_createtable(L,0,2);lua_pushlightuserdata(L,&w);lua_pushcclosurek(L,client_index,"client.__index",1,nullptr);lua_setfield(L,-2,"__index");lua_pushliteral(L,"world snapshot client");lua_setfield(L,-2,"__metatable");
    lua_setmetatable(L,-2);lua_setglobal(L,"client");
    lua_createtable(L,0,1);lua_pushlightuserdata(L,&w);lua_pushcclosurek(L,instance_new,"Instance.new",1,nullptr);lua_setfield(L,-2,"new");lua_setreadonly(L,-1,1);lua_setglobal(L,"Instance");
    lua_newtable(L);lua_pushlightuserdata(L,&w);lua_pushcclosurek(L,instance_handle_new,"InstanceHandle.new",1,nullptr);lua_setfield(L,-2,"new");lua_setreadonly(L,-1,true);lua_setglobal(L,"InstanceHandle");
    install_instance_compat(L,w);
    install_instance_extended(L,w);
    assert(lua_gettop(L) == stack_base);
}
Json capture_value(lua_State* L,World& world,int index){return convert_lua_value(L,world,index);}
void push_serialized_value(lua_State* L,World& world,const Json& value){push_json(L,world,value);}
int clone_client_template(lua_State* L) {
    auto* handle=checked_handle(L,1);
    if(!handle->world->persistent){luaL_error(L,"template cloning requires a persistent world");return 0;}
    bool previous=cloning_client_template;cloning_client_template=true;
    lua_pushcfunction(L,clone_instance,"client template clone");lua_pushvalue(L,1);
    int status=lua_pcall(L,1,1,0);cloning_client_template=previous;
    if(status!=LUA_OK){lua_error(L);return 0;}return 1;
}
void push_instance(lua_State* L,World& world,const std::string& id){auto* node=find_node(world,id);if(node)push_node(L,world,node);else lua_pushnil(L);}
std::string instance_display_name(World& world,const std::string& id){return instance_name(world,world.nodes.at(id));}
void push_player_scripts(lua_State* L,World& w) {
    auto* player=find_node(w,w.local_player);if(!player){lua_pushnil(L);return;}
    for(const auto& id:w.order){auto& node=w.nodes.at(id);if(!node.destroyed&&node.class_name=="PlayerScripts"&&parent_id(w,node)==player->id){push_node(L,w,&node);return;}}
    if(w.nodes.size()>=kMaxNodes+1000)throw std::runtime_error("client container limit exceeded");
    Node node;node.id=w.allocate_local_id();
    node.class_name="PlayerScripts";node.name="PlayerScripts";node.parent=player->id;node.server_owned=false;node.attributes.set_known(true);node.collection_tags_known=true;
    node.origins=Json{{"Name","local"},{"ClassName","local"},{"Parent","local"}};
    std::vector<Json> entries{Json{{"kind","create"},{"node",node_record(node)}}};check_mutation_budget(w,entries);
    auto id=node.id;auto [it,ok]=w.nodes.emplace(id,std::move(node));w.order.push_back(id);append_mutations(w,std::move(entries));
    parent_events(w,it->second,"",player->id);push_node(L,w,&it->second);
}
std::unordered_set<std::string> actors_that_may_own_client_work(World& world) {
    std::unordered_set<std::string> owners;
    for(const auto& id:world.order) {
        auto found=world.nodes.find(id);
        if(found==world.nodes.end()||found->second.destroyed)continue;
        const Node& candidate=found->second;
        if(candidate.class_name!="LocalScript"&&candidate.class_name!="Script")continue;
        if(candidate.class_name=="Script") {
            const Json* run_context=property_value(world,candidate,"RunContext");
            if(run_context&&run_context->is_object()&&run_context->size()==1
               &&run_context->contains("$enum")&&run_context->at("$enum").is_object()
               &&run_context->at("$enum").contains("value"))
                run_context=&run_context->at("$enum").at("value");
            // Unknown values stay eligible for the prefilter. ScriptExecution
            // remains authoritative for client RunContext eligibility.
            if(run_context&&run_context->is_number()&&run_context->get<double>()!=2.0)continue;
        }
        std::string parent=parent_id(world,candidate);
        for(size_t depth=0;!parent.empty()&&depth<=world.nodes.size();++depth) {
            Node* ancestor=find_node(world,parent);
            if(!ancestor||ancestor->destroyed)break;
            if(ancestor->class_name=="Actor") {
                owners.insert(ancestor->id);
                // ScriptExecution eligibility assigns work to the nearest
                // ancestor whose ClassName is exactly Actor.
                break;
            }
            if(ancestor->id==world.game_id)break;
            parent=parent_id(world,*ancestor);
        }
    }
    return owners;
}
Json script_nodes(World& w) {
    Json rows=Json::array();
    for(const auto& id:w.order) {
        auto& n=w.nodes.at(id);
        if(n.class_name!="LocalScript"&&n.class_name!="ModuleScript"&&n.class_name!="Script")continue;
        Json row{{"id",id},{"destroyed",n.destroyed},
                 {"destroy_cleanup_complete",n.script_destroy_cleanup_complete}};
        for(const char* key:{"Disabled","Enabled","RunContext"}) {
            const Json* value=property_value(w,n,key);if(value)row[key]=*value;
        }
        rows.push_back(std::move(row));
    }
    return rows;
}
bool stop_script_tasks(lua_State* from,World& w,const std::string& owner,lua_State* deferred_main,bool lifecycle_disable) {
    if(!w.scheduler)return false;
    clear_owned_bindables(from,w,owner);
    clear_render_owner(from,w,owner);
    clear_context_owner(from,w,owner);
    auto owned=[&](lua_State* thread){return w.scheduler->owner(thread)==owner;};
    for(auto& connection:w.connections)if(connection->owner==owner){connection->connected=false;connection->manually_disconnected=true;}
    for(auto it=w.child_waiters.begin();it!=w.child_waiters.end();) {
        if(owned(it->thread)){lua_unref(from,it->reference);it=w.child_waiters.erase(it);}else ++it;
    }
    for(auto it=w.handle_waiters.begin();it!=w.handle_waiters.end();) {
        if(owned(it->thread)){lua_unref(from,it->reference);it=w.handle_waiters.erase(it);}else ++it;
    }
    for(auto it=w.chat_waiters.begin();it!=w.chat_waiters.end();) {
        if(owned(it->thread)){lua_unref(from,it->reference);it=w.chat_waiters.erase(it);}else ++it;
    }
    for(auto it=w.text_service_waiters.begin();it!=w.text_service_waiters.end();) {
        if(owned(it->thread)){lua_unref(from,it->reference);it=w.text_service_waiters.erase(it);}else ++it;
    }
    const bool preserved_main=lifecycle_disable?w.scheduler->disable_script_owner(owner)
                                               :w.scheduler->cancel_owner(owner,deferred_main);
    prune_signal_waiters(w);
    dispatch_remote_server_waiters(from,w);
    return preserved_main;
}
void dispatch_pending_events(lua_State* from,World& world) {
    // Already received signals must make progress even when 128 interrupted
    // callbacks keep the ordinary runnable batch full. Admit a bounded batch
    // against the total callback-task limit (including the foreground reserve).
    // Periodic frame generation below still uses callback_capacity(), so this
    // cannot generate new Heartbeats while the runnable queue is backpressured.
    size_t capacity=std::min(size_t{128},world.scheduler->invocation_capacity());
    while(!world.events.empty()) {
        const auto& front=world.events.front();
        if(front.connection&&!front.connection->signal_waiter&&front.connection->native_kind==Connection::NativeKind::None&&
           !front.connection->manually_disconnected&&
           front.connection->enabled&&
           (front.connection->connected||front.connection->auto_disconnected)&&capacity==0)break;
        auto event=std::move(world.events.front());world.events.pop_front();
        auto push=[&](lua_State* target){
            if(event.argument_ref!=LUA_NOREF) {
                lua_getref(target,event.argument_ref);int table=lua_gettop(target);
                size_t budget=10000;std::unordered_set<const void*> path;
                const bool actor_message=event.connection&&event.connection->signal
                    &&event.connection->signal->key.rfind("ActorMessage:",0)==0;
                size_t actor_bytes=kActorMessageBytes,actor_items=kActorMessageItems;
                for(int i=0;i<event.argument_count;i++){
                    lua_rawgeti(target,table,i+1);
                    if(actor_message){
                        const auto value=capture_actor_message_value(target,world,-1,actor_bytes,actor_items,path);
                        push_actor_message_value(target,world,value);
                    }else copy_bindable_value(target,-1,path,budget);
                    lua_remove(target,-2);
                }
                lua_remove(target,table);return event.argument_count;
            }
            for(const auto& value:event.arguments)push_json(target,world,value);return static_cast<int>(event.arguments.size());
        };
        if(event.waiter) {
            auto* connection=event.connection;
            if(connection&&connection->signal_waiter) {
                if((!connection->connected&&!connection->auto_disconnected)
                   ||connection->manually_disconnected||!connection->signal) {
                    if(event.waiter_ref!=LUA_NOREF)lua_unref(from,event.waiter_ref);
                    continue;
                }
                // If Disable races an already queued signal, keep the wait alive
                // for the next signal after Enable rather than waking it early.
                if(!connection->enabled) {
                    world.waiters[connection->signal].push_back(
                        SignalWaiter{event.waiter,event.waiter_ref,connection});
                    continue;
                }
                connection->connected=false;
                connection->auto_disconnected=true;
            }
            world.scheduler->wake_signal(event.waiter,push,event.response_ready);
            if(event.waiter_ref!=LUA_NOREF)lua_unref(from,event.waiter_ref);
            if(connection&&connection->signal_waiter)
                prune_signal_waiters(world,connection->signal);
        }
        else if(event.connection) {
            auto& c=*event.connection;
            if(c.manually_disconnected||(!c.connected&&!c.auto_disconnected)||!c.enabled)continue;
            if(c.native_kind==Connection::NativeKind::ClientIdle) {
                if(!world.client_idle_control_enabled||!c.signal||!c.signal->node||
                   c.signal->node->destroyed||c.signal->node->id!=world.client_idle_player_id)continue;
                // Ignore event arguments, including firesignal supplied values.
                // The native client policy reads only its own monotonic clock.
                if(event.engine_idle_event) {
                    world.client_idle_event_observed=true;
                    ++world.client_idle_event_count;
                    const auto now=std::chrono::duration_cast<std::chrono::milliseconds>(
                        std::chrono::steady_clock::now().time_since_epoch()).count();
                    if(world.client_idle.poll_disconnect(now,world.client_idle_disconnect_enabled))
                        world.client_idle_disconnect_requested=true;
                }
                continue;
            }
            auto connection=c.shared_from_this();
            world.scheduler->callback(from,c.callback,push,[connection](){
                return !connection->manually_disconnected&&connection->enabled&&
                       (connection->connected||connection->auto_disconnected);
            },c.owner,c.identity,c.creator_ref,event.response_ready);
            --capacity;
            if(c.once){c.connected=false;c.auto_disconnected=true;}
        }
    }
    // Undispatched BindableEvent entries still own argument references.
    if(world.events.empty()) {
        for(int reference:world.event_argument_refs)lua_unref(from,reference);
        world.event_argument_refs.clear();
    }
    prune_disconnected_connections(from,world);
}
void progress_invocations(lua_State* from,World& world) {
    if(!world.scheduler)return;
    if(!world.bindable_invocations.empty())dispatch_bindable_functions(from,world,false);
    if(!world.remote_client_invocations.empty())dispatch_remote_client_functions(from,world);
}
void dispatch_events(lua_State* from,World& world) {
    if(!world.scheduler)return;
    if(world.client_idle_control_enabled) {
        const auto now=std::chrono::duration_cast<std::chrono::milliseconds>(
            std::chrono::steady_clock::now().time_since_epoch()).count();
        world.client_idle_event_due=world.client_idle.poll_idle(now);
        if(world.client_idle_event_due)if(auto* player=find_node(world,world.local_player);player&&!player->destroyed)
            emit(world,*player,"Idled",Json::array({world.client_idle.elapsed_seconds(now)}),LUA_NOREF,0,false,true);
    }
    process_debris(world,std::chrono::steady_clock::now());
    focused_textbox(world);
    flush_jump_requests(from,world);
    refresh_display_events(world);
    dispatch_bindable_functions(from,world);
    dispatch_remote_client_functions(from,world);
    dispatch_remote_server_waiters(from,world);
    advance_animations(world);
    advance_sounds(world);
    advance_tweens(from,world);
    flush_motor_transforms(world);
    refresh_attachment_events(world);
    refresh_joint_events(world);
    refresh_assembly_root_events(world);
    refresh_humanoid_root_part_events(world);
    refresh_part_mass_events(world);
    progress_virtual_input_playback(from,world);
    dispatch_pending_events(from,world);
    dispatch_context_actions(from,world);
    progress_virtual_input_waiters(from,world);
    // A headless source-scheduler tick, not a renderer or a physics step.
    // Polls may be slower than 60 Hz: report actual elapsed time once, without
    // flooding callbacks with synthetic catch-up frames after an idle period.
    auto heartbeat_now=std::chrono::steady_clock::now();
    if(world.heartbeat_previous==std::chrono::steady_clock::time_point{})
        world.heartbeat_previous=heartbeat_now;
    double heartbeat_dt=std::chrono::duration<double>(heartbeat_now-world.heartbeat_previous).count();
    if(world.render_previous==std::chrono::steady_clock::time_point{})world.render_previous=heartbeat_now;
    double render_dt=std::chrono::duration<double>(heartbeat_now-world.render_previous).count();
    if(world.render_steps.active&&world.render_steps.thread) {
        const char* state=world.scheduler->task_state(world.render_steps.thread);
        if(!std::strcmp(state,"completed")||!std::strcmp(state,"cancelled")||!std::strcmp(state,"untracked")) {
            world.render_steps.active=false;world.render_steps.group.clear();
        }
    }
    // Decide whether this poll may generate a simulation frame *before* the
    // render frame queues RenderStepped/PreRender. Checking world.events after
    // it starved Heartbeat on every poll where a render frame was due (i.e.
    // always, on hosts that poll slower than the render cap).
    const bool simulation_frame_allowed=world.events.empty()&&world.scheduler->callback_capacity()>0;
    if(world.events.empty()&&!world.context_actions.active&&world.context_actions.pending.empty()&&!world.render_steps.active&&world.scheduler->callback_capacity()>0&&render_dt>0&&(world.render_cap==0||render_dt>=1.0/world.render_cap)) {
        world.render_previous=heartbeat_now;auto service=world.services.find("RunService");
        if(service!=world.services.end())if(auto* node=find_node(world,service->second)) {
            start_render_frame(from,world,*node,render_dt);
        }
    }
    // The headless tick advances when callback dispatch has capacity. Keep
    // the previous timestamp while backpressured so the next actual frame
    // reports all elapsed time. Already queued events remain receipt ordered;
    // generating another frame here would amplify the backlog on every poll.
    if(simulation_frame_allowed&&heartbeat_dt>=1.0/60.0) {
        world.heartbeat_previous=heartbeat_now;
        world.heartbeat_delta=heartbeat_dt;
        world.simulation_time+=heartbeat_dt;
        auto service=world.services.find("RunService");
        // Roblox frame order: Stepped/PreSimulation, simulation, PostSimulation, Heartbeat.
        if(service!=world.services.end())if(auto* node=find_node(world,service->second)) {
            emit(world,*node,"Stepped",Json::array({world.simulation_time,heartbeat_dt}));
            emit(world,*node,"PreSimulation",Json::array({heartbeat_dt}));
            emit(world,*node,"PostSimulation",Json::array({heartbeat_dt}));
            emit(world,*node,"Heartbeat",Json::array({heartbeat_dt}));
        }
    }
    progress_text_service(from,world);
    for(auto it=world.chat_waiters.begin();it!=world.chat_waiters.end();) {
        bool finished=world.scheduler->completion(it->thread)!=LUA_YIELD;
        bool expired=std::chrono::steady_clock::now()>=it->deadline;
        if(finished||expired) {
            if(!finished)world.scheduler->wake_signal(it->thread,[](lua_State* target){lua_pushnil(target);lua_pushliteral(target,"server chat response timed out");return 2;});
            auto request=it->request;
            world.chat_requests.erase(std::remove_if(world.chat_requests.begin(),world.chat_requests.end(),[&](const Json& item){return item.at("request_id")==request;}),world.chat_requests.end());
            lua_unref(from,it->reference);it=world.chat_waiters.erase(it);
        } else ++it;
    }
    for(auto it=world.handle_waiters.begin();it!=world.handle_waiters.end();) {
        Node* found=find_node(world,it->id);
        if(found||std::chrono::steady_clock::now()>=it->deadline) {
            world.scheduler->wake_signal(it->thread,[&](lua_State* target){if(found)push_node(target,world,found);else lua_pushnil(target);return 1;});
            lua_unref(from,it->reference);it=world.handle_waiters.erase(it);
        } else ++it;
    }
    for(auto it=world.child_waiters.begin();it!=world.child_waiters.end();) {
        Node* found=nullptr;
        for(const auto& id:world.order){Node& child=world.nodes.at(id);if(parent_id(world,child)==it->parent&&instance_name(world,child)==it->name){found=&child;break;}}
        if(found||std::chrono::steady_clock::now()>=it->deadline) {
            world.scheduler->wake_signal(it->thread,[&](lua_State* target){if(found)push_node(target,world,found);else lua_pushnil(target);return 1;});
            lua_unref(from,it->reference);it=world.child_waiters.erase(it);
        } else ++it;
    }
    dispatch_pending_events(from,world);
}
void complete_remote_server(lua_State* from,World& w,const Json& responses) {
    complete_remote_server_impl(from,w,responses);
}
void complete_text_service(lua_State* from,World& w,const Json& responses) {
    complete_text_service_impl(from,w,responses);
}
void complete_chat(lua_State* from,World& w,const Json& responses) {
    if(!w.chat_enabled||!w.persistent)throw std::runtime_error("chat bridge is disabled");
    if(!responses.is_array()||responses.size()>128)throw std::runtime_error("invalid chat response batch");
    for(const auto& response:responses) {
        uint64_t request=response.at("request_id").get<uint64_t>();
        auto waiter=std::find_if(w.chat_waiters.begin(),w.chat_waiters.end(),[&](const ChatWaiter& item){return item.request==request;});
        if(waiter==w.chat_waiters.end())continue;
        if(response.contains("error")) {
            auto error=response.at("error").get<std::string>();if(error.size()>1024)throw std::runtime_error("chat error exceeds limit");
            w.scheduler->wake_signal(waiter->thread,[&](lua_State* target){lua_pushnil(target);lua_pushlstring(target,error.data(),error.size());return 2;});
        } else {
            const auto& message=response.at("message");
            const bool universe=message.contains("route")&&message.at("route").is_string()
                &&message.at("route").get<std::string>()=="universe";
            if(message.contains("route")&&!message.at("route").is_string())throw std::runtime_error("chat response route must be a string");
            if(universe!=waiter->universe)throw std::runtime_error("chat response route mismatch");
            if(message.at("channel_id")!=waiter->channel)throw std::runtime_error("chat response channel mismatch");
            int status=message.at("status_value").get<int>();if(status<1||status>9)throw std::runtime_error("unsupported chat response status");
            if(w.nodes.size()>=kMaxNodes+1000)throw std::runtime_error("chat instance limit exceeded");
            Node node;node.id="chat-message:"+std::to_string(request);node.class_name="TextChatMessage";node.name="TextChatMessage";node.server_owned=false;
            for(const auto& field:std::array<std::pair<const char*,const char*>,4>{{{"Text","text"},{"Metadata","metadata"},{"PrefixText","prefix_text"},{"MessageId","message_id"}}}) {
                auto value=message.at(field.second).get<std::string>();if(value.size()>16384)throw std::runtime_error("chat message field exceeds limit");node.properties[field.first]=value;node.origins[field.first]="received";
            }
            node.properties["Status"]=Json{{"$chat_status",status}};
            node.properties["TextChannel"]=universe?Json(nullptr):Json{{"$ref",waiter->channel}};
            node.properties["TextSource"]=universe?Json(nullptr):(message.contains("source_id")&&!message["source_id"].is_null()?Json{{"$ref",message.at("source_id")}}:Json(nullptr));
            node.origins["Status"]="received";node.origins["TextChannel"]="received";node.origins["TextSource"]="received";
            auto [it,inserted]=w.nodes.emplace(node.id,std::move(node));if(!inserted)throw std::runtime_error("chat message ID collision");
            ++w.chat_messages;
            Node* created_message=&it->second;
            w.scheduler->wake_signal(waiter->thread,[&](lua_State* target){push_node(target,w,created_message);return 1;});
            if(!universe)if(auto* channel=find_node(w,waiter->channel))emit(w,*channel,"MessageReceived",Json::array({Json{{"$ref",it->first}}}));
            auto service=w.services.find("TextChatService");if(service!=w.services.end())if(auto* node=find_node(w,service->second))
                emit(w,*node,universe?"UniverseChatMessageReceived":"MessageReceived",Json::array({Json{{"$ref",it->first}}}));
        }
        lua_unref(from,waiter->reference);w.chat_waiters.erase(waiter);
    }
}
ValidatedDeltaView::ValidatedDeltaView(const Json& delta,const void* shared_world_view)
    :delta_(&delta),shared_world_view_(shared_world_view) {
    if(delta.dump().size()>8*1024*1024)
        throw std::runtime_error("world delta exceeds 8 MiB");
    if(!delta.is_object()||delta.value("version",0)!=1||!delta.value("delta",false)
       ||!delta.contains("actions")||!delta["actions"].is_array()||delta["actions"].size()>10000)
        throw std::runtime_error("invalid ordered world delta");
    if(delta.contains("local_player")&&!delta.at("local_player").is_null()
       &&!delta.at("local_player").is_string())
        throw std::runtime_error("local_player must be a session-owned reference or null");
    if(delta.contains("services")) {
        const auto& services=delta.at("services");
        if(!services.is_object()||services.size()>128)throw std::runtime_error("delta services must be a bounded object");
        for(auto it=services.begin();it!=services.end();++it) {
            if(it.key().empty()||it.key().size()>256||(!it.value().is_null()&&!it.value().is_string())
               ||(it.value().is_string()&&it.value().get_ref<const std::string&>().empty()))
                throw std::runtime_error("delta service entries must contain instance ids or null");
        }
    }
}
const Json& ValidatedDeltaView::received_part_pose_fields(const Json& action_fields,
                                                         const Json& frame) const {
    if(auto found=received_part_pose_fields_.find(&action_fields);
       found!=received_part_pose_fields_.end())return found->second;
    // Derive lazily at the original validation point, without caching errors.
    Json derived=part_fields_from_frame(frame);
    auto [inserted,_]=received_part_pose_fields_.emplace(&action_fields,std::move(derived));
    return inserted->second;
}

void update_snapshot(lua_State* from,World& w,const std::string& text,bool defer_camera_refresh,
                     Json* deferred_animation_responses) {
    if(text.size()>8*1024*1024)throw std::runtime_error("world delta exceeds 8 MiB");
    Json delta=Json::parse(text);
    const ValidatedDeltaView validated(delta);
    update_snapshot(from,w,validated,defer_camera_refresh,deferred_animation_responses);
}

void update_snapshot(lua_State* from,World& w,const ValidatedDeltaView& validated,
                     bool defer_camera_refresh,Json* deferred_animation_responses) {
    SharedJsonScope json_scope;
    HierarchyEventBatch hierarchy_batch(w);
    const Json& delta=validated.value();
    if(delta.contains("session_stats"))w.session_stats=validated_session_stats(delta.at("session_stats"));
    if(delta.contains("input_capabilities"))update_input_capabilities(w,delta.at("input_capabilities"));
    if(delta.contains("input_events"))update_input_events(w,delta.at("input_events"));
    if(delta.contains("viewport")){update_viewport(w,delta.at("viewport"));w.display=display_from_viewport(w.viewport);}
    if(delta.contains("display"))update_display(w,delta.at("display"));
    if(delta.contains("animation_responses")) {
        if(deferred_animation_responses)*deferred_animation_responses=delta.at("animation_responses");
        else complete_animations(w,delta.at("animation_responses"));
    }
    const bool initial_ready=delta.value("initial_replication_complete",false);
    std::unordered_set<std::string> received_joint_poses,received_part_poses;
    std::unordered_set<std::string> refresh_rust_nodes;
    std::unordered_map<std::string,CFrameValue> received_root_frames; // pre-update frames of moved parts
    Children removal_children;
    bool removal_children_ready=false;
    for(const auto& action:delta["actions"]) {
        uint64_t sequence=action.at("sequence").get<uint64_t>();
        if(sequence<=w.sequence)continue;
        std::string kind=action.at("kind").get<std::string>();
        std::string id=action.contains("id")&&!action["id"].is_null()?action["id"].get<std::string>():"";
        if(!id.empty()&&kind!="remove")refresh_rust_nodes.insert(id);
        if(kind=="create") {
            removal_children_ready=false;
            Node incoming=parse_node_record(action.at("node"),{},validated.shared_world_view());incoming.server_owned=true;
            if(!incoming.rust_world_node)incoming.origins=w.received_origin_pool.intern(incoming.origins.read());
            refresh_rust_nodes.insert(incoming.id);
            Node* existing=find_node(w,incoming.id);
            if(existing&&existing->class_name!=incoming.class_name)throw std::runtime_error("replicated instance changed class");
            if(!w.locally_destroyed.count(incoming.id)) {
                w.joint_topology_dirty=true;
                if(joint_rigid(incoming)&&incoming.class_name!="WeldConstraint") {
                    received_joint_poses.insert(incoming.id);
                    if(existing)remember_received_motor_transform(w,*existing,incoming.properties);
                }
                if(pivot_part(w,incoming)&&incoming.properties.contains("CFrame"))received_part_poses.insert(incoming.id);
            }
            if(!existing) {
                if(w.nodes.size()>=kMaxNodes+1000)throw std::runtime_error("persistent instance limit exceeded");
                id=incoming.id;std::string parent=incoming.parent;incoming.parent.clear();
                auto [it,ok]=w.nodes.emplace(id,std::move(incoming));w.order.push_back(id);
                it->second.parent=parent;parent_events(w,it->second,"",parent);
            } else if(!w.locally_destroyed.count(id)) {
                auto old_parent=parent_id(w,*existing);auto old_name=instance_name(w,*existing);
                const bool was_script_disabled=script_disabled_state(w,*existing);
                const uint64_t old_disable_generation=existing->script_state.disabled_generation();
                auto attributes=existing->attributes;
                auto tags=existing->collection_tags;bool tags_known=existing->collection_tags_known;
                *existing=std::move(incoming);w.local_properties.erase(id);
                existing->script_state.set_disabled_generation(old_disable_generation);
                if(!action.at("node").contains("attributes"))existing->attributes=std::move(attributes);
                if(!action.at("node").contains("collection_tags")){existing->collection_tags=std::move(tags);existing->collection_tags_known=tags_known;}
                if(old_name!=existing->name)property_events(w,*existing,"Name");
                parent_events(w,*existing,old_parent,existing->parent);
                notify_script_disabled_transition(from,w,*existing,was_script_disabled);
            }
        } else if(kind=="set") {
            Node* n=find_node(w,id);
            if(n&&!n->destroyed&&!w.locally_destroyed.count(id)) {
                const auto& fields=action.at("fields");if(!fields.is_object())throw std::runtime_error("world fields must be an object");
                const bool was_script_disabled=script_disabled_state(w,*n);
                const bool changes_content=text_layout_node(*n)&&(fields.contains("Text")||fields.contains("RichText"));
                const auto old_content=changes_content?text_content(w,*n):std::nullopt;
                const Json* received_color=fields.contains("Color")?nullptr:received_color_alias(*n,fields);
                const Json received_group=fields.contains("CollisionGroup")?Json(nullptr):received_collision_group_alias(*n,fields);
                if(fields.contains("Parent"))removal_children_ready=false;
                std::optional<CFrameValue> frame_before;
                if(pivot_part(w,*n)&&!received_root_frames.count(id))try{frame_before=pivot_frame(w,*n,"CFrame");}catch(const std::exception&){}
                const bool part_updated=apply_received_part_transform(w,*n,fields,sequence,&validated);
                if(part_updated){received_part_poses.insert(id);if(frame_before)received_root_frames.emplace(id,*frame_before);}
                const bool attachment_updated=apply_received_attachment_transform(w,*n,fields,sequence);
                const bool text_font_updated=apply_received_text_font(w,*n,fields,sequence);
                if(joint_rigid(*n)&&n->class_name!="WeldConstraint"&&(fields.contains("C0")||fields.contains("C1")||(n->class_name=="Motor6D"&&fields.contains("Transform")))) {
                    remember_received_motor_transform(w,*n,fields);
                    received_joint_poses.insert(id);
                }
                for(auto it=fields.begin();it!=fields.end();++it) {
                    const std::string name=(text_layout_node(*n)&&it.key()=="TextWrap")?"TextWrapped":it.key();const Json value=it.value();
                    if(text_layout_node(*n)&&it.key()=="TextWrap"&&fields.contains("TextWrapped"))continue;
                    if(text_font_updated&&(name=="Font"||name=="FontFace"))continue;
                    if(attachment_updated&&attachment_local_key(name))continue;
                    if(attachment_node(w,*n)&&(attachment_world_key(name)||bone_computed_key(*n,name)))continue;
                    if(part_updated&&(part_transform_key(name)||(name=="ReplicationPV"&&value.is_object()&&value.contains("$cframe"))))continue;
                    if(name=="Source"||name=="Bytecode"||name=="ScriptBytecode")continue;
                    if(n->class_name=="UserInputService"&&(input_capability_key(name)||name=="PreferredInput"))continue;
                    if(display_readonly_property(w,*n,name))continue;
                    if((name=="ContentText"&&text_layout_node(*n))||textbox_selection_key(*n,name)||
                       (name=="IsLoaded"&&gui_image(*n)))continue;
                    if(n->class_name=="Camera"&&name=="ViewportSize")continue;
                    if(name=="Parent") {
                        std::string parent=value.is_null()?"":value.at("$ref").get<std::string>();
                        auto old=parent_id(w,*n);auto* target=find_node(w,parent);
                        if(target&&(target==n||is_descendant_of(w,*target,id)))throw std::runtime_error("received Parent would create a cycle in the client model");
                        clear_local_property(w,id,"Parent");n->parent=parent;n->revisions[name]=sequence;n->origins[name]="received";
                        parent_events(w,*n,old,parent);
                    } else {
                        Json old=name=="Name"?Json(instance_name(w,*n)):property_value(w,*n,name)?*property_value(w,*n,name):Json(nullptr);
                        clear_local_property(w,id,name);n->properties[name]=value;n->revisions[name]=sequence;n->origins[name]="received";
                        if(name=="Name"){if(!value.is_string())throw std::runtime_error("received Name must be text");n->name=value.get<std::string>();}
                        bool deferred_character=n->class_name=="Player"&&name=="Character"&&value.is_object()
                            &&value.contains("$ref")&&value["$ref"].is_string()&&!find_node(w,value["$ref"].get<std::string>());
                        if(old!=value&&!deferred_character) {
                            if(!(n->class_name=="ScrollingFrame"&&name=="CanvasPosition"))property_events(w,*n,name);
                            humanoid_received_health_change(w,*n,name,old,value);
                            if(name=="Text"&&n->class_name=="TextBox")authored_textbox_text_changed(w,*n);
                        }
                        if(n->class_name=="Player"&&name=="Character"&&old!=value) {
                            w.pending_characters.erase(id);
                            if(old.is_object()&&old.contains("$ref")&&old["$ref"].is_string()&&find_node(w,old["$ref"].get<std::string>()))emit(w,*n,"CharacterRemoving",Json::array({old}));
                            if(deferred_character)w.pending_characters[id]=value["$ref"].get<std::string>();
                            else if(!value.is_null())emit(w,*n,"CharacterAdded",Json::array({value}));
                        }
                        if((name=="CFrame"||name=="ReplicationPV")&&value.is_object()&&value.contains("$cframe")) {
                            const auto& frame=value["$cframe"];
                            if(frame.is_object()&&frame.contains("position")) {
                                Json position{{"$vector3",frame["position"]}};
                                const auto* previous=property_value(w,*n,"Position");bool changed=!previous||*previous!=position;
                                clear_local_property(w,id,"Position");n->properties["Position"]=position;
                                n->revisions["Position"]=sequence;n->origins["Position"]="derived_received";
                                if(changed)property_events(w,*n,"Position");
                                if(name=="ReplicationPV") {
                                    clear_local_property(w,id,"CFrame");n->properties["CFrame"]=value;
                                    n->revisions["CFrame"]=sequence;n->origins["CFrame"]="derived_received";property_events(w,*n,"CFrame");
                                }
                            }
                        }
                    }
                }
                if(fields.contains("Disabled")||fields.contains("Enabled"))
                    notify_script_disabled_transition(from,w,*n,was_script_disabled);
                if(changes_content&&old_content!=text_content(w,*n))property_events(w,*n,"ContentText");
                if(received_color) {
                    const Json* previous=property_value(w,*n,"Color");bool changed=!previous||*previous!=*received_color;
                    clear_local_property(w,id,"Color");n->properties["Color"]=*received_color;
                    n->origins["Color"]="derived_received";n->revisions["Color"]=sequence;
                    if(changed)property_events(w,*n,"Color");
                }
                if(!received_group.is_null()) {
                    const Json* previous=property_value(w,*n,"CollisionGroup");bool changed=!previous||*previous!=received_group;
                    clear_local_property(w,id,"CollisionGroup");n->properties["CollisionGroup"]=received_group;
                    n->origins["CollisionGroup"]="derived_received";n->revisions["CollisionGroup"]=sequence;
                    if(changed)property_events(w,*n,"CollisionGroup");
                }
                if(action.contains("attribute_snapshot")) {
                    const auto& snapshot=action.at("attribute_snapshot");
                    const auto& incoming=snapshot.at("attributes");
                    if(!incoming.is_object()||incoming.size()>1024||!snapshot.at("attributes_known").is_boolean())
                        throw std::runtime_error("invalid received attribute snapshot");
                    std::set<std::string> changed;
                    const Json previous=n->attributes.snapshot();
                    for(auto it=previous.begin();it!=previous.end();++it)
                        if(!incoming.contains(it.key())||incoming.at(it.key())!=*it)changed.insert(it.key());
                    for(auto it=incoming.begin();it!=incoming.end();++it)
                        if(!previous.contains(it.key())||previous.at(it.key())!=*it)changed.insert(it.key());
                    n->attributes.replace(incoming,snapshot.at("attributes_known").get<bool>());
                    for(const auto& name:changed){emit(w,*n,"AttributeChanged",Json::array({name}));emit(w,*n,"attribute:"+name);}
                }
                if(action.contains("tag_snapshot"))collection_received_state(w,*n,action.at("tag_snapshot"));
            }
        } else if(kind=="attribute") {
            Node* n=find_node(w,id);
            if(n&&!n->destroyed&&!w.locally_destroyed.count(id)) {
                const auto& update=action.at("attribute_update");
                const auto name=update.at("name").get<std::string>();
                const bool known=update.at("known").get<bool>();
                if(name.empty()||name.size()>100||name.find('\0')!=std::string::npos)
                    throw std::runtime_error("invalid received attribute name");
                const bool had=n->attributes.contains(name);
                if(known) {
                    const auto& value=update.at("value");
                    if(!had&&(!value.is_null()||!n->attributes.known())&&n->attributes.size()>=1024)
                        throw std::runtime_error("attribute count limit exceeded");
                    const bool changed=n->attributes.set(name,value);
                    if(changed){emit(w,*n,"AttributeChanged",Json::array({name}));emit(w,*n,"attribute:"+name);}
                } else {
                    n->attributes.erase(name);n->attributes.set_known(false);
                    if(had){emit(w,*n,"AttributeChanged",Json::array({name}));emit(w,*n,"attribute:"+name);}
                }
            }
        } else if(kind=="remove") {
            Node* n=find_node(w,id);
            if(n&&!n->destroyed&&!w.locally_destroyed.count(id)) {
                // Reuse the parent index for a consecutive stream-out batch.
                // Creates/reparents invalidate it before the next removal.
                if(!removal_children_ready){removal_children=children_index(w);removal_children_ready=true;}
                std::deque<std::string> pending{id};
                while(!pending.empty()) {
                    auto key=pending.front();pending.pop_front();Node& node=w.nodes.at(key);
                    if(node.destroyed)continue;
                    auto child=removal_children.find(key);if(child!=removal_children.end())for(const auto& value:child->second)pending.push_back(value);
                    emit(w,node,"Destroying");parent_events(w,node,parent_id(w,node),"");
                    node.parent.clear();node.destroyed=true;clear_local_property(w,key,"Parent");
                    if(key==w.local_player) {
                        w.client_idle_control_enabled=false;
                        w.client_idle_event_due=false;
                        w.client_idle_disconnect_requested=false;
                        w.client_idle_player_id.clear();
                    }
                    for(auto& c:w.connections)if(c->signal->node==&node){c->connected=false;c->auto_disconnected=true;}
                }
            }
        } else if(kind=="event") {
            ++w.server_events;Node* n=find_node(w,id);
            if(n&&!n->destroyed){
                const auto& args=action.at("arguments");if(!args.is_array())throw std::runtime_error("event arguments must be an array");
                const auto name=action.at("name").get<std::string>();
                if(n->class_name=="RemoteFunction"&&name=="RemoteOnInvokeClient")queue_remote_client_invoke(from,w,*n,args);
                else emit(w,*n,name,args);
            }
            else ++w.unresolved_server_events;
        } else throw std::runtime_error("unsupported world action kind");
        w.sequence=static_cast<double>(sequence);++w.server_actions;
    }
    if(delta.contains("services"))for(auto it=delta.at("services").begin();it!=delta.at("services").end();++it) {
        if(it.value().is_null()){w.services.erase(it.key());continue;}
        const std::string id=it.value().get<std::string>();
        Node* service=find_node(w,id);
        if(!service||service->destroyed||service->class_name!=it.key()||id==w.game_id)
            throw std::runtime_error("delta service reference is invalid");
        if(service->parent.empty())service->parent=w.game_id;
        w.services[it.key()]=id;
    }
    for(auto it=w.services.begin();it!=w.services.end();) {
        Node* service=find_node(w,it->second);
        if(!service||service->destroyed||service->class_name!=it->first)it=w.services.erase(it);
        else ++it;
    }
    // Drop frame-local overlays from Rust-backed received state and bind each
    // changed instance to its final immutable record. Local properties remain
    // in World::local_properties and continue to win in property_value().
    if(const void* view=validated.shared_world_view())for(const auto& id:refresh_rust_nodes) {
        Node* node=find_node(w,id);if(!node||node->destroyed||!node->server_owned)continue;
        auto bundle=rust_world_lookup_bundle(view,id);if(!bundle)continue;
        Json header=bundle->node().header();
        node->rust_world_node=RustNodeProviderBundle::node_view(bundle);
        bind_rust_node_properties(*node,bundle);
        Json alias_fields=Json::object();
        for(const char* key:{"TextWrap","Color3uint8","CollisionGroupReplicate"})if(const Json* v=node->properties.lookup(key))alias_fields[key]=*v;
        if(text_layout_node(*node)&&node->properties.contains("TextWrap")&&!node->properties.contains("TextWrapped")) {
            node->properties["TextWrapped"]=*node->properties.lookup("TextWrap");
            node->origins["TextWrapped"]="derived_received";
            if(node->revisions.contains("TextWrap"))node->revisions["TextWrapped"]=node->revisions["TextWrap"];
        }
        if(!node->properties.contains("Color"))if(const Json* color=received_color_alias(*node,alias_fields)) {
            node->properties["Color"]=*color;node->origins["Color"]="derived_received";
            if(node->revisions.contains("Color3uint8"))node->revisions["Color"]=node->revisions["Color3uint8"];
        }
        if(!node->properties.contains("CollisionGroup"))if(Json group=received_collision_group_alias(*node,alias_fields);!group.is_null()) {
            node->properties["CollisionGroup"]=std::move(group);node->origins["CollisionGroup"]="derived_received";
            if(node->revisions.contains("CollisionGroupReplicate"))node->revisions["CollisionGroup"]=node->revisions["CollisionGroupReplicate"];
        }
        node->properties.clear_transient_cache();
        node->origins.clear_transient_cache();
        node->revisions.clear_transient_cache();
    }
    // The Rust session resolves the authenticated UserId. This bridge only
    // binds that explicit reference after its ordered create actions exist.
    if(delta.contains("local_player")) {
        const auto& binding=delta.at("local_player");
        const std::string id=binding.is_null()?"":binding.get<std::string>();
        Node* player=id.empty()?nullptr:find_node(w,id);
        if(!id.empty()&&(!player||player->destroyed||player->class_name!="Player"))
            throw std::runtime_error("session local_player does not identify a live Player");
        if(w.local_player!=id) {
            w.local_player=id;
            w.client_idle_control_enabled=false;
            w.client_idle_event_due=false;
            w.client_idle_disconnect_requested=false;
            w.client_idle_player_id.clear();
            if(player&&w.actor_id.empty()&&!w.actor_runtime)(void)signal_for(w,*player,"Idled");
            auto players=w.services.find("Players");
            if(players!=w.services.end())if(auto* service=find_node(w,players->second))
                property_events(w,*service,"LocalPlayer");
        }
    }
    for(auto it=w.pending_characters.begin();it!=w.pending_characters.end();) {
        Node* player=find_node(w,it->first);Node* character=find_node(w,it->second);
        auto* value=player?property_value(w,*player,"Character"):nullptr;
        Json expected{{"$ref",it->second}};
        if(!player||player->destroyed||!value||*value!=expected)it=w.pending_characters.erase(it);
        else if(character){property_events(w,*player,"Character");emit(w,*player,"CharacterAdded",Json::array({expected}));it=w.pending_characters.erase(it);}
        else ++it;
    }
    // Loaded describes completion of the initial replication, not character
    // spawning or later streaming. Apply the received actions before waking
    // listeners; repeated markers cannot fire this transition again.
    if(!defer_camera_refresh)refresh_current_camera(w);
    // Complete the ordered input transaction before deriving poses or waking
    // listeners. Explicit received part poses win within this transaction.
    // Carry first: dirty joints in the same batch are then solved on top of it.
    carry_received_assemblies(w,received_root_frames,received_part_poses,static_cast<uint64_t>(w.sequence));
    flush_joint_poses(w,std::move(received_joint_poses),received_part_poses,static_cast<uint64_t>(w.sequence));
    if(initial_ready&&!w.initial_replication_complete) {
        w.initial_replication_complete=true;
        emit(w,w.nodes.at(w.game_id),"Loaded");
    }
    (void)from;
}
#include "render_surface.inc"
} // namespace world_bindings
