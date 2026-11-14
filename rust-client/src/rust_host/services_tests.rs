use super::{
    services, signals,
    world::{self, World, WorldRef},
};
use mlua::{Lua, Value};
use serde_json::json;
use std::{cell::RefCell, rc::Rc};

fn fixture() -> WorldRef {
    let snapshot = json!({"version":1,"sequence":1,"services":{
        "Players":"players","HttpService":"http","CollectionService":"collection",
        "RunService":"run","TestService":"test","Debris":"debris","Stats":"stats"
    },"nodes":[
        {"id":"game","class_name":"DataModel","name":"game","parent":null,"properties":{},"server_owned":true},
        {"id":"players","class_name":"Players","name":"Players","parent":"game","properties":{},"server_owned":true},
        {"id":"http","class_name":"HttpService","name":"HttpService","parent":"game","properties":{},"server_owned":true},
        {"id":"collection","class_name":"CollectionService","name":"CollectionService","parent":"game","properties":{},"server_owned":true},
        {"id":"run","class_name":"RunService","name":"RunService","parent":"game","properties":{},"server_owned":true},
        {"id":"test","class_name":"TestService","name":"TestService","parent":"game","properties":{},"server_owned":true},
        {"id":"debris","class_name":"Debris","name":"Debris","parent":"game","properties":{},"server_owned":true},
        {"id":"stats","class_name":"Stats","name":"Stats","parent":"game","properties":{},"server_owned":true},
        {"id":"network","class_name":"Folder","name":"Network","parent":"stats","properties":{},"server_owned":true},
        {"id":"server_stats","class_name":"Folder","name":"ServerStatsItem","parent":"network","properties":{},"server_owned":true},
        {"id":"ping","class_name":"StatsItem","name":"Data Ping","parent":"server_stats","properties":{},"server_owned":true},
        {"id":"player","class_name":"Player","name":"Tester","parent":"players","properties":{"UserId":42},"server_owned":true},
        {"id":"part","class_name":"Part","name":"TaggedPart","parent":"game","properties":{},"server_owned":true,"collection_tags_known":true,"collection_tags":[]}
    ]});
    Rc::new(RefCell::new(World::from_snapshot(&snapshot, None).unwrap()))
}

fn setup() -> (Lua, WorldRef) {
    let lua = Lua::new();
    let world = fixture();
    world::install(&lua, &world).unwrap();
    signals::install(&lua, &world).unwrap();
    services::install(&lua, &world).unwrap();
    (lua, world)
}

#[test]
fn http_service_json_and_url_methods_use_bounded_codec() {
    let (lua, _world) = setup();
    let encoded: String = lua
        .load(
            r#"
        local http = game:GetService("HttpService")
        return http:JSONEncode({ok=true, values={1, 2}})
    "#,
        )
        .eval()
        .unwrap();
    assert_eq!(encoded, r#"{"ok":true,"values":[1,2]}"#);
    let url: String = lua
        .load(r#"return game:GetService("HttpService"):UrlEncode("hi there!")"#)
        .eval()
        .unwrap();
    assert_eq!(url, "hi%20there%21");
    let decoded: Value = lua
        .load(r#"return game:GetService("HttpService"):JSONDecode("[1,2]")"#)
        .eval()
        .unwrap();
    assert!(matches!(decoded, Value::Table(_)));
}

#[test]
fn collection_service_preserves_unknown_tag_state_and_records_local_changes() {
    let (lua, world) = setup();
    let bootstrap_mutations = world.borrow().effects["mutations"]
        .as_array()
        .unwrap()
        .len();
    let unknown = lua.load(r#"return game:GetService("CollectionService"):GetTags(game:GetService("Players"):GetPlayers()[1])"#).eval::<Value>();
    assert!(unknown.is_err());
    lua.load(r#"game:GetService("CollectionService"):AddTag(game:FindFirstChild("TaggedPart"), "test-tag")"#).exec().unwrap();
    let tagged: bool = lua.load(r#"return game:GetService("CollectionService"):HasTag(game:FindFirstChild("TaggedPart"), "test-tag")"#).eval().unwrap();
    assert!(tagged);
    assert_eq!(
        world.borrow().effects["mutations"][bootstrap_mutations],
        json!({"kind":"set_tags","id":"part","tags":["test-tag"]})
    );
}

#[test]
fn players_and_runservice_reflect_fixture_and_local_client_mode() {
    let (lua, _world) = setup();
    let count: i64 = lua
        .load(r#"return #game:GetService("Players"):GetPlayers()"#)
        .eval()
        .unwrap();
    assert_eq!(count, 1);
    let user_id: i64 = lua
        .load(r#"return game:GetService("Players"):GetPlayerByUserId(42).UserId"#)
        .eval()
        .unwrap();
    assert_eq!(user_id, 42);
    let is_client: bool = lua
        .load(r#"return game:GetService("RunService"):IsClient()"#)
        .eval()
        .unwrap();
    let is_server: bool = lua
        .load(r#"return game:GetService("RunService"):IsServer()"#)
        .eval()
        .unwrap();
    assert!(is_client);
    assert!(!is_server);
}

#[test]
fn user_settings_creates_only_local_defaults() {
    let (lua, world) = setup();
    let sensitivity: f64 = lua
        .load(r#"return UserSettings():GetService("UserGameSettings").MouseSensitivity"#)
        .eval()
        .unwrap();
    assert_eq!(sensitivity, 1.0);
    let settings_id = world.borrow().services["UserSettings"].clone();
    assert!(!world.borrow().nodes[&settings_id].server_owned);
    let writable: bool = lua
        .load(
            r#"
            local settings = UserSettings():GetService("UserGameSettings")
            settings.MouseSensitivity = 1.5
            return settings.MouseSensitivity == 1.5
        "#,
        )
        .eval()
        .unwrap();
    assert!(writable);
    let readonly = lua
        .load(
            r#"
            local settings = UserSettings():GetService("UserGameSettings")
            settings.IsUsingCameraYInverted = true
        "#,
        )
        .exec();
    assert!(readonly.is_err());
}

#[test]
fn stats_reads_only_fixture_measured_rtt() {
    let (lua, world) = setup();
    world.borrow_mut().snapshot["session_stats"] = json!({"network_rtt_ms":28.375});
    let value: f64 = lua.load(r#"return game:GetService("Stats").Network:FindFirstChild("ServerStatsItem"):FindFirstChild("Data Ping"):GetValue()"#).eval().unwrap();
    assert_eq!(value, 28.375);
}

#[test]
fn stats_globals_use_the_same_service_and_only_actual_session_values() {
    let (lua, world) = setup();
    lua.load(
        r#"
        assert(Stats == stats and Stats() == game:GetService('Stats'))
        local ping = stats().Network.ServerStatsItem['Data Ping']
        assert(not pcall(function() return ping:GetValue() end))
    "#,
    )
    .exec()
    .unwrap();
    world.borrow_mut().snapshot["session_stats"] = json!({"network_rtt_ms":31.25});
    assert_eq!(
        lua.load("return Stats().Network.ServerStatsItem['Data Ping']:GetValue()")
            .eval::<f64>()
            .unwrap(),
        31.25
    );
}

#[test]
fn stats_globals_create_one_local_service_when_snapshot_has_none() {
    let world = Rc::new(RefCell::new(
        World::from_snapshot(&json!({"version":1,"nodes":[],"services":{}}), None).unwrap(),
    ));
    let lua = Lua::new();
    signals::install(&lua, &world).unwrap();
    world::install(&lua, &world).unwrap();
    services::install(&lua, &world).unwrap();
    lua.load(
        r#"
        assert(game:FindService('Stats') == nil)
        local service = stats()
        assert(service == Stats() and service == game:GetService('Stats'))
        assert(service:IsA('Stats') and service.Parent == game)
    "#,
    )
    .exec()
    .unwrap();
    let state = world.borrow();
    let nodes: Vec<_> = state
        .nodes
        .values()
        .filter(|node| node.class_name == "Stats")
        .collect();
    assert_eq!(nodes.len(), 1);
    assert!(!nodes[0].server_owned);
    assert!(state.snapshot.get("session_stats").is_none());
}

#[test]
fn debris_zero_lifetime_destroys_the_local_item_on_tick() {
    let (lua, world) = setup();
    lua.load(
        r#"
        local item = Instance.new("Part")
        game:GetService("Debris"):AddItem(item, 0)
    "#,
    )
    .exec()
    .unwrap();
    services::tick(&lua, &world);
    assert!(world
        .borrow()
        .nodes
        .values()
        .any(|node| node.class_name == "Part" && node.destroyed));
}

#[test]
fn local_player_unknown_is_nil_and_user_settings_use_internal_local_nodes() {
    let (lua, world) = setup();
    let unknown: Value = lua
        .load("return game:GetService('Players').LocalPlayer")
        .eval()
        .unwrap();
    assert!(matches!(unknown, Value::Nil));
    let (settings_name, game_name, sensitivity): (String, String, f64) = lua
        .load(
            r#"
        local settings=UserSettings()
        local gameSettings=settings:FindFirstChild('GameSettings')
        return settings.Name, gameSettings.Name, gameSettings.MouseSensitivity
    "#,
        )
        .eval()
        .unwrap();
    assert_eq!(settings_name, "GlobalBasicSettings");
    assert_eq!(game_name, "GameSettings");
    assert_eq!(sensitivity, 1.0);
    assert!(world
        .borrow()
        .nodes
        .values()
        .any(|n| n.class_name == "UserSettings"));
    assert!(world
        .borrow()
        .nodes
        .values()
        .any(|n| n.class_name == "UserGameSettings"));
}

#[test]
fn datamodel_is_loaded_reflects_only_received_replication_state() {
    let (lua, world) = setup();
    let before: bool = lua.load("return game:IsLoaded()").eval().unwrap();
    assert!(!before);
    world.borrow_mut().snapshot["initial_replication_complete"] = json!(true);
    let after: bool = lua.load("return game:IsLoaded()").eval().unwrap();
    assert!(after);
}

#[test]
fn engine_globals_are_local_singletons_with_explicit_unavailable_operations() {
    let (lua, world) = setup();
    let bootstrap_mutations = world.borrow().effects.get("mutations").cloned();
    lua.load(r#"
        local global, plugins = settings(), PluginManager()
        assert(global == settings() and plugins == PluginManager())
        assert(global.Name == 'GlobalSettings' and global.ClassName == 'GlobalSettings')
        assert(global:IsA('GenericSettings') and global:IsA('ServiceProvider'))
        assert(plugins.Name == 'PluginManager' and plugins:IsA('Instance'))
        assert(global.Parent == nil and plugins.Parent == nil)
        assert(game:FindService('PluginManager') == nil)
        local ok, message = pcall(function() global:GetFFlag('missing') end)
        assert(not ok and string.find(tostring(message), 'engine flag values are unavailable', 1, true))
        ok, message = pcall(function() plugins:CreatePlugin() end)
        assert(not ok and string.find(tostring(message), 'PluginSecurity is unavailable', 1, true))
        assert(not pcall(settings, true) and not pcall(PluginManager, 'x'))
    "#).exec().unwrap();
    assert_eq!(
        world.borrow().effects.get("mutations"),
        bootstrap_mutations.as_ref()
    );
}
