//! Rust-owned world and Luau Instance handles.
//!
//! Server supplied state stays in the canonical projected world. Each `Node`
//! retains at most a projected per-record handle; only client-local writes are
//! copied into `local_overrides`.

use crate::engine_world_projection::ProjectedNode;
use crate::engine_world_view::SharedProjectedWorldView;
use mlua::{
    AnyUserData, Lua, MetaMethod, MultiValue, ObjectLike, Thread, UserData, UserDataFields,
    UserDataMethods, Value,
};
use rove_client_core::world::ReadOnlyField;
use serde_json::{json, Map as JsonMap, Value as Json};
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use std::{
    cell::RefCell,
    collections::HashMap,
    rc::{Rc, Weak},
};

#[path = "world_queries.rs"]
mod world_queries;

pub type WorldRef = Rc<RefCell<World>>;
type InstanceHandleWaiters = Rc<RefCell<InstanceHandleWaitState>>;
#[derive(Default)]
struct InstanceHandleWaitState {
    next_id: u64,
    pending: Vec<InstanceHandleWaiter>,
}
struct InstanceHandleWaiter {
    token: u64,
    id: String,
    world: Weak<RefCell<World>>,
    thread: Thread,
    started: Instant,
    timeout_seconds: Option<f64>,
    timer: Option<Thread>,
}
pub type Method = fn(&Lua, &WorldRef, &str, MultiValue) -> mlua::Result<MultiValue>;
type MethodCallback = Rc<dyn Fn(&Lua, &WorldRef, &str, MultiValue) -> mlua::Result<MultiValue>>;
pub type Getter = fn(&Lua, &WorldRef, &str) -> mlua::Result<Option<Value>>;
pub type KeyedGetter = fn(&Lua, &WorldRef, &str, &str) -> mlua::Result<Option<Value>>;
pub type Setter = fn(&Lua, &WorldRef, &str, Value) -> mlua::Result<bool>;
pub type KeyedSetter = fn(&Lua, &WorldRef, &str, &str, Value) -> mlua::Result<bool>;

#[derive(Clone)]
pub struct Node {
    pub id: String,
    pub class_name: String,
    pub name: String,
    pub parent: Option<String>,
    pub destroyed: bool,
    pub server_owned: bool,
    /// Set only for client-local objects. Received properties are read lazily.
    pub properties: JsonMap<String, Json>,
    pub local_overrides: JsonMap<String, Json>,
    pub attributes: JsonMap<String, Json>,
    pub attributes_known: bool,
    pub revisions: JsonMap<String, Json>,
    pub local_revision: u64,
    pub collection_tags: Vec<String>,
    pub collection_tags_known: bool,
    pub(crate) projected: Option<ProjectedNode>,
}

pub struct World {
    pub nodes: HashMap<String, Node>,
    pub services: HashMap<String, String>,
    pub snapshot: Json,
    pub sequence: u64,
    lightweight_assets: bool,
    pub effects: JsonMap<String, Json>,
    pub focused_textbox: Option<String>,
    pub focused_text: String,
    pub focused_text_revision: u64,
    pub focused_editor_revision: u64,
    pub focused_cursor: i32,
    pub focused_anchor: i32,
    pub(crate) shared: Option<SharedProjectedWorldView>,
    pub(crate) methods: HashMap<(String, String), MethodCallback>,
    pub(crate) getters: HashMap<String, Vec<Getter>>,
    pub(crate) keyed_getters: HashMap<String, Vec<KeyedGetter>>,
    pub(crate) setters: HashMap<String, Vec<Setter>>,
    pub(crate) keyed_setters: HashMap<String, Vec<KeyedSetter>>,
    local_namespace: String,
    next_local_id: u64,
    node_order: Vec<String>,
    local_revision: u64,
}

impl World {
    /// Fork the current instance graph for a separate Luau state. Projected
    /// nodes keep only their individual shared record handles; VM registries,
    /// hook tables, effects, and the parent WorldView owner are not shared.
    pub fn fork_for_vm(&self) -> World {
        World {
            nodes: self.nodes.clone(),
            services: self.services.clone(),
            snapshot: self.snapshot.clone(),
            sequence: self.sequence,
            lightweight_assets: self.lightweight_assets,
            effects: JsonMap::new(),
            focused_textbox: self.focused_textbox.clone(),
            focused_text: self.focused_text.clone(),
            focused_text_revision: self.focused_text_revision,
            focused_editor_revision: self.focused_editor_revision,
            focused_cursor: self.focused_cursor,
            focused_anchor: self.focused_anchor,
            shared: None,
            methods: HashMap::new(),
            getters: HashMap::new(),
            keyed_getters: HashMap::new(),
            setters: HashMap::new(),
            keyed_setters: HashMap::new(),
            local_namespace: self.local_namespace.clone(),
            next_local_id: self.next_local_id,
            node_order: self.node_order.clone(),
            local_revision: self.local_revision,
        }
    }

    pub fn fork_for_actor(&self, actor_id: &str) -> World {
        let mut fork = self.fork_for_vm();
        let mut encoded = String::with_capacity(actor_id.len() * 2);
        for byte in actor_id.as_bytes() {
            use std::fmt::Write as _;
            let _ = write!(&mut encoded, "{byte:02x}");
        }
        fork.local_namespace = format!("local:actor:{encoded}");
        fork
    }

    /// Refresh Actor state from root without flattening projected records or
    /// discarding Actor-local instances, overlays, and VM-owned registries.
    pub fn sync_actor_fork(&mut self, root: &World) {
        let actor_prefix = format!("{}:", self.local_namespace);
        let actor_local_ids = self
            .nodes
            .keys()
            .filter(|id| id.starts_with(&actor_prefix) && !root.nodes.contains_key(*id))
            .cloned()
            .collect::<Vec<_>>();

        let mut merged = root.nodes.clone();
        for (id, actor_node) in &self.nodes {
            if !root.nodes.contains_key(id) || actor_node.local_overrides.is_empty() {
                continue;
            }
            let Some(node) = merged.get_mut(id) else {
                continue;
            };
            // If another VM has committed a newer local write to this node,
            // keep the root version. Equal revisions are the same already-
            // merged state; greater Actor revisions are applied by its journal.
            if actor_node.local_revision < node.local_revision {
                continue;
            }
            for (key, value) in &actor_node.local_overrides {
                node.local_overrides.insert(key.clone(), value.clone());
            }
            if actor_node.local_overrides.contains_key("Name") {
                node.name = actor_node.name.clone();
            }
            if actor_node.local_overrides.contains_key("Parent") {
                node.parent = actor_node.parent.clone();
            }
            node.local_revision = actor_node.local_revision;
        }
        for id in &actor_local_ids {
            if let Some(node) = self.nodes.get(id) {
                merged.insert(id.clone(), node.clone());
            }
        }
        let present = merged
            .keys()
            .cloned()
            .collect::<std::collections::HashSet<_>>();
        for node in merged.values_mut() {
            if node
                .parent
                .as_ref()
                .is_some_and(|parent| !present.contains(parent))
            {
                node.parent = None;
            }
        }

        self.nodes = merged;
        let mut order = root.node_order.clone();
        let mut ordered = order
            .iter()
            .cloned()
            .collect::<std::collections::HashSet<_>>();
        for id in actor_local_ids {
            if ordered.insert(id.clone()) {
                order.push(id);
            }
        }
        self.node_order = order;
        self.services = root.services.clone();
        self.snapshot = root.snapshot.clone();
        self.sequence = root.sequence;
        self.local_revision = self.local_revision.max(root.local_revision);
        self.next_local_id = self.next_local_id.max(root.next_local_id);
    }

    pub fn from_snapshot(
        snapshot: &Json,
        shared: Option<&SharedProjectedWorldView>,
    ) -> Result<World, String> {
        let root = snapshot
            .as_object()
            .ok_or("world snapshot must be an object")?;
        if root.get("version").and_then(Json::as_u64) != Some(1) {
            return Err("unsupported or missing world snapshot version".into());
        }
        let sequence = root.get("sequence").and_then(Json::as_u64).unwrap_or(0);
        let records = root.get("nodes").and_then(Json::as_array);
        if records.is_none() && shared.is_none() {
            return Err("world snapshot nodes must be an array without a projected view".into());
        }
        let node_count = records.filter(|nodes| !nodes.is_empty()).map_or_else(
            || shared.map_or(0, SharedProjectedWorldView::node_count),
            Vec::len,
        );
        if node_count > 100_000 {
            return Err("world snapshot exceeds node limit".into());
        }
        let mut world = World {
            nodes: HashMap::with_capacity(node_count),
            services: HashMap::new(),
            snapshot: Json::Null,
            sequence,
            lightweight_assets: root
                .get("lightweight_assets")
                .and_then(Json::as_bool)
                .unwrap_or(false),
            effects: JsonMap::new(),
            focused_textbox: None,
            focused_text: String::new(),
            focused_text_revision: 0,
            focused_editor_revision: 0,
            focused_cursor: 0,
            focused_anchor: 0,
            shared: shared.cloned(),
            methods: HashMap::new(),
            getters: HashMap::new(),
            keyed_getters: HashMap::new(),
            setters: HashMap::new(),
            keyed_setters: HashMap::new(),
            local_namespace: "local".into(),
            next_local_id: 1,
            node_order: Vec::with_capacity(node_count),
            local_revision: 0,
        };
        for record in records.into_iter().flatten() {
            let obj = record.as_object().ok_or("world node must be an object")?;
            let id = required_string(obj.get("id"), "node id")?;
            if world.nodes.contains_key(&id) {
                return Err(format!("duplicate world node id {id:?}"));
            }
            let projected = shared.and_then(|v| v.lookup_node(&id));
            let class_name = obj
                .get("class_name")
                .and_then(Json::as_str)
                .or_else(|| projected.as_ref().map(ProjectedNode::class_name))
                .ok_or("world node class_name must be a string")?
                .to_owned();
            let name = match obj.get("name").and_then(Json::as_str) {
                Some(name) => name.to_owned(),
                None => projected
                    .as_ref()
                    .and_then(|p| p.name().ok())
                    .unwrap_or_else(|| class_name.clone()),
            };
            let parent = match obj.get("parent") {
                None | Some(Json::Null) => None,
                Some(Json::String(s)) => Some(s.clone()),
                Some(Json::Object(m)) => {
                    m.get("$ref").and_then(Json::as_str).map(ToOwned::to_owned)
                }
                _ => return Err("world node parent must be a string or null".into()),
            };
            let destroyed = obj
                .get("destroyed")
                .and_then(Json::as_bool)
                .unwrap_or(false);
            let server_owned = obj
                .get("server_owned")
                .and_then(Json::as_bool)
                .unwrap_or(true);
            let attributes_known = obj
                .get("attributes_known")
                .and_then(Json::as_bool)
                .unwrap_or(obj.contains_key("attributes"));
            let attributes = obj
                .get("attributes")
                .and_then(Json::as_object)
                .cloned()
                .unwrap_or_default();
            let revisions = obj
                .get("property_revisions")
                .and_then(Json::as_object)
                .cloned()
                .unwrap_or_default();
            let properties = if projected.is_some() {
                JsonMap::new()
            } else {
                obj.get("properties")
                    .and_then(Json::as_object)
                    .cloned()
                    .unwrap_or_default()
            };
            let collection_tags = obj
                .get("collection_tags")
                .and_then(Json::as_array)
                .map(|v| {
                    v.iter()
                        .filter_map(Json::as_str)
                        .map(ToOwned::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            let collection_tags_known = obj
                .get("collection_tags_known")
                .and_then(Json::as_bool)
                .unwrap_or(obj.contains_key("collection_tags"));
            let node = Node {
                id: id.clone(),
                class_name: class_name.clone(),
                name,
                parent,
                destroyed,
                server_owned,
                properties,
                local_overrides: JsonMap::new(),
                attributes,
                attributes_known,
                revisions,
                local_revision: 0,
                collection_tags,
                collection_tags_known,
                projected,
            };
            if let Some(projected) = &node.projected {
                for wire_key in ["Color3uint8", "CollisionGroupReplicate"] {
                    if let Some(value) = projected.property(wire_key)? {
                        let _ =
                            super::world_aliases::from_wire(&node.class_name, wire_key, &value)?;
                    }
                }
            }
            world.nodes.insert(id, node);
            world
                .node_order
                .push(required_string(obj.get("id"), "node id")?);
            if class_name.ends_with("Service")
                || matches!(
                    class_name.as_str(),
                    "Workspace" | "Players" | "RunService" | "UserSettings"
                )
            {
                world.services.entry(class_name).or_insert_with(|| {
                    required_string(obj.get("id"), "node id").unwrap_or_default()
                });
            }
        }
        if world.nodes.is_empty() {
            if let Some(shared) = shared {
                shared.for_each_node(|projected| {
                    let id = projected.id().to_owned();
                    let class_name = projected.class_name().to_owned();
                    let name = projected.name().unwrap_or_else(|_| class_name.clone());
                    let parent = match projected.parent() {
                        ReadOnlyField::Value(value) => Some(value),
                        ReadOnlyField::Missing | ReadOnlyField::Null => None,
                    };
                    let record = projected.raw();
                    let destroyed = record
                        .record_field("destroyed")
                        .and_then(|value| value.as_bool())
                        .unwrap_or(false);
                    let server_owned = record
                        .record_field("server_owned")
                        .and_then(|value| value.as_bool())
                        .unwrap_or(true);
                    let attributes = match projected.section("attributes") {
                        Ok(ReadOnlyField::Value(Json::Object(map))) => map,
                        _ => JsonMap::new(),
                    };
                    let attributes_known = record
                        .record_field("attributes_known")
                        .and_then(|value| value.as_bool())
                        .unwrap_or(false);
                    let collection_tags = match projected.section("collection_tags") {
                        Ok(ReadOnlyField::Value(Json::Array(values))) => values
                            .iter()
                            .filter_map(Json::as_str)
                            .map(ToOwned::to_owned)
                            .collect(),
                        _ => Vec::new(),
                    };
                    let collection_tags_known = record
                        .record_field("collection_tags_known")
                        .and_then(|value| value.as_bool())
                        .unwrap_or(false);
                    let revisions = match projected.section("property_revisions") {
                        Ok(ReadOnlyField::Value(Json::Object(map))) => map,
                        _ => JsonMap::new(),
                    };
                    world.node_order.push(id.clone());
                    if class_name.ends_with("Service")
                        || matches!(
                            class_name.as_str(),
                            "Workspace" | "Players" | "RunService" | "UserSettings"
                        )
                    {
                        world
                            .services
                            .entry(class_name.clone())
                            .or_insert_with(|| id.clone());
                    }
                    world.nodes.insert(
                        id.clone(),
                        Node {
                            id,
                            class_name,
                            name,
                            parent,
                            destroyed,
                            server_owned,
                            properties: JsonMap::new(),
                            local_overrides: JsonMap::new(),
                            attributes,
                            attributes_known,
                            revisions,
                            local_revision: 0,
                            collection_tags,
                            collection_tags_known,
                            projected: Some(projected),
                        },
                    );
                });
            }
        }
        if let Some(services) = root.get("services").and_then(Json::as_object) {
            for (name, value) in services {
                let id = value
                    .as_str()
                    .or_else(|| value.get("$ref").and_then(Json::as_str))
                    .ok_or("service values must be node IDs or $ref records")?;
                world.services.insert(name.clone(), id.to_owned());
            }
        }
        let data_models = world
            .nodes
            .values()
            .filter(|node| node.class_name == "DataModel")
            .count();
        if data_models > 1 {
            return Err("snapshot has multiple DataModel nodes".into());
        }
        if data_models == 0 {
            // A projected snapshot can arrive before its DataModel record.
            // This local root supplies the host's service lookup surface.
            let id = "__world_snapshot_datamodel__".to_owned();
            if world.nodes.contains_key(&id) {
                return Err("reserved local DataModel identity is occupied".into());
            }
            world.node_order.push(id.clone());
            world.nodes.insert(
                id.clone(),
                Node {
                    id,
                    class_name: "DataModel".into(),
                    name: "Game".into(),
                    parent: None,
                    destroyed: false,
                    server_owned: false,
                    properties: JsonMap::new(),
                    local_overrides: JsonMap::new(),
                    attributes: JsonMap::new(),
                    attributes_known: true,
                    revisions: JsonMap::new(),
                    local_revision: 0,
                    collection_tags: Vec::new(),
                    collection_tags_known: true,
                    projected: None,
                },
            );
        }
        let game_id = world
            .nodes
            .values()
            .find(|node| node.class_name == "DataModel")
            .map(|node| node.id.clone())
            .ok_or("snapshot DataModel could not be initialized")?;
        for id in world.services.values() {
            if id == &game_id || !world.nodes.contains_key(id) {
                return Err("service references an unknown node".into());
            }
            if let Some(node) = world.nodes.get_mut(id) {
                if node.parent.is_none() && !node.destroyed {
                    node.parent = Some(game_id.clone());
                }
            }
        }
        let mut metadata = snapshot.clone();
        if let Some(object) = metadata.as_object_mut() {
            object.remove("nodes");
        }
        world.snapshot = metadata;
        Ok(world)
    }

    pub fn apply_delta(
        &mut self,
        delta: &Json,
        shared: Option<&SharedProjectedWorldView>,
    ) -> Result<(), String> {
        // Stage every field that can change. A malformed late action must not
        // leave a half-applied receipt or advance the sequence.
        let mut staged = World {
            nodes: self.nodes.clone(),
            services: self.services.clone(),
            snapshot: self.snapshot.clone(),
            sequence: self.sequence,
            lightweight_assets: self.lightweight_assets,
            effects: self.effects.clone(),
            focused_textbox: self.focused_textbox.clone(),
            focused_text: self.focused_text.clone(),
            focused_text_revision: self.focused_text_revision,
            focused_editor_revision: self.focused_editor_revision,
            focused_cursor: self.focused_cursor,
            focused_anchor: self.focused_anchor,
            shared: shared.cloned().or_else(|| self.shared.clone()),
            methods: HashMap::new(),
            getters: HashMap::new(),
            keyed_getters: HashMap::new(),
            setters: HashMap::new(),
            keyed_setters: HashMap::new(),
            local_namespace: self.local_namespace.clone(),
            next_local_id: self.next_local_id,
            node_order: self.node_order.clone(),
            local_revision: self.local_revision,
        };
        staged.apply_delta_in_place(delta, shared)?;
        self.nodes = staged.nodes;
        self.services = staged.services;
        self.snapshot = staged.snapshot;
        self.sequence = staged.sequence;
        self.effects = staged.effects;
        self.node_order = staged.node_order;
        self.local_namespace = staged.local_namespace;
        self.next_local_id = staged.next_local_id;
        self.local_revision = staged.local_revision;
        self.shared = staged.shared;
        Ok(())
    }

    fn apply_delta_in_place(
        &mut self,
        delta: &Json,
        shared: Option<&SharedProjectedWorldView>,
    ) -> Result<(), String> {
        let root = delta.as_object().ok_or("world delta must be an object")?;
        if serde_json::to_vec(delta)
            .map_err(|error| error.to_string())?
            .len()
            > 8 * 1024 * 1024
        {
            return Err("world delta exceeds 8 MiB".into());
        }
        let target = root
            .get("sequence")
            .and_then(Json::as_u64)
            .ok_or("world delta sequence must be an unsigned integer")?;
        if root.get("version").and_then(Json::as_u64) != Some(1)
            || root.get("delta").and_then(Json::as_bool) != Some(true)
        {
            return Err("unsupported world delta envelope".into());
        }
        if target < self.sequence {
            return Err("world delta sequence moved backwards".into());
        }
        let actions = root
            .get("actions")
            .and_then(Json::as_array)
            .ok_or("world delta actions must be an array")?;
        if actions.len() > 10_000 {
            return Err("world delta action limit exceeded".into());
        }
        if let Some(player) = root.get("local_player") {
            if !player.is_null() && !player.is_string() {
                return Err("local_player must be a session-owned reference or null".into());
            }
            if let Some(id) = player.as_str() {
                let created = actions.iter().rev().find_map(|action| {
                    if action.get("kind").and_then(Json::as_str) != Some("create") {
                        return None;
                    }
                    let node = action.get("node")?;
                    (node.get("id").and_then(Json::as_str) == Some(id)).then_some(node)
                });
                let class = created
                    .and_then(|node| node.get("class_name").and_then(Json::as_str))
                    .or_else(|| {
                        self.nodes
                            .get(id)
                            .filter(|node| !node.destroyed)
                            .map(|node| node.class_name.as_str())
                    });
                if class != Some("Player") {
                    return Err("local_player must reference a live Player".into());
                }
            }
        }
        if let Some(value) = root.get("services") {
            let services = value
                .as_object()
                .ok_or("delta services must be an object")?;
            if services.len() > 128 {
                return Err("delta services exceed the service limit".into());
            }
            for (name, value) in services {
                if name.is_empty()
                    || name.len() > 256
                    || (!value.is_null() && !value.is_string())
                    || value.as_str().is_some_and(str::is_empty)
                {
                    return Err("delta service entries must contain instance ids or null".into());
                }
            }
        }
        let mut last_action_sequence = self.sequence;
        for action in actions {
            let a = action
                .as_object()
                .ok_or("world delta action must be an object")?;
            let kind = a
                .get("kind")
                .and_then(Json::as_str)
                .ok_or("world delta action kind is missing")?;
            let action_sequence = a
                .get("sequence")
                .and_then(Json::as_u64)
                .ok_or("world delta action sequence is missing")?;
            if action_sequence <= self.sequence {
                continue;
            }
            if action_sequence <= last_action_sequence || action_sequence > target {
                return Err(
                    "world delta action sequence is unordered or beyond its envelope".into(),
                );
            }
            last_action_sequence = action_sequence;
            match kind {
                "create" => {
                    let record = a
                        .get("node")
                        .and_then(Json::as_object)
                        .ok_or("create action node is missing")?;
                    let id = required_string(record.get("id"), "node id")?;
                    let class_name = required_string(record.get("class_name"), "node class_name")?;
                    let name = record
                        .get("name")
                        .and_then(Json::as_str)
                        .unwrap_or(&class_name)
                        .to_owned();
                    let parent = record
                        .get("parent")
                        .and_then(Json::as_str)
                        .map(ToOwned::to_owned);
                    let projected = shared.and_then(|v| v.lookup_node(&id));
                    let properties = if projected.is_some() {
                        JsonMap::new()
                    } else {
                        record
                            .get("properties")
                            .and_then(Json::as_object)
                            .cloned()
                            .unwrap_or_default()
                    };
                    let revisions = record
                        .get("property_revisions")
                        .and_then(Json::as_object)
                        .cloned()
                        .unwrap_or_default();
                    let collection_tags = record
                        .get("collection_tags")
                        .and_then(Json::as_array)
                        .map(|v| {
                            v.iter()
                                .filter_map(Json::as_str)
                                .map(ToOwned::to_owned)
                                .collect()
                        })
                        .unwrap_or_default();
                    let collection_tags_known = record
                        .get("collection_tags_known")
                        .and_then(Json::as_bool)
                        .unwrap_or(false);
                    let node = Node {
                        id: id.clone(),
                        class_name,
                        name,
                        parent,
                        destroyed: false,
                        server_owned: true,
                        properties,
                        local_overrides: JsonMap::new(),
                        attributes: record
                            .get("attributes")
                            .and_then(Json::as_object)
                            .cloned()
                            .unwrap_or_default(),
                        attributes_known: record
                            .get("attributes_known")
                            .and_then(Json::as_bool)
                            .unwrap_or(false),
                        revisions,
                        local_revision: 0,
                        collection_tags,
                        collection_tags_known,
                        projected,
                    };
                    let is_new = !self.nodes.contains_key(&id);
                    self.nodes.insert(id, node);
                    if is_new {
                        self.node_order
                            .push(required_string(record.get("id"), "node id")?);
                    }
                }
                "set" | "set_property" => {
                    let id = required_string(a.get("id"), "set id")?;
                    let mut fields = JsonMap::new();
                    if let Some(set_fields) = a.get("fields").and_then(Json::as_object) {
                        fields.extend(set_fields.iter().map(|(k, v)| (k.clone(), v.clone())));
                    }
                    if let Some(key) = a
                        .get("property")
                        .or_else(|| a.get("key"))
                        .and_then(Json::as_str)
                    {
                        fields.insert(
                            key.into(),
                            a.get("value").cloned().ok_or("set value is missing")?,
                        );
                    }
                    let node = self
                        .nodes
                        .get_mut(&id)
                        .ok_or("set references unknown node")?;
                    for (key, value) in fields {
                        let alias = super::world_aliases::canonical_key(&node.class_name, &key);
                        if alias.is_some() {
                            let _ =
                                super::world_aliases::from_wire(&node.class_name, &key, &value)?;
                        }
                        if key == "Parent" {
                            node.parent = ref_id(&value);
                        } else if key == "Name" {
                            if let Some(s) = value.as_str() {
                                node.name = s.into();
                            }
                        }
                        node.local_overrides.remove(&key);
                        if let Some(canonical) = alias {
                            node.local_overrides.remove(canonical);
                        }
                        if shared.and_then(|v| v.lookup_node(&id)).is_none() {
                            node.properties.insert(key.clone(), value);
                            node.revisions.insert(key, json!(action_sequence));
                        } else if let Some(projected) = shared.and_then(|v| v.lookup_node(&id)) {
                            match projected.revision(&key).ok().flatten() {
                                Some(revision) => {
                                    node.revisions.insert(key, revision);
                                }
                                None => {
                                    node.revisions.remove(&key);
                                }
                            }
                        }
                    }
                    if let Some(projected) = shared.and_then(|v| v.lookup_node(&id)) {
                        node.projected = Some(projected);
                    }
                    if let Some(snapshot) = a.get("attribute_snapshot") {
                        let attrs = snapshot
                            .get("attributes")
                            .and_then(Json::as_object)
                            .ok_or("attribute snapshot values must be an object")?;
                        if attrs.len() > 1024 {
                            return Err("attribute count limit exceeded".into());
                        }
                        node.attributes = attrs.clone();
                        node.attributes_known = snapshot
                            .get("attributes_known")
                            .and_then(Json::as_bool)
                            .ok_or("attribute snapshot known flag is missing")?;
                    }
                    if let Some(snapshot) = a.get("tag_snapshot") {
                        let tags = snapshot
                            .get("collection_tags")
                            .and_then(Json::as_array)
                            .ok_or("collection tag snapshot values must be an array")?;
                        node.collection_tags_known = snapshot
                            .get("collection_tags_known")
                            .and_then(Json::as_bool)
                            .unwrap_or(true);
                        node.collection_tags = validate_tags(tags, node.collection_tags_known)?;
                    }
                }
                "attribute" => {
                    let id = required_string(a.get("id"), "attribute id")?;
                    let update = a
                        .get("attribute_update")
                        .and_then(Json::as_object)
                        .ok_or("attribute update is missing")?;
                    let key = required_string(update.get("name"), "attribute name")?;
                    let node = self
                        .nodes
                        .get_mut(&id)
                        .ok_or("attribute update references unknown node")?;
                    let known = update.get("known").and_then(Json::as_bool).unwrap_or(false);
                    if known {
                        let value = update
                            .get("value")
                            .cloned()
                            .ok_or("known attribute update value is missing")?;
                        if !node.attributes.contains_key(&key)
                            && (!value.is_null() || !node.attributes_known)
                            && node.attributes.len() >= 1024
                        {
                            return Err("attribute count limit exceeded".into());
                        }
                        node.attributes.insert(key, value);
                    } else {
                        node.attributes.remove(&key);
                        node.attributes_known = false;
                    }
                    // A known single key does not establish that the entire
                    // attribute map was replicated. Only a full snapshot does.
                }
                "set_tags" => {
                    let id = required_string(a.get("id"), "tag update id")?;
                    let tags = a
                        .get("tags")
                        .and_then(Json::as_array)
                        .ok_or("tag update list is missing")?;
                    validate_tags(tags, true)?;
                    let node = self
                        .nodes
                        .get_mut(&id)
                        .ok_or("tag update references unknown node")?;
                    node.collection_tags = validate_tags(tags, true)?;
                    node.collection_tags_known = true;
                }
                "remove" | "destroy" => {
                    let roots = if let Some(id) = a.get("id").and_then(Json::as_str) {
                        vec![id.to_owned()]
                    } else if let Some(ids) = a.get("ids").and_then(Json::as_array) {
                        ids.iter()
                            .filter_map(Json::as_str)
                            .map(ToOwned::to_owned)
                            .collect()
                    } else {
                        return Err("destroy action ids are missing".into());
                    };
                    let mut pending = roots;
                    let mut selected = Vec::new();
                    let mut seen = std::collections::HashSet::new();
                    while let Some(id) = pending.pop() {
                        if !seen.insert(id.clone()) {
                            continue;
                        }
                        if !self.nodes.contains_key(&id) {
                            continue;
                        }
                        selected.push(id.clone());
                        pending.extend(self.node_order.iter().filter_map(|child| {
                            self.nodes
                                .get(child)
                                .filter(|node| {
                                    !node.destroyed && node.parent.as_deref() == Some(id.as_str())
                                })
                                .map(|node| node.id.clone())
                        }));
                        if selected.len() > 100_000 {
                            return Err("destroy subtree limit exceeded".into());
                        }
                    }
                    for id in selected {
                        if let Some(node) = self.nodes.get_mut(&id) {
                            node.destroyed = true;
                            node.parent = None;
                            node.local_overrides.insert("Parent".into(), Json::Null);
                        }
                    }
                }
                "event" => {
                    required_string(a.get("id"), "event id")?;
                    required_string(a.get("name"), "event name")?;
                    if !a.get("arguments").is_some_and(Json::is_array) {
                        return Err("event arguments must be an array".into());
                    }
                }
                other => return Err(format!("unsupported world delta action {other:?}")),
            }
        }
        self.sequence = target;
        self.snapshot["sequence"] = json!(target);
        for (key, value) in root {
            if matches!(key.as_str(), "actions" | "sequence" | "version" | "delta") {
                continue;
            }
            if key == "services" {
                if let Some(services) = value.as_object() {
                    for (name, value) in services {
                        if value.is_null() {
                            self.services.remove(name);
                            continue;
                        }
                        let id = value
                            .as_str()
                            .ok_or("delta service reference must be a node ID or null")?;
                        let game_id = self
                            .nodes
                            .values()
                            .find(|node| node.class_name == "DataModel")
                            .map(|node| node.id.clone())
                            .ok_or("delta DataModel is unavailable")?;
                        let service = self
                            .nodes
                            .get_mut(id)
                            .ok_or("delta service reference is invalid")?;
                        if service.destroyed
                            || service.class_name != name.as_str()
                            || id == game_id.as_str()
                        {
                            return Err("delta service reference is invalid".into());
                        }
                        if service.parent.is_none() {
                            service.parent = Some(game_id);
                        }
                        self.services.insert(name.clone(), id.to_owned());
                    }
                }
            }
            if let Some(snapshot) = self.snapshot.as_object_mut() {
                snapshot.insert(key.clone(), value.clone());
            }
        }
        let nodes = &self.nodes;
        self.services.retain(|name, id| {
            nodes
                .get(id)
                .is_some_and(|node| !node.destroyed && node.class_name == name.as_str())
        });
        Ok(())
    }
}

fn required_string(value: Option<&Json>, what: &str) -> Result<String, String> {
    value
        .and_then(Json::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| format!("{what} must be a string"))
}

fn local_instance_id(namespace: &str, counter: u64) -> String {
    format!("{namespace}:{counter}")
}

fn allocate_local_instance_id(world: &mut World) -> mlua::Result<String> {
    loop {
        let id = local_instance_id(&world.local_namespace, world.next_local_id);
        world.next_local_id = world
            .next_local_id
            .checked_add(1)
            .ok_or_else(|| mlua::Error::RuntimeError("local instance ID exhausted".into()))?;
        if !world.nodes.contains_key(&id) {
            return Ok(id);
        }
    }
}
fn ref_id(value: &Json) -> Option<String> {
    value.as_str().map(ToOwned::to_owned).or_else(|| {
        value
            .get("$ref")
            .and_then(Json::as_str)
            .map(ToOwned::to_owned)
    })
}

fn validate_tags(values: &[Json], known: bool) -> Result<Vec<String>, String> {
    if values.len() > 256 {
        return Err("instance tag limit exceeded".into());
    }
    let mut tags = values
        .iter()
        .map(|value| {
            let tag = value
                .as_str()
                .ok_or_else(|| "collection tag must be a string".to_owned())?;
            if tag.is_empty() || tag.len() > 65_536 || tag.contains('\0') {
                return Err("invalid collection tag".to_owned());
            }
            Ok(tag.to_owned())
        })
        .collect::<Result<Vec<_>, String>>()?;
    if !known && !tags.is_empty() {
        return Err("unknown tag state cannot contain values".into());
    }
    tags.sort();
    if tags.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err("duplicate collection tag".into());
    }
    Ok(tags)
}

fn node_property(node: &Node, key: &str) -> Option<Json> {
    if let Some(value) = node.local_overrides.get(key) {
        return Some(value.clone());
    }
    match key {
        "Name" => return Some(json!(node.name)),
        "ClassName" => return Some(json!(node.class_name)),
        "Parent" => {
            return Some(
                node.parent
                    .as_ref()
                    .map(|id| json!({"$ref":id}))
                    .unwrap_or(Json::Null),
            );
        }
        _ => {}
    }
    if let Some(projected) = &node.projected {
        return projected.property(key).ok().flatten();
    }
    node.properties.get(key).cloned()
}

fn attribute_value_eq(left: &Json, right: &Json) -> bool {
    match (left, right) {
        (Json::Number(a), Json::Number(b)) => a.as_f64() == b.as_f64(),
        (Json::Array(a), Json::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(a, b)| attribute_value_eq(a, b))
        }
        (Json::Object(a), Json::Object(b)) => {
            a.len() == b.len()
                && a.iter().all(|(key, value)| {
                    b.get(key)
                        .is_some_and(|other| attribute_value_eq(value, other))
                })
        }
        _ => left == right,
    }
}

/// Apply a receipt and then publish its local event effects only after the
/// world borrow has been released. The state update itself is transactional.
pub fn apply_delta_with_lua(
    lua: &Lua,
    ctx: &WorldRef,
    delta: &Json,
    shared: Option<&SharedProjectedWorldView>,
) -> mlua::Result<()> {
    let actions = delta
        .get("actions")
        .and_then(Json::as_array)
        .ok_or_else(|| mlua::Error::RuntimeError("world delta actions must be an array".into()))?;
    let starting_sequence = ctx.borrow().sequence;
    let (previous_player, previously_loaded) = {
        let world = ctx.borrow();
        (
            world
                .snapshot
                .get("local_player")
                .cloned()
                .unwrap_or(Json::Null),
            world.snapshot["initial_replication_complete"]
                .as_bool()
                .unwrap_or(false),
        )
    };
    let mut ids = std::collections::HashSet::new();
    let mut destroy_roots = Vec::new();
    let mut keys: HashMap<String, std::collections::HashSet<String>> = HashMap::new();
    for action in actions {
        if let Some(id) = action.get("id").and_then(Json::as_str) {
            ids.insert(id.to_owned());
        }
        if action.get("kind").and_then(Json::as_str) == Some("create") {
            if let Some(id) = action
                .get("node")
                .and_then(|node| node.get("id"))
                .and_then(Json::as_str)
            {
                ids.insert(id.to_owned());
            }
        }
        if let Some(id_list) = action.get("ids").and_then(Json::as_array) {
            ids.extend(
                id_list
                    .iter()
                    .filter_map(Json::as_str)
                    .map(ToOwned::to_owned),
            );
            if matches!(
                action.get("kind").and_then(Json::as_str),
                Some("remove" | "destroy")
            ) {
                destroy_roots.extend(
                    id_list
                        .iter()
                        .filter_map(Json::as_str)
                        .map(ToOwned::to_owned),
                );
            }
        }
        if matches!(
            action.get("kind").and_then(Json::as_str),
            Some("remove" | "destroy")
        ) {
            if let Some(id) = action.get("id").and_then(Json::as_str) {
                destroy_roots.push(id.to_owned());
            }
        }
        if let Some(fields) = action.get("fields").and_then(Json::as_object) {
            let id = action.get("id").and_then(Json::as_str).unwrap_or_default();
            keys.entry(id.into())
                .or_default()
                .extend(fields.keys().cloned());
        }
    }
    {
        let world = ctx.borrow();
        let mut pending = destroy_roots;
        let mut seen = std::collections::HashSet::new();
        while let Some(id) = pending.pop() {
            if !seen.insert(id.clone()) {
                continue;
            }
            ids.insert(id.clone());
            pending.extend(world.node_order.iter().filter_map(|child| {
                world
                    .nodes
                    .get(child)
                    .filter(|node| !node.destroyed && node.parent.as_deref() == Some(id.as_str()))
                    .map(|node| node.id.clone())
            }));
        }
    }
    let before: HashMap<String, Option<Node>> = {
        let world = ctx.borrow();
        ids.into_iter()
            .map(|id| (id.clone(), world.nodes.get(&id).cloned()))
            .collect()
    };
    let old_properties: HashMap<(String, String), Option<Json>> = {
        let world = ctx.borrow();
        keys.into_iter()
            .flat_map(|(id, keys)| {
                let node = world.nodes.get(&id);
                keys.into_iter().map(move |key| {
                    (
                        (id.clone(), key.clone()),
                        node.and_then(|n| node_property(n, &key)),
                    )
                })
            })
            .collect()
    };
    ctx.borrow_mut()
        .apply_delta(delta, shared)
        .map_err(mlua::Error::RuntimeError)?;
    resolve_instance_handle_waiters(lua, ctx)?;
    refresh_globals(lua, ctx)?;
    let (player, loaded, players_service, game) = {
        let world = ctx.borrow();
        (
            world
                .snapshot
                .get("local_player")
                .cloned()
                .unwrap_or(Json::Null),
            world.snapshot["initial_replication_complete"]
                .as_bool()
                .unwrap_or(false),
            world.services.get("Players").cloned(),
            world
                .node_order
                .iter()
                .find(|id| {
                    world
                        .nodes
                        .get(*id)
                        .is_some_and(|n| !n.destroyed && n.class_name == "DataModel")
                })
                .cloned(),
        )
    };
    if player != previous_player {
        if let Some(id) = players_service {
            emit_property_changed(lua, ctx, &id, "LocalPlayer")?;
        }
    }
    if loaded && !previously_loaded {
        if let Some(id) = game {
            super::signals::fire(lua, ctx, &id, "Loaded", MultiValue::new())?;
        }
    }

    for action in actions {
        let sequence = action.get("sequence").and_then(Json::as_u64).unwrap_or(0);
        if sequence <= starting_sequence {
            continue;
        }
        let kind = action
            .get("kind")
            .and_then(Json::as_str)
            .unwrap_or_default();
        let id = action
            .get("id")
            .and_then(Json::as_str)
            .or_else(|| {
                action
                    .get("node")
                    .and_then(|node| node.get("id"))
                    .and_then(Json::as_str)
            })
            .unwrap_or_default();
        match kind {
            "create" => {
                let parent = ctx.borrow().nodes.get(id).map(|node| node.parent.clone());
                if let Some(parent) = parent {
                    hierarchy_changed(lua, ctx, id, None, parent)?;
                }
            }
            "set" | "set_property" => {
                if let Some(fields) = action.get("fields").and_then(Json::as_object) {
                    for key in fields.keys() {
                        let old = old_properties
                            .get(&(id.into(), key.clone()))
                            .cloned()
                            .flatten();
                        let new = ctx
                            .borrow()
                            .nodes
                            .get(id)
                            .and_then(|n| node_property(n, key));
                        if old != new {
                            emit_property_changed(lua, ctx, id, key)?;
                            if key == "Parent" {
                                let parent =
                                    ctx.borrow().nodes.get(id).and_then(|n| n.parent.clone());
                                hierarchy_changed(
                                    lua,
                                    ctx,
                                    id,
                                    before
                                        .get(id)
                                        .and_then(|n| n.as_ref())
                                        .and_then(|n| n.parent.clone()),
                                    parent,
                                )?;
                            }
                        }
                    }
                } else if let Some(key) = action
                    .get("property")
                    .or_else(|| action.get("key"))
                    .and_then(Json::as_str)
                {
                    let old = before
                        .get(id)
                        .and_then(|n| n.as_ref())
                        .and_then(|n| node_property(n, key));
                    let new = ctx
                        .borrow()
                        .nodes
                        .get(id)
                        .and_then(|n| node_property(n, key));
                    if old != new {
                        emit_property_changed(lua, ctx, id, key)?;
                        if key == "Parent" {
                            hierarchy_changed(
                                lua,
                                ctx,
                                id,
                                old.and_then(|v| ref_id(&v)),
                                new.and_then(|v| ref_id(&v)),
                            )?;
                        }
                    }
                }
                if let Some(snapshot) = action.get("attribute_snapshot") {
                    let old = before
                        .get(id)
                        .and_then(|n| n.as_ref())
                        .map(|n| &n.attributes);
                    let new = snapshot.get("attributes").and_then(Json::as_object);
                    if let (Some(old), Some(new)) = (old, new) {
                        let mut names = old.keys().chain(new.keys()).cloned().collect::<Vec<_>>();
                        names.sort();
                        names.dedup();
                        for name in names {
                            let changed = match (old.get(&name), new.get(&name)) {
                                (Some(a), Some(b)) => !attribute_value_eq(a, b),
                                (None, None) => false,
                                _ => true,
                            };
                            if changed {
                                fire_attribute_changed(lua, ctx, id, &name)?;
                            }
                        }
                    }
                }
                if let Some(snapshot) = action.get("tag_snapshot") {
                    fire_tag_snapshot_changes(
                        lua,
                        ctx,
                        id,
                        before.get(id).and_then(|n| n.as_ref()),
                        snapshot,
                    )?;
                }
            }
            "attribute" => {
                let update = action.get("attribute_update");
                let name = update
                    .and_then(|u| u.get("name"))
                    .and_then(Json::as_str)
                    .unwrap_or_default();
                let old = before
                    .get(id)
                    .and_then(|n| n.as_ref())
                    .and_then(|n| n.attributes.get(name));
                let old_present = before
                    .get(id)
                    .and_then(|n| n.as_ref())
                    .is_some_and(|n| n.attributes.contains_key(name));
                let known = update
                    .and_then(|u| u.get("known"))
                    .and_then(Json::as_bool)
                    .unwrap_or(false);
                let new = update.and_then(|u| u.get("value"));
                let changed = if known {
                    match (old, new) {
                        (Some(a), Some(b)) => !attribute_value_eq(a, b),
                        (None, None) => false,
                        _ => true,
                    }
                } else {
                    old_present
                };
                if changed {
                    fire_attribute_changed(lua, ctx, id, name)?;
                }
            }
            "remove" | "destroy" => {
                let roots = if let Some(ids) = action.get("ids").and_then(Json::as_array) {
                    ids.iter()
                        .filter_map(Json::as_str)
                        .map(ToOwned::to_owned)
                        .collect::<Vec<_>>()
                } else {
                    vec![id.to_owned()]
                };
                let mut pending = roots;
                let mut selected = Vec::new();
                let mut seen = std::collections::HashSet::new();
                while let Some(node_id) = pending.pop() {
                    if !seen.insert(node_id.clone()) {
                        continue;
                    }
                    if let Some(Some(node)) = before.get(&node_id) {
                        pending.extend(before.iter().filter_map(|(child_id, child)| {
                            child
                                .as_ref()
                                .filter(|child| child.parent.as_deref() == Some(node_id.as_str()))
                                .map(|_| child_id.clone())
                        }));
                        selected.push((node_id, node.clone()));
                    }
                }
                for (node_id, node) in selected {
                    super::signals::fire(lua, ctx, &node_id, "Destroying", MultiValue::new())?;
                    hierarchy_changed(lua, ctx, &node_id, node.parent.clone(), None)?;
                    let _ = super::bindables::cleanup_instance(lua, ctx, &node_id);
                    let _ = super::signals::cleanup_instance(lua, ctx, &node_id);
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// Refresh an Actor fork from the root after another VM committed local state,
/// publishing the corresponding instance signals in the receiving VM.
pub fn sync_actor_fork_with_lua(
    lua: &Lua,
    actor_world: &WorldRef,
    root_world: &WorldRef,
    journal: &[Json],
) -> mlua::Result<()> {
    let mut touched = std::collections::HashSet::new();
    let mut destroyed = Vec::new();
    for row in journal {
        match row.get("kind").and_then(Json::as_str) {
            Some("create") => {
                if let Some(id) = row.pointer("/node/id").and_then(Json::as_str) {
                    touched.insert(id.to_owned());
                }
            }
            Some("set" | "attribute" | "set_tags") => {
                if let Some(id) = row.get("id").and_then(Json::as_str) {
                    touched.insert(id.to_owned());
                }
            }
            Some("destroy") => {
                if let Some(ids) = row.get("ids").and_then(Json::as_array) {
                    for id in ids.iter().filter_map(Json::as_str) {
                        touched.insert(id.to_owned());
                        destroyed.push(id.to_owned());
                    }
                }
            }
            _ => {}
        }
    }
    let before = {
        let world = actor_world.borrow();
        touched
            .iter()
            .filter_map(|id| world.nodes.get(id).cloned().map(|node| (id.clone(), node)))
            .collect::<HashMap<_, _>>()
    };
    let old_property_values = journal
        .iter()
        .filter_map(|row| {
            Some((
                (
                    row.get("id")?.as_str()?.to_owned(),
                    row.get("property")?.as_str()?.to_owned(),
                ),
                get_property(
                    actor_world,
                    row.get("id")?.as_str()?,
                    row.get("property")?.as_str()?,
                )
                .ok(),
            ))
        })
        .collect::<HashMap<_, _>>();
    // Destroy callbacks observe a live Instance and run before its handles are
    // closed by the world sync below.
    for id in &destroyed {
        if let Some(node) = before.get(id).filter(|node| !node.destroyed) {
            super::signals::fire(lua, actor_world, id, "Destroying", MultiValue::new())?;
            hierarchy_changed(lua, actor_world, id, node.parent.clone(), None)?;
            let _ = super::bindables::cleanup_instance(lua, actor_world, id);
            let _ = super::signals::cleanup_instance(lua, actor_world, id);
        }
    }
    {
        let root = root_world.borrow();
        actor_world.borrow_mut().sync_actor_fork(&root);
    }
    for row in journal {
        let kind = row.get("kind").and_then(Json::as_str).unwrap_or_default();
        match kind {
            "create" => {
                let Some(id) = row.pointer("/node/id").and_then(Json::as_str) else {
                    continue;
                };
                let parent = actor_world
                    .borrow()
                    .nodes
                    .get(id)
                    .and_then(|node| node.parent.clone());
                hierarchy_changed(lua, actor_world, id, None, parent)?;
            }
            "set" => {
                let (Some(id), Some(key)) = (
                    row.get("id").and_then(Json::as_str),
                    row.get("property").and_then(Json::as_str),
                ) else {
                    continue;
                };
                let old_value = old_property_values
                    .get(&(id.to_owned(), key.to_owned()))
                    .cloned()
                    .flatten()
                    .or_else(|| before.get(id).and_then(|node| node_property(node, key)));
                let new_value = get_property(actor_world, id, key).ok();
                if old_value != new_value {
                    emit_property_changed(lua, actor_world, id, key)?;
                    if key == "Parent" {
                        hierarchy_changed(
                            lua,
                            actor_world,
                            id,
                            old_value.as_ref().and_then(ref_id),
                            new_value.as_ref().and_then(ref_id),
                        )?;
                    }
                }
            }
            "attribute" => {
                let (Some(id), Some(name)) = (
                    row.get("id").and_then(Json::as_str),
                    row.get("name").and_then(Json::as_str),
                ) else {
                    continue;
                };
                let old = before
                    .get(id)
                    .and_then(|node| node.attributes.get(name))
                    .cloned();
                let new = actor_world
                    .borrow()
                    .nodes
                    .get(id)
                    .and_then(|node| node.attributes.get(name))
                    .cloned();
                if match (&old, &new) {
                    (Some(a), Some(b)) => !attribute_value_eq(a, b),
                    (None, None) => false,
                    _ => true,
                } {
                    fire_attribute_changed(lua, actor_world, id, name)?;
                }
            }
            "set_tags" => {
                let Some(id) = row.get("id").and_then(Json::as_str) else {
                    continue;
                };
                let new = actor_world
                    .borrow()
                    .nodes
                    .get(id)
                    .map(|node| json!({"collection_tags":node.collection_tags}));
                if let Some(new) = new {
                    fire_tag_snapshot_changes(lua, actor_world, id, before.get(id), &new)?;
                }
            }
            "destroy" => {}
            _ => {}
        }
    }
    Ok(())
}

fn fire_attribute_changed(lua: &Lua, ctx: &WorldRef, id: &str, name: &str) -> mlua::Result<()> {
    let mut args = MultiValue::new();
    args.push_back(Value::String(lua.create_string(name)?));
    super::signals::fire(lua, ctx, id, "AttributeChanged", args)?;
    super::signals::fire(
        lua,
        ctx,
        id,
        &format!("attribute:{name}"),
        MultiValue::new(),
    )
}

fn fire_tag_snapshot_changes(
    lua: &Lua,
    ctx: &WorldRef,
    id: &str,
    before: Option<&Node>,
    snapshot: &Json,
) -> mlua::Result<()> {
    let Some(service) = ctx.borrow().services.get("CollectionService").cloned() else {
        return Ok(());
    };
    let old = before
        .map(|node| node.collection_tags.as_slice())
        .unwrap_or(&[]);
    let new = snapshot
        .get("collection_tags")
        .and_then(Json::as_array)
        .map(|tags| tags.iter().filter_map(Json::as_str).collect::<Vec<_>>())
        .unwrap_or_default();
    for tag in old {
        if !new.contains(&tag.as_str()) {
            super::signals::fire(lua, ctx, &service, &format!("tag-removed:{tag}"), {
                let mut args = MultiValue::new();
                args.push_back(instance(lua, ctx, id)?);
                args
            })?;
        }
    }
    for tag in new {
        if !old.iter().any(|old_tag| *old_tag == tag) {
            super::signals::fire(lua, ctx, &service, &format!("tag-added:{tag}"), {
                let mut args = MultiValue::new();
                args.push_back(instance(lua, ctx, id)?);
                args
            })?;
        }
    }
    Ok(())
}

pub fn register_method(ctx: &WorldRef, class: &'static str, name: &'static str, method: Method) {
    register_method_closure(ctx, class, name, method);
}
pub fn register_method_closure<F>(
    ctx: &WorldRef,
    class: &'static str,
    name: &'static str,
    method: F,
) where
    F: Fn(&Lua, &WorldRef, &str, MultiValue) -> mlua::Result<MultiValue> + 'static,
{
    ctx.borrow_mut()
        .methods
        .insert((class.into(), name.into()), Rc::new(method));
}
fn class_chain(class: &str) -> Vec<String> {
    let parents = class_parents();
    let mut result = Vec::new();
    let mut pending = vec![class.to_owned()];
    while let Some(current) = pending.pop() {
        if result.contains(&current) {
            continue;
        }
        result.push(current.clone());
        if result.len() >= 256 {
            break;
        }
        if let Some(ancestors) = parents.get(&current) {
            pending.extend(ancestors.iter().rev().cloned());
        }
    }
    if !result.iter().any(|name| name == "Instance") {
        result.push("Instance".into());
    }
    result
}
fn canonical_member(class: &str, key: &str) -> String {
    match key {
        "className" => "ClassName",
        "archivable" => "Archivable",
        "children" | "getChildren" => "GetChildren",
        "clone" => "Clone",
        "destroy" => "Destroy",
        "remove" => "Remove",
        "findFirstChild" => "FindFirstChild",
        "waitForChild" => "WaitForChild",
        "isA" => "IsA",
        "isDescendantOf" => "IsDescendantOf",
        "childAdded" => "ChildAdded",
        "getFullName" => "GetFullName",
        "character" if class == "Player" => "Character",
        "TextWrap" if matches!(class, "TextLabel" | "TextButton" | "TextBox") => "TextWrapped",
        _ => key,
    }
    .into()
}
pub fn register_getter(ctx: &WorldRef, key: &'static str, getter: Getter) {
    ctx.borrow_mut()
        .getters
        .entry(key.into())
        .or_default()
        .push(getter);
}
pub fn register_keyed_getter(ctx: &WorldRef, key: &'static str, getter: KeyedGetter) {
    ctx.borrow_mut()
        .keyed_getters
        .entry(key.into())
        .or_default()
        .push(getter);
}
pub fn register_setter(ctx: &WorldRef, key: &'static str, setter: Setter) {
    ctx.borrow_mut()
        .setters
        .entry(key.into())
        .or_default()
        .push(setter);
}
pub fn register_keyed_setter(ctx: &WorldRef, key: &'static str, setter: KeyedSetter) {
    ctx.borrow_mut()
        .keyed_setters
        .entry(key.into())
        .or_default()
        .push(setter);
}

struct InstanceCacheTables(RefCell<HashMap<usize, (Weak<RefCell<World>>, mlua::Table)>>);

pub(crate) fn instance_cache_table(lua: &Lua, ctx: &WorldRef) -> mlua::Result<mlua::Table> {
    let world_key = Rc::as_ptr(ctx) as usize;
    if let Some(caches) = lua.app_data_ref::<InstanceCacheTables>() {
        let mut caches = caches.0.borrow_mut();
        caches.retain(|_, (world, _)| world.strong_count() > 0);
        if let Some((world, cache)) = caches.get(&world_key) {
            if Weak::ptr_eq(world, &Rc::downgrade(ctx)) {
                return Ok(cache.clone());
            }
        }
        caches.remove(&world_key);
    }
    let cache = lua.create_table()?;
    let metatable = lua.create_table()?;
    metatable.set("__mode", "v")?;
    cache.set_metatable(Some(metatable))?;
    if lua.app_data_ref::<InstanceCacheTables>().is_none() {
        lua.set_app_data(InstanceCacheTables(RefCell::new(HashMap::new())));
    }
    if let Some(caches) = lua.app_data_ref::<InstanceCacheTables>() {
        let mut caches = caches.0.borrow_mut();
        caches.retain(|_, (world, _)| world.strong_count() > 0);
        caches.insert(world_key, (Rc::downgrade(ctx), cache.clone()));
    }
    Ok(cache)
}

pub fn instance(lua: &Lua, ctx: &WorldRef, id: &str) -> mlua::Result<Value> {
    let cache = instance_cache_table(lua, ctx)?;
    if let Value::UserData(value) = cache.raw_get::<Value>(id)? {
        // The cache is private to this World bucket, and cache.replace only
        // accepts handles from this World. Avoid borrowing the userdata here:
        // property setters can re-enter instance() while that same userdata is
        // mutably borrowed, and evicting it would change canonical identity.
        return Ok(Value::UserData(value));
    }
    if !ctx.borrow().nodes.contains_key(id) {
        return Ok(Value::Nil);
    }
    let userdata = lua.create_userdata(InstanceHandle {
        id: id.into(),
        world: Rc::downgrade(ctx),
    })?;
    cache.raw_set(id, userdata.clone())?;
    Ok(Value::UserData(userdata))
}

/// Make a second userdata wrapper for the same underlying Instance handle.
pub fn clone_instance_reference(lua: &Lua, value: &Value) -> mlua::Result<Value> {
    let Value::UserData(value) = value else {
        return Err(mlua::Error::RuntimeError("expected Instance".into()));
    };
    let handle = value
        .borrow::<InstanceHandle>()
        .map_err(|_| mlua::Error::RuntimeError("expected Instance".into()))?;
    Ok(Value::UserData(lua.create_userdata(InstanceHandle {
        id: handle.id.clone(),
        world: handle.world.clone(),
    })?))
}

/// Compare wrapper identity by underlying World and node, matching the native
/// handle comparison even when the world itself has since been released.
pub fn compare_instance_references(a: &Value, b: &Value) -> mlua::Result<bool> {
    let (Value::UserData(a), Value::UserData(b)) = (a, b) else {
        return Err(mlua::Error::RuntimeError("expected two Instances".into()));
    };
    let a = a
        .borrow::<InstanceHandle>()
        .map_err(|_| mlua::Error::RuntimeError("expected two Instances".into()))?;
    let b = b
        .borrow::<InstanceHandle>()
        .map_err(|_| mlua::Error::RuntimeError("expected two Instances".into()))?;
    Ok(a.id == b.id && Weak::ptr_eq(&a.world, &b.world))
}

pub fn cached_instance_ids(lua: &Lua, ctx: &WorldRef) -> mlua::Result<Vec<String>> {
    let cache = instance_cache_table(lua, ctx)?;
    let mut ids = Vec::new();
    for pair in cache.pairs::<String, Value>() {
        let (id, value) = pair?;
        if instance_belongs_to(&value, ctx) {
            ids.push(id);
        }
    }
    Ok(ids)
}

pub fn instance_id(value: &Value) -> Option<String> {
    let Value::UserData(data) = value else {
        return None;
    };
    data.borrow::<InstanceHandle>().ok().map(|h| h.id.clone())
}

/// True only when this userdata is one of the Instance handles owned by `ctx`.
pub fn instance_belongs_to(value: &Value, ctx: &WorldRef) -> bool {
    let Value::UserData(data) = value else {
        return false;
    };
    let Ok(handle) = data.borrow::<InstanceHandle>() else {
        return false;
    };
    handle
        .world
        .upgrade()
        .is_some_and(|world| Rc::ptr_eq(&world, ctx))
}

fn decode_instance_ref(lua: &Lua, value: &Json) -> mlua::Result<Value> {
    let id = value.get("$ref").and_then(Json::as_str).ok_or_else(|| {
        mlua::Error::RuntimeError("instance reference must contain a string $ref".into())
    })?;
    let weak = lua.app_data_ref::<Weak<RefCell<World>>>().ok_or_else(|| {
        mlua::Error::RuntimeError("no world is installed in this Luau state".into())
    })?;
    let ctx = weak
        .upgrade()
        .ok_or_else(|| mlua::Error::RuntimeError("world has been released".into()))?;
    instance(lua, &ctx, id)
}

/// Decode an Instance property payload using the exact handles from this
/// World. Unlike the generic value codec, this also understands the snapshot
/// reference and TextChatMessage status tags.
pub fn value_from_json(lua: &Lua, ctx: &WorldRef, value: &Json) -> mlua::Result<Value> {
    if value.get("$ref").is_some() {
        let id = value
            .get("$ref")
            .and_then(Json::as_str)
            .ok_or_else(|| mlua::Error::RuntimeError("invalid Instance reference".into()))?;
        return instance(lua, ctx, id);
    }
    if value.get("$instance_handle").is_some() {
        return decode_instance_handle(lua, value);
    }
    if let Some(status) = value.get("$chat_status").and_then(Json::as_i64) {
        let item = match status {
            1 => "Unknown",
            2 => "Success",
            3 => "Sending",
            4 => "TextFilterFailed",
            5 => "Floodchecked",
            6 => "InvalidPrivacySettings",
            7 => "InvalidTextChannelPermissions",
            8 => "MessageTooLong",
            9 => "ModerationTimeout",
            _ => {
                return Err(mlua::Error::RuntimeError(
                    "unsupported TextChatMessageStatus".into(),
                ));
            }
        };
        return Ok(Value::UserData(super::enums::enum_item(
            lua,
            "TextChatMessageStatus",
            item,
        )?));
    }
    if let Some(tag) = value.get("$enum") {
        let enum_name = tag
            .get("type")
            .and_then(Json::as_str)
            .ok_or_else(|| mlua::Error::RuntimeError("$enum.type must be a string".into()))?;
        let expected = tag
            .get("value")
            .and_then(Json::as_i64)
            .ok_or_else(|| mlua::Error::RuntimeError("$enum.value must be an integer".into()))?;
        let enum_root: mlua::Table = lua.globals().get("Enum")?;
        let enum_type: AnyUserData = enum_root.get(enum_name)?;
        let item = if let Some(name) = tag.get("name").and_then(Json::as_str) {
            enum_type.get::<Value>(name)?
        } else {
            let from_value: mlua::Function = enum_type.get("FromValue")?;
            from_value.call::<Value>(expected as f64)?
        };
        let Value::UserData(item) = item else {
            return Err(mlua::Error::RuntimeError("unknown $enum value".into()));
        };
        if !super::enums::enum_item_matches(&item, enum_name)?
            || super::enums::enum_item_value(&item)? as i64 != expected
        {
            return Err(mlua::Error::RuntimeError(
                "$enum name/value mismatch".into(),
            ));
        }
        return Ok(Value::UserData(item));
    }
    super::values::from_json(lua, value)
}

fn encode_instance_ref(data: &AnyUserData) -> mlua::Result<Option<Json>> {
    let Ok(handle) = data.borrow::<InstanceHandle>() else {
        return Ok(None);
    };
    if handle.world.upgrade().is_none() {
        return Ok(None);
    }
    Ok(Some(json!({"$ref":handle.id})))
}

struct InstanceRefValue {
    id: String,
    world: Weak<RefCell<World>>,
}
impl UserData for InstanceRefValue {
    fn add_fields<F: UserDataFields<Self>>(fields: &mut F) {
        fields.add_meta_field("__type", "InstanceHandle");
    }

    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(MetaMethod::Index, |lua, _this, key: String| {
            match key.as_str() {
                "Get" => {
                    Ok(Value::Function(lua.create_function(move |lua, receiver: AnyUserData| {
                        let (id, owner) = instance_handle_receiver(&receiver)?;
                        let world = owner.upgrade().ok_or_else(|| mlua::Error::RuntimeError("world has been released".into()))?;
                        if id.is_empty() { Ok(Value::Nil) } else { instance(lua, &world, &id) }
                    })?))
                }
                "Wait" => {
                    let register = lua.create_function(move |lua, (receiver, timeout): (AnyUserData, Option<f64>)| {
                        let (id, owner) = instance_handle_receiver(&receiver)?;
                        begin_instance_handle_wait(lua, &owner, &id, timeout)
                    })?;
                    let wrapper: mlua::Function = lua.load(
                        "return function(register) return function(self, timeout)\n\
                         local ok, done, value = pcall(register, self, timeout)\n\
                         if not ok then error(tostring(done), 2) end\n\
                         if done then return value end\n\
                         if not coroutine.isyieldable() then error('waiting for an unresolved InstanceHandle requires a scheduled yieldable thread', 2) end\n\
                         return coroutine.yield()\n\
                         end end",
                    ).eval()?;
                    Ok(Value::Function(wrapper.call::<mlua::Function>(register)?))
                }
                _ => Err(mlua::Error::RuntimeError("unknown InstanceHandle member".into())),
            }
        });
        methods.add_meta_method(MetaMethod::Eq, |_, this, other: Value| {
            let Value::UserData(other) = other else {
                return Ok(false);
            };
            let Ok(other) = other.borrow::<InstanceRefValue>() else {
                return Ok(false);
            };
            let same_world = match (this.world.upgrade(), other.world.upgrade()) {
                (Some(a), Some(b)) => Rc::ptr_eq(&a, &b),
                _ => false,
            };
            Ok(same_world && this.id == other.id)
        });
        methods.add_meta_method(MetaMethod::ToString, |_, this, ()| {
            if this.id.is_empty() {
                return Ok("nil".to_owned());
            }
            let Some(world) = this.world.upgrade() else {
                return Ok("nil".to_owned());
            };
            let name = world
                .borrow()
                .nodes
                .get(&this.id)
                .map(|node| node.name.clone())
                .unwrap_or_else(|| "nil".into());
            Ok(name)
        });
    }
}

fn instance_handle_receiver(
    receiver: &AnyUserData,
) -> mlua::Result<(String, Weak<RefCell<World>>)> {
    let handle = receiver
        .borrow::<InstanceRefValue>()
        .map_err(|_| mlua::Error::RuntimeError("InstanceHandle receiver required".into()))?;
    Ok((handle.id.clone(), handle.world.clone()))
}

fn begin_instance_handle_wait(
    lua: &Lua,
    weak_world: &Weak<RefCell<World>>,
    id: &str,
    timeout: Option<f64>,
) -> mlua::Result<(bool, Value)> {
    if timeout.is_some_and(|seconds| !seconds.is_finite() || seconds <= 0.0) {
        return Err(mlua::Error::RuntimeError(
            "timeOut must be greater than 0".into(),
        ));
    }
    let world = weak_world
        .upgrade()
        .ok_or_else(|| mlua::Error::RuntimeError("world has been released".into()))?;
    if id.is_empty() {
        return Ok((true, Value::Nil));
    }
    if world.borrow().nodes.contains_key(id) {
        return Ok((true, instance(lua, &world, id)?));
    }
    let waiters = lua
        .app_data_ref::<InstanceHandleWaiters>()
        .ok_or_else(|| {
            mlua::Error::RuntimeError(
                "waiting for an unresolved InstanceHandle requires a scheduled yieldable thread"
                    .into(),
            )
        })?
        .clone();
    if unsafe { mlua::ffi::lua_isyieldable(lua.state()) } == 0 {
        return Err(mlua::Error::RuntimeError(
            "waiting for an unresolved InstanceHandle requires a scheduled yieldable thread".into(),
        ));
    }
    let (token, thread) = {
        let mut state = waiters.borrow_mut();
        if state.pending.len() >= 2048 {
            return Err(mlua::Error::RuntimeError(
                "InstanceHandle waiter limit exceeded".into(),
            ));
        }
        state.next_id = state.next_id.wrapping_add(1).max(1);
        (state.next_id, lua.current_thread())
    };
    let timer = if let Some(seconds) = timeout {
        let callback = lua.create_function(move |lua, ()| {
            timeout_instance_handle_waiter(lua, token)?;
            Ok(())
        })?;
        let runner: mlua::Function = lua.load("return function(seconds, callback) local remaining=seconds; while remaining>86400 do task.wait(86400); remaining=remaining-86400 end; if remaining>0 then task.wait(remaining) end; callback() end").eval()?;
        let args = MultiValue::from_vec(vec![Value::Number(seconds), Value::Function(callback)]);
        Some(super::scheduler::enqueue(lua, runner, args)?)
    } else {
        None
    };
    if let Err(error) = super::scheduler::park(lua, &thread) {
        if let Some(timer) = timer {
            cancel_instance_handle_timer(lua, timer);
        }
        return Err(mlua::Error::RuntimeError(format!(
            "waiting for an unresolved InstanceHandle requires a scheduled yieldable thread: {error}"
        )));
    }
    waiters.borrow_mut().pending.push(InstanceHandleWaiter {
        token,
        id: id.to_owned(),
        world: weak_world.clone(),
        thread,
        started: Instant::now(),
        timeout_seconds: timeout,
        timer,
    });
    Ok((false, Value::Nil))
}

fn cancel_instance_handle_timer(lua: &Lua, thread: Thread) {
    let result = (|| -> mlua::Result<()> {
        let task: mlua::Table = lua.globals().get("task")?;
        let cancel: mlua::Function = task.get("cancel")?;
        cancel.call::<()>(thread)
    })();
    let _ = result;
}

fn timeout_instance_handle_waiter(lua: &Lua, token: u64) -> mlua::Result<()> {
    let waiters = lua
        .app_data_ref::<InstanceHandleWaiters>()
        .ok_or_else(|| {
            mlua::Error::RuntimeError("InstanceHandle wait state is unavailable".into())
        })?
        .clone();
    let pending = {
        let mut state = waiters.borrow_mut();
        let Some(index) = state
            .pending
            .iter()
            .position(|waiter| waiter.token == token)
        else {
            return Ok(());
        };
        let expired = state.pending[index]
            .timeout_seconds
            .is_some_and(|seconds| state.pending[index].started.elapsed().as_secs_f64() >= seconds);
        if !expired {
            return Ok(());
        }
        state.pending.remove(index)
    };
    super::scheduler::resume(lua, pending.thread, MultiValue::from_vec(vec![Value::Nil]))
}

pub(super) fn resolve_instance_handle_waiters(lua: &Lua, ctx: &WorldRef) -> mlua::Result<()> {
    let Some(waiters) = lua
        .app_data_ref::<InstanceHandleWaiters>()
        .map(|state| state.clone())
    else {
        return Ok(());
    };
    let ready = {
        let mut state = waiters.borrow_mut();
        let mut ready = Vec::new();
        let mut i = 0;
        while i < state.pending.len() {
            let waiter = &state.pending[i];
            let same_world = waiter
                .world
                .upgrade()
                .is_some_and(|world| Rc::ptr_eq(&world, ctx));
            if !same_world {
                i += 1;
                continue;
            }
            let found = waiter.id.is_empty() || ctx.borrow().nodes.contains_key(&waiter.id);
            let expired = waiter
                .timeout_seconds
                .is_some_and(|seconds| waiter.started.elapsed().as_secs_f64() >= seconds);
            if found || expired {
                ready.push(state.pending.remove(i));
            } else {
                i += 1;
            }
        }
        ready
    };
    for waiter in ready {
        let value = if !waiter.id.is_empty() && ctx.borrow().nodes.contains_key(&waiter.id) {
            instance(lua, ctx, &waiter.id)?
        } else {
            Value::Nil
        };
        if let Some(timer) = waiter.timer {
            cancel_instance_handle_timer(lua, timer);
        }
        super::scheduler::resume(lua, waiter.thread, MultiValue::from_vec(vec![value]))?;
    }
    Ok(())
}
fn decode_instance_handle(lua: &Lua, value: &Json) -> mlua::Result<Value> {
    let id = match value.get("$instance_handle") {
        Some(Json::Null) => String::new(),
        Some(Json::String(id)) => id.clone(),
        _ => {
            return Err(mlua::Error::RuntimeError(
                "instance handle must contain a string id or null".into(),
            ));
        }
    };
    let weak = lua.app_data_ref::<Weak<RefCell<World>>>().ok_or_else(|| {
        mlua::Error::RuntimeError("no world is installed in this Luau state".into())
    })?;
    if weak.upgrade().is_none() {
        return Err(mlua::Error::RuntimeError("world has been released".into()));
    }
    Ok(Value::UserData(lua.create_userdata(InstanceRefValue {
        id,
        world: weak.clone(),
    })?))
}
fn encode_instance_handle(data: &AnyUserData) -> mlua::Result<Option<Json>> {
    let Ok(handle) = data.borrow::<InstanceRefValue>() else {
        return Ok(None);
    };
    if handle.world.upgrade().is_none() {
        return Ok(None);
    }
    Ok(Some(if handle.id.is_empty() {
        json!({"$instance_handle":null})
    } else {
        json!({"$instance_handle":handle.id})
    }))
}

pub fn get_property(ctx: &WorldRef, id: &str, key: &str) -> mlua::Result<Json> {
    let world = ctx.borrow();
    let node = world
        .nodes
        .get(id)
        .ok_or_else(|| mlua::Error::RuntimeError("instance is not in this world".into()))?;
    if let Some(value) = node.local_overrides.get(key) {
        return Ok(value.clone());
    }
    match key {
        "Name" => return Ok(json!(node.name)),
        "ClassName" => return Ok(json!(node.class_name)),
        "Parent" => {
            return Ok(node
                .parent
                .as_ref()
                .map(|p| json!({"$ref":p}))
                .unwrap_or(Json::Null));
        }
        _ => {}
    }
    if let Some(projected) = &node.projected {
        let received = projected.property(key).map_err(mlua::Error::RuntimeError)?;
        if let Some(value) = received {
            return Ok(value);
        }
        if let Some(wire_key) = super::world_aliases::wire_key(&node.class_name, key) {
            if let Some(value) = projected
                .property(wire_key)
                .map_err(mlua::Error::RuntimeError)?
            {
                return super::world_aliases::from_wire(&node.class_name, wire_key, &value)
                    .map_err(mlua::Error::RuntimeError)?
                    .ok_or_else(|| {
                        mlua::Error::RuntimeError(format!(
                            "property '{key}' unavailable in snapshot for {} ({})",
                            node.class_name, node.class_name
                        ))
                    });
            }
        }
        if world.lightweight_assets && super::world_aliases::is_lightweight_asset(key) {
            return Err(mlua::Error::RuntimeError(format!(
                "property '{key}' unavailable in lightweight asset mode"
            )));
        }
        return Err(mlua::Error::RuntimeError(format!(
            "property '{key}' unavailable in snapshot for {} ({})",
            node.class_name, node.class_name
        )));
    }
    if let Some(value) = node.properties.get(key) {
        return Ok(value.clone());
    }
    if let Some(wire_key) = super::world_aliases::wire_key(&node.class_name, key) {
        if let Some(value) = node.properties.get(wire_key) {
            return super::world_aliases::from_wire(&node.class_name, wire_key, value)
                .map_err(mlua::Error::RuntimeError)?
                .ok_or_else(|| {
                    mlua::Error::RuntimeError(format!(
                        "property '{key}' unavailable in snapshot for {} ({})",
                        node.class_name, node.class_name
                    ))
                });
        }
    }
    if world.lightweight_assets && super::world_aliases::is_lightweight_asset(key) {
        return Err(mlua::Error::RuntimeError(format!(
            "property '{key}' unavailable in lightweight asset mode"
        )));
    }
    if !node.server_owned {
        if let Some(default) = local_part_mass_default(&node.class_name, key) {
            return Ok(default);
        }
    }
    Err(mlua::Error::RuntimeError(format!(
        "property '{key}' unavailable in snapshot for {} ({})",
        node.class_name, node.class_name
    )))
}

pub fn get_property_revision(ctx: &WorldRef, id: &str, key: &str) -> Option<u64> {
    let world = ctx.borrow();
    let node = world.nodes.get(id)?;
    if let Some(revision) = node.revisions.get(key).and_then(Json::as_u64) {
        return Some(revision);
    }
    let wire_key = super::world_aliases::wire_key(&node.class_name, key);
    node.projected
        .as_ref()
        .and_then(|projected| {
            projected
                .revision(key)
                .ok()
                .flatten()
                .and_then(|value| value.as_u64())
                .or_else(|| {
                    wire_key.and_then(|wire| {
                        projected
                            .revision(wire)
                            .ok()
                            .flatten()
                            .and_then(|value| value.as_u64())
                    })
                })
        })
        .or_else(|| wire_key.and_then(|wire| node.revisions.get(wire).and_then(Json::as_u64)))
}

fn property_origin(ctx: &WorldRef, id: &str, key: &str) -> mlua::Result<String> {
    let (class, local, parent, projected, known_parent) = {
        let world = ctx.borrow();
        let node = world
            .nodes
            .get(id)
            .ok_or_else(|| mlua::Error::RuntimeError("instance is not in this world".into()))?;
        (
            node.class_name.clone(),
            node.local_overrides.contains_key(key),
            node.parent.clone(),
            node.projected.clone(),
            world
                .nodes
                .keys()
                .cloned()
                .collect::<std::collections::HashSet<_>>(),
        )
    };
    if local {
        return Ok("local".into());
    }
    if key == "Name" || key == "ClassName" {
        return Ok("received".into());
    }
    if key == "Parent" {
        if parent
            .as_ref()
            .is_some_and(|parent| !known_parent.contains(parent))
        {
            return Ok("unresolved_reference".into());
        }
        return Ok("received".into());
    }
    if let Some(projected) = projected {
        if let Some(origin) = projected.origin(key).map_err(mlua::Error::RuntimeError)? {
            if let Some(origin) = origin.as_str() {
                return Ok(origin.to_owned());
            }
        }
        if let Some(wire_key) = super::world_aliases::wire_key(&class, key) {
            if let Some(origin) = projected
                .origin(wire_key)
                .map_err(mlua::Error::RuntimeError)?
            {
                if let Some(origin) = origin.as_str() {
                    return Ok("derived_received".into());
                }
            }
        }
    }
    let (has_property, is_local) = {
        let world = ctx.borrow();
        let node = &world.nodes[id];
        (node.properties.contains_key(key), !node.server_owned)
    };
    if is_local {
        if constructor_default(&class, key).is_ok() {
            return Ok("class_default".into());
        }
        if has_property {
            return Ok("local".into());
        }
    }
    if has_property {
        return Ok("received".into());
    }
    Ok("unavailable".into())
}

/// Store an engine feedback value without producing a script-authored mutation.
/// A supplied revision tracks the source receipt used by physics stale fences.
pub fn set_engine_property(
    ctx: &WorldRef,
    id: &str,
    key: &str,
    value: Json,
    revision: Option<u64>,
) -> mlua::Result<()> {
    let mut world = ctx.borrow_mut();
    let node = world.nodes.get_mut(id).ok_or_else(|| {
        mlua::Error::RuntimeError("engine update references an unknown instance".into())
    })?;
    if key == "Name" {
        node.name = value
            .as_str()
            .ok_or_else(|| mlua::Error::RuntimeError("Name must be a string".into()))?
            .into();
    }
    if key == "Parent" {
        node.parent = ref_id(&value);
    }
    node.local_overrides.insert(key.into(), value);
    if let Some(revision) = revision {
        node.revisions.insert(key.into(), json!(revision));
    }
    Ok(())
}

pub fn emit_property_changed(lua: &Lua, ctx: &WorldRef, id: &str, key: &str) -> mlua::Result<()> {
    let mut args = MultiValue::new();
    args.push_back(Value::String(lua.create_string(key)?));
    super::signals::fire(lua, ctx, id, "Changed", args)?;
    super::signals::fire(lua, ctx, id, &format!("property:{key}"), MultiValue::new())
}

fn fire_with_instances(
    lua: &Lua,
    ctx: &WorldRef,
    id: &str,
    event: &str,
    args: &[Option<String>],
) -> mlua::Result<()> {
    let mut values = MultiValue::new();
    for id in args {
        values.push_back(match id {
            Some(id) => instance(lua, ctx, id)?,
            None => Value::Nil,
        });
    }
    super::signals::fire(lua, ctx, id, event, values)
}

fn hierarchy_changed(
    lua: &Lua,
    ctx: &WorldRef,
    id: &str,
    old: Option<String>,
    new: Option<String>,
) -> mlua::Result<()> {
    if old == new {
        return Ok(());
    }
    if let Some(parent) = &old {
        fire_with_instances(lua, ctx, parent, "ChildRemoved", &[Some(id.into())])?;
    }
    if let Some(parent) = &new {
        fire_with_instances(lua, ctx, parent, "ChildAdded", &[Some(id.into())])?;
    }
    fire_with_instances(
        lua,
        ctx,
        id,
        "AncestryChanged",
        &[Some(id.into()), new.clone()],
    )?;
    let ancestors = |start: Option<String>| {
        let world = ctx.borrow();
        let mut result = Vec::new();
        let mut current = start;
        let mut seen = std::collections::HashSet::new();
        while let Some(parent) = current {
            if !seen.insert(parent.clone()) {
                break;
            }
            result.push(parent.clone());
            current = world.nodes.get(&parent).and_then(|n| n.parent.clone());
            if result.len() >= 100_000 {
                break;
            }
        }
        result
    };
    for ancestor in ancestors(old) {
        fire_with_instances(
            lua,
            ctx,
            &ancestor,
            "DescendantRemoving",
            &[Some(id.into())],
        )?;
    }
    for ancestor in ancestors(new) {
        fire_with_instances(lua, ctx, &ancestor, "DescendantAdded", &[Some(id.into())])?;
    }
    Ok(())
}

pub fn set_property(
    lua: &Lua,
    ctx: &WorldRef,
    id: &str,
    key: &str,
    value: Json,
) -> mlua::Result<()> {
    let mut world = ctx.borrow_mut();
    let base_local_revision = world
        .nodes
        .get(id)
        .ok_or_else(|| mlua::Error::RuntimeError("instance is not in this world".into()))?
        .local_revision;
    let old_value = world
        .nodes
        .get(id)
        .and_then(|node| node_property(node, key));
    let old_parent = world
        .nodes
        .get(id)
        .ok_or_else(|| mlua::Error::RuntimeError("instance is not in this world".into()))?
        .parent
        .clone();
    let new_parent = if key == "Parent" {
        ref_id(&value)
    } else {
        old_parent.clone()
    };
    if key == "Parent" {
        if let Some(parent) = &new_parent {
            if !world.nodes.contains_key(parent) {
                return Err(mlua::Error::RuntimeError(
                    "Parent belongs to a different world snapshot".into(),
                ));
            }
            let mut cursor = Some(parent.clone());
            let mut seen = std::collections::HashSet::new();
            while let Some(current) = cursor {
                if current == id {
                    return Err(mlua::Error::RuntimeError(
                        "attempt to set Parent would create a cycle".into(),
                    ));
                }
                if !seen.insert(current.clone()) {
                    break;
                }
                cursor = world.nodes.get(&current).and_then(|n| n.parent.clone());
            }
        }
    }
    let node = world.nodes.get_mut(id).unwrap();
    if node.destroyed {
        return Err(mlua::Error::RuntimeError(
            "cannot write a destroyed instance".into(),
        ));
    }
    if key == "ClassName" {
        return Err(mlua::Error::RuntimeError("ClassName is read-only".into()));
    }
    if key == "Name" {
        let name = value
            .as_str()
            .ok_or_else(|| mlua::Error::RuntimeError("Name must be a string".into()))?;
        node.name = name.into();
    } else if key == "Parent" {
        node.parent = ref_id(&value);
    }
    node.local_overrides.insert(key.into(), value.clone());
    world.local_revision = world.local_revision.wrapping_add(1).max(1);
    let revision = world.local_revision;
    if let Some(node) = world.nodes.get_mut(id) {
        node.local_revision = revision;
    }
    let mut mutation = json!({"kind":"set","id":id,"property":key,"value":value});
    if world.local_namespace.starts_with("local:actor:") {
        mutation["_base_local_revision"] = json!(base_local_revision);
    }
    world
        .effects
        .entry("mutations")
        .or_insert_with(|| Json::Array(vec![]))
        .as_array_mut()
        .unwrap()
        .push(mutation);
    drop(world);
    let changed = old_value.as_ref() != Some(&value);
    if changed {
        emit_property_changed(lua, ctx, id, key)?;
    }
    if key == "Parent" && old_parent != new_parent {
        hierarchy_changed(lua, ctx, id, old_parent, new_parent)?;
    }
    Ok(())
}

pub fn is_a(ctx: &WorldRef, id: &str, class: &str) -> bool {
    let world = ctx.borrow();
    let Some(node) = world.nodes.get(id) else {
        return false;
    };
    if node.class_name == class || class == "Instance" || class == "Object" {
        return true;
    }
    class_chain(&node.class_name)
        .iter()
        .any(|ancestor| ancestor == class)
}
pub fn children(ctx: &WorldRef, id: &str) -> Vec<String> {
    let world = ctx.borrow();
    let parent_id = id;
    world
        .node_order
        .iter()
        .filter_map(|node_id| {
            world
                .nodes
                .get(node_id)
                .filter(|n| !n.destroyed && n.parent.as_deref() == Some(parent_id))
                .map(|n| n.id.clone())
        })
        .collect()
}

pub fn descendants(ctx: &WorldRef, id: &str) -> Vec<String> {
    let mut result = Vec::new();
    let mut pending = children(ctx, id);
    let mut cursor = 0;
    while cursor < pending.len() && pending.len() <= 100_000 {
        let child = pending[cursor].clone();
        cursor += 1;
        pending.extend(children(ctx, &child));
        result.push(child);
    }
    result
}

pub fn ordered_ids(ctx: &WorldRef) -> Vec<String> {
    ctx.borrow().node_order.clone()
}

/// Actor-fork mutation journals carry private local revision fences until the
/// host merges them into the root world.
pub fn uses_actor_namespace(ctx: &WorldRef) -> bool {
    ctx.borrow().local_namespace.starts_with("local:actor:")
}

/// Advance a local instance revision and return the revision this write was
/// based on. Only actor-fork mutation journals expose that base privately.
pub fn bump_local_revision(ctx: &WorldRef, id: &str) -> mlua::Result<u64> {
    let mut world = ctx.borrow_mut();
    let base = world
        .nodes
        .get(id)
        .ok_or_else(|| mlua::Error::RuntimeError("instance is not in this world".into()))?
        .local_revision;
    world.local_revision = world.local_revision.wrapping_add(1).max(1);
    let revision = world.local_revision;
    if let Some(node) = world.nodes.get_mut(id) {
        node.local_revision = revision;
    }
    Ok(base)
}

fn class_parents() -> &'static HashMap<String, Vec<String>> {
    static PARENTS: OnceLock<HashMap<String, Vec<String>>> = OnceLock::new();
    PARENTS.get_or_init(|| {
        let bytes = include_str!("../../../runtime/class-parents-745.json");
        let mut parents = serde_json::from_str::<Json>(bytes)
            .ok()
            .and_then(|v| v.get("parents").cloned())
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default()
            .into_iter()
            .filter_map(|(class, value)| {
                let parents = match value {
                    Json::String(parent) => vec![parent],
                    Json::Array(values) => values
                        .into_iter()
                        .filter_map(|v| v.as_str().map(ToOwned::to_owned))
                        .collect(),
                    _ => return None,
                };
                Some((class, parents))
            })
            .collect::<HashMap<_, _>>();
        parents
            .entry("GlobalSettings".into())
            .or_insert_with(|| vec!["GenericSettings".into()]);
        parents
            .entry("GenericSettings".into())
            .or_insert_with(|| vec!["ServiceProvider".into()]);
        parents
            .entry("ServiceProvider".into())
            .or_insert_with(|| vec!["Instance".into()]);
        parents
            .entry("PluginManager".into())
            .or_insert_with(|| vec!["Instance".into()]);
        parents
    })
}
pub fn emit(ctx: &WorldRef, key: &str, value: Json) {
    let mut world = ctx.borrow_mut();
    let values = world
        .effects
        .entry(key)
        .or_insert_with(|| Json::Array(vec![]));
    if let Json::Array(items) = values {
        items.push(value);
    }
}

fn constructors() -> &'static Json {
    static DATA: OnceLock<Json> = OnceLock::new();
    DATA.get_or_init(|| {
        serde_json::from_str(include_str!("../../../runtime/class-constructors-745.json"))
            .unwrap_or(Json::Null)
    })
}

fn local_part_enum_type(class: &str, key: &str) -> Option<&'static str> {
    let ancestry = class_chain(class);
    match key {
        "Material" if ancestry.iter().any(|parent| parent == "BasePart") => Some("Material"),
        "Shape" if ancestry.iter().any(|parent| parent == "Part") => Some("PartType"),
        _ => None,
    }
}

fn local_part_mass_default(class: &str, key: &str) -> Option<Json> {
    let (enum_type, item, value) = match local_part_enum_type(class, key)? {
        "Material" => ("Material", "Plastic", 256),
        "PartType" => ("PartType", "Block", 1),
        _ => return None,
    };
    Some(json!({"$enum":{"type":enum_type,"name":item,"value":value}}))
}

fn constructor_default(class: &str, key: &str) -> mlua::Result<(Json, bool)> {
    let classes = constructors().get("classes");
    if key == "ClassName" {
        return Ok((json!(class), false));
    }
    if key == "Name" {
        let name = classes
            .and_then(|all| all.get(class))
            .and_then(|desc| desc.get("name"))
            .and_then(Json::as_str)
            .unwrap_or(class);
        return Ok((json!(name), true));
    }
    if key == "Parent" {
        let creatable = classes
            .and_then(|all| all.get(class))
            .and_then(|desc| desc.get("creatable"))
            .and_then(Json::as_bool)
            .unwrap_or(false);
        if creatable {
            return Ok((Json::Null, true));
        }
        return Err(mlua::Error::RuntimeError(
            "code-instantiated default is unavailable for this property".into(),
        ));
    }
    if let Some(default) = local_part_mass_default(class, key) {
        return Ok((default, true));
    }
    let lower_key = key.to_ascii_lowercase();
    let lookup_key = if key == "Archivable" {
        "archivable"
    } else {
        key
    };
    for candidate in class_chain(class) {
        let desc = classes.and_then(|all| all.get(&candidate));
        if let Some(property) = desc
            .and_then(|desc| desc.get("properties"))
            .and_then(|props| props.get(lookup_key).or_else(|| props.get(&lower_key)))
        {
            let Some(value) = property.get("value") else {
                break;
            };
            return Ok((
                value.clone(),
                property
                    .get("writable")
                    .and_then(Json::as_bool)
                    .unwrap_or(false),
            ));
        }
    }
    Err(mlua::Error::RuntimeError(
        "code-instantiated default is unavailable for this property".into(),
    ))
}

fn json_property_equal(left: &Json, right: &Json) -> bool {
    match (left.as_f64(), right.as_f64()) {
        (Some(a), Some(b)) => a == b,
        _ => left == right,
    }
}

/// Create one metadata-backed client-local object. This is shared by Instance.new
/// and source playback so default properties and mutation output stay identical.
pub fn create_instance(
    lua: &Lua,
    ctx: &WorldRef,
    class: &str,
    parent: Option<&str>,
) -> mlua::Result<Value> {
    let description = constructors()
        .get("classes")
        .and_then(|c| c.get(class))
        .filter(|v| v.get("creatable").and_then(Json::as_bool) == Some(true))
        .ok_or_else(|| {
            mlua::Error::RuntimeError(format!(
                "class '{class}' is not a supported creatable class"
            ))
        })?;
    let parent = parent.map(ToOwned::to_owned);
    if let Some(id) = &parent {
        if !ctx
            .borrow()
            .nodes
            .get(id)
            .is_some_and(|node| !node.destroyed)
        {
            return Err(mlua::Error::RuntimeError(
                "Parent belongs to a different world snapshot".into(),
            ));
        }
    }
    let (id, name, properties) = {
        let mut world = ctx.borrow_mut();
        if world.nodes.len() >= 101_000 {
            return Err(mlua::Error::RuntimeError(
                "local instance limit exceeded".into(),
            ));
        }
        let id = allocate_local_instance_id(&mut world)?;
        let name = description
            .get("name")
            .and_then(Json::as_str)
            .unwrap_or(class)
            .to_owned();
        let mut properties = JsonMap::new();
        if let Some(defaults) = description.get("properties").and_then(Json::as_object) {
            for (key, record) in defaults {
                if let Some(value) = record.get("value") {
                    properties.insert(key.clone(), value.clone());
                }
            }
        }
        (id, name, properties)
    };
    let node = Node {
        id: id.clone(),
        class_name: class.into(),
        name: name.clone(),
        parent: None,
        destroyed: false,
        server_owned: false,
        properties: properties.clone(),
        local_overrides: JsonMap::new(),
        attributes: JsonMap::new(),
        attributes_known: true,
        revisions: JsonMap::new(),
        local_revision: 0,
        collection_tags: Vec::new(),
        collection_tags_known: true,
        projected: None,
    };
    {
        let mut world = ctx.borrow_mut();
        world.nodes.insert(id.clone(), node);
        world.node_order.push(id.clone());
    }
    emit(
        ctx,
        "mutations",
        json!({"kind":"create","node":{"id":id,"class_name":class,"name":name,"parent":null,
        "properties":properties,"attributes":{},"attributes_known":true,"collection_tags":[],"collection_tags_known":true,
        "destroyed":false,"server_owned":false,"property_revisions":{}}}),
    );
    if let Some(parent) = parent {
        set_property(lua, ctx, &id, "Parent", json!({"$ref":parent}))?;
    }
    instance(lua, ctx, &id)
}

/// Create a small host-managed local service result without admitting new
/// instance classes to the script-facing `Instance.new` constructor.
pub fn create_internal_node(
    lua: &Lua,
    ctx: &WorldRef,
    class: &str,
    parent: Option<&str>,
    properties: serde_json::Map<String, Json>,
) -> mlua::Result<Value> {
    if !matches!(
        class,
        "TextChatMessage"
            | "UserSettings"
            | "UserGameSettings"
            | "GlobalSettings"
            | "PluginManager"
    ) {
        return Err(mlua::Error::RuntimeError(
            "class is not available to internal node creation".into(),
        ));
    }
    if serde_json::to_vec(&properties)
        .map_err(mlua::Error::external)?
        .len()
        > 64 * 1024
    {
        return Err(mlua::Error::RuntimeError(
            "internal node properties exceed the 64 KiB limit".into(),
        ));
    }
    let (id, node) = {
        let mut world = ctx.borrow_mut();
        if let Some(parent) = parent {
            if !world.nodes.get(parent).is_some_and(|node| !node.destroyed) {
                return Err(mlua::Error::RuntimeError(
                    "internal node parent is unavailable".into(),
                ));
            }
        }
        let id = allocate_local_instance_id(&mut world)?;
        let node = Node {
            id: id.clone(),
            class_name: class.to_owned(),
            name: class.to_owned(),
            parent: parent.map(ToOwned::to_owned),
            destroyed: false,
            server_owned: false,
            properties,
            local_overrides: JsonMap::new(),
            attributes: JsonMap::new(),
            attributes_known: true,
            revisions: JsonMap::new(),
            local_revision: 0,
            collection_tags: Vec::new(),
            collection_tags_known: true,
            projected: None,
        };
        world.node_order.push(id.clone());
        world.nodes.insert(id.clone(), node.clone());
        (id, node)
    };
    emit(
        ctx,
        "mutations",
        json!({"kind":"create","node":{
            "id":node.id,"class_name":node.class_name,"name":node.name,
            "parent":node.parent.as_ref().map(|id| json!(id)).unwrap_or(Json::Null),
            "properties":node.properties,"attributes":node.attributes,
            "attributes_known":node.attributes_known,"collection_tags":node.collection_tags,
            "collection_tags_known":node.collection_tags_known,"destroyed":false,"server_owned":false,
            "property_revisions":node.revisions
        }}),
    );
    if let Some(parent) = parent {
        hierarchy_changed(lua, ctx, &id, None, Some(parent.to_owned()))?;
    }
    instance(lua, ctx, &id)
}

/// Recreate the two local engine singletons without adding a server service or
/// producing a script-visible create mutation.
pub fn ensure_engine_global_node(lua: &Lua, ctx: &WorldRef, class: &str) -> mlua::Result<Value> {
    let id = match class {
        "GlobalSettings" => "local:engine:GlobalSettings",
        "PluginManager" => "local:engine:PluginManager",
        _ => {
            return Err(mlua::Error::RuntimeError(
                "class is not an engine global singleton".into(),
            ));
        }
    };
    {
        let mut world = ctx.borrow_mut();
        if world.nodes.get(id).is_some_and(|node| !node.destroyed) {
            // Keep the existing singleton stable.
        } else {
            if world.nodes.len() >= 101_000 {
                return Err(mlua::Error::RuntimeError(
                    "engine global node limit exceeded".into(),
                ));
            }
            if !world.nodes.contains_key(id) {
                world.node_order.push(id.to_owned());
            }
            world.nodes.insert(
                id.to_owned(),
                Node {
                    id: id.to_owned(),
                    class_name: class.to_owned(),
                    name: class.to_owned(),
                    parent: None,
                    destroyed: false,
                    server_owned: false,
                    properties: JsonMap::new(),
                    local_overrides: JsonMap::new(),
                    attributes: JsonMap::new(),
                    attributes_known: true,
                    revisions: JsonMap::new(),
                    local_revision: 0,
                    collection_tags: Vec::new(),
                    collection_tags_known: true,
                    projected: None,
                },
            );
        }
    };
    instance(lua, ctx, id)
}

fn local_service_supported(class: &str) -> bool {
    matches!(
        class,
        "VirtualInputManager"
            | "VRService"
            | "SoundService"
            | "PhysicsService"
            | "EncodingService"
            | "PathfindingService"
            | "CorePackages"
            | "SharedTableRegistry"
            | "TextService"
            | "RunService"
            | "HttpService"
            | "ServerStorage"
            | "ServerScriptService"
            | "GuiService"
            | "UserInputService"
            | "ContextActionService"
            | "Lighting"
            | "CoreGui"
            | "Debris"
            | "CollectionService"
            | "TweenService"
            | "TestService"
            | "Stats"
    )
}

/// Return the stable client-owned local service Instance, creating it only for
/// the pinned list of services that the legacy runtime supplies locally.
pub fn ensure_client_service(lua: &Lua, ctx: &WorldRef, class: &str) -> mlua::Result<Value> {
    if !local_service_supported(class) {
        return Err(mlua::Error::RuntimeError(format!(
            "local service {class} is unsupported"
        )));
    }
    let existing = {
        let mut world = ctx.borrow_mut();
        if let Some((id, _)) = world
            .nodes
            .iter()
            .find(|(_, node)| !node.server_owned && !node.destroyed && node.class_name == class)
        {
            let id = id.clone();
            world.services.insert(class.to_owned(), id.clone());
            Some(id)
        } else {
            None
        }
    };
    if let Some(id) = existing {
        return instance(lua, ctx, &id);
    }
    let (id, game_id) = {
        let mut world = ctx.borrow_mut();
        if world.nodes.len() >= 100_000 {
            return Err(mlua::Error::RuntimeError(
                "world node limit exceeded".into(),
            ));
        }
        let game_id = world
            .nodes
            .values()
            .find(|node| node.class_name == "DataModel" && !node.destroyed)
            .map(|node| node.id.clone())
            .ok_or_else(|| {
                mlua::Error::RuntimeError(
                    "DataModel is unavailable for local service creation".into(),
                )
            })?;
        let id = allocate_local_instance_id(&mut world)?;
        let node = Node {
            id: id.clone(),
            class_name: class.to_owned(),
            name: class.to_owned(),
            parent: Some(game_id.clone()),
            destroyed: false,
            server_owned: false,
            properties: JsonMap::from_iter([("Archivable".into(), json!(true))]),
            local_overrides: JsonMap::new(),
            attributes: JsonMap::new(),
            attributes_known: true,
            revisions: JsonMap::new(),
            local_revision: 0,
            collection_tags: Vec::new(),
            collection_tags_known: true,
            projected: None,
        };
        world.node_order.push(id.clone());
        world.nodes.insert(id.clone(), node);
        world.services.insert(class.to_owned(), id.clone());
        (id, game_id)
    };
    emit(
        ctx,
        "mutations",
        json!({"kind":"create","node":{
            "id":id,"class_name":class,"name":class,"parent":game_id,
            "properties":{"Archivable":true},"attributes":{},"attributes_known":true,
            "collection_tags":[],"collection_tags_known":true,"destroyed":false,
            "server_owned":false,"property_revisions":{}
        }}),
    );
    hierarchy_changed(lua, ctx, &id, None, Some(game_id))?;
    instance(lua, ctx, &id)
}

/// Adopt one locally-created node from an Actor VM into the shared root world.
/// The ID must use this Actor's reserved namespace, and the serialized create
/// row must match the node the Actor VM actually created.
pub fn apply_actor_create(
    lua: &Lua,
    root: &WorldRef,
    actor_world: &WorldRef,
    actor_id: &str,
    record: &Json,
) -> mlua::Result<()> {
    let id = record
        .get("id")
        .and_then(Json::as_str)
        .ok_or_else(|| mlua::Error::RuntimeError("Actor create row needs an ID".into()))?;
    let mut encoded_id = String::with_capacity(actor_id.len() * 2);
    for byte in actor_id.as_bytes() {
        use std::fmt::Write as _;
        let _ = write!(&mut encoded_id, "{byte:02x}");
    }
    let namespace = format!("local:actor:{encoded_id}:");
    if !id.starts_with(&namespace) {
        return Err(mlua::Error::RuntimeError(
            "Actor create ID is outside its local namespace".into(),
        ));
    }
    let serialized_size = serde_json::to_vec(record)
        .map_err(mlua::Error::external)?
        .len();
    if serialized_size > 1024 * 1024 {
        return Err(mlua::Error::RuntimeError(
            "Actor create record exceeds the 1 MiB limit".into(),
        ));
    }
    let (source, parent) = {
        let actor = actor_world.borrow();
        let source = actor.nodes.get(id).cloned().ok_or_else(|| {
            mlua::Error::RuntimeError("Actor create row references no local node".into())
        })?;
        if source.server_owned || source.projected.is_some() || source.destroyed {
            return Err(mlua::Error::RuntimeError(
                "Actor create row cannot adopt a received or destroyed node".into(),
            ));
        }
        let data = record
            .as_object()
            .ok_or_else(|| mlua::Error::RuntimeError("invalid Actor create row".into()))?;
        let class = data
            .get("class_name")
            .and_then(Json::as_str)
            .ok_or_else(|| mlua::Error::RuntimeError("Actor create row needs class_name".into()))?;
        if class != source.class_name {
            return Err(mlua::Error::RuntimeError(
                "Actor create row class does not match its local node".into(),
            ));
        }
        let creatable = constructors()
            .get("classes")
            .and_then(|all| all.get(class))
            .and_then(|description| description.get("creatable"))
            .and_then(Json::as_bool)
            .unwrap_or(false);
        if !creatable && !local_service_supported(class) {
            return Err(mlua::Error::RuntimeError(
                "Actor create class is not a supported local constructor or service".into(),
            ));
        }
        let parent = data.get("parent").and_then(ref_id);
        (source, parent)
    };
    let data = record
        .as_object()
        .ok_or_else(|| mlua::Error::RuntimeError("invalid Actor create row".into()))?;
    let name = data
        .get("name")
        .and_then(Json::as_str)
        .ok_or_else(|| mlua::Error::RuntimeError("Actor create row needs name".into()))?;
    let properties = data
        .get("properties")
        .and_then(Json::as_object)
        .ok_or_else(|| mlua::Error::RuntimeError("Actor create row needs properties".into()))?;
    let attributes = data
        .get("attributes")
        .and_then(Json::as_object)
        .cloned()
        .unwrap_or_default();
    let attributes_known = data
        .get("attributes_known")
        .and_then(Json::as_bool)
        .unwrap_or(true);
    let collection_tags = data
        .get("collection_tags")
        .and_then(Json::as_array)
        .cloned()
        .unwrap_or_default();
    let collection_tags_known = data
        .get("collection_tags_known")
        .and_then(Json::as_bool)
        .unwrap_or(true);
    let collection_tags = validate_tags(&collection_tags, collection_tags_known)
        .map_err(mlua::Error::RuntimeError)?;
    let property_revisions = data
        .get("property_revisions")
        .and_then(Json::as_object)
        .cloned()
        .unwrap_or_default();
    if data.get("server_owned").and_then(Json::as_bool) == Some(true)
        || data.get("destroyed").and_then(Json::as_bool) == Some(true)
    {
        return Err(mlua::Error::RuntimeError(
            "Actor create row cannot adopt a server-owned or destroyed node".into(),
        ));
    }
    {
        let mut world = root.borrow_mut();
        if world.nodes.contains_key(id) {
            return Err(mlua::Error::RuntimeError(
                "Actor create ID already exists in the root world".into(),
            ));
        }
        if let Some(parent) = &parent {
            if !world.nodes.get(parent).is_some_and(|node| !node.destroyed) {
                return Err(mlua::Error::RuntimeError(
                    "Actor create parent is unavailable in the root world".into(),
                ));
            }
        }
        if local_service_supported(&source.class_name) {
            let expected_parent = world
                .nodes
                .values()
                .find(|node| node.class_name == "DataModel" && !node.destroyed)
                .map(|node| node.id.as_str());
            if parent.as_deref() != expected_parent
                || name != source.class_name
                || !attributes.is_empty()
                || !attributes_known
                || !collection_tags.is_empty()
                || !collection_tags_known
                || properties.len() != 1
                || properties.get("Archivable") != Some(&json!(true))
            {
                return Err(mlua::Error::RuntimeError(
                    "Actor local service create record does not match its engine defaults".into(),
                ));
            }
            if world
                .services
                .get(&source.class_name)
                .is_some_and(|existing| existing != id)
            {
                return Err(mlua::Error::RuntimeError(
                    "Actor local service already exists in the root world".into(),
                ));
            }
        }
        let mut node = source;
        node.name = name.to_owned();
        node.parent = parent.clone();
        node.properties = properties.clone();
        node.local_overrides.clear();
        node.attributes = attributes;
        node.attributes_known = attributes_known;
        node.collection_tags = collection_tags;
        node.collection_tags_known = collection_tags_known;
        node.revisions = property_revisions;
        node.local_revision = 0;
        node.server_owned = false;
        node.destroyed = false;
        node.projected = None;
        world.node_order.push(id.to_owned());
        world.nodes.insert(id.to_owned(), node.clone());
        if local_service_supported(&node.class_name) {
            world
                .services
                .insert(node.class_name.clone(), id.to_owned());
        }
        let mut mutation = json!({"kind":"create","node":{
            "id":node.id,"class_name":node.class_name,"name":node.name,
            "parent":node.parent.as_ref().map(|id| json!(id)).unwrap_or(Json::Null),
            "properties":node.properties,"attributes":node.attributes,
            "attributes_known":node.attributes_known,"collection_tags":node.collection_tags,
            "collection_tags_known":node.collection_tags_known,
            "property_revisions":node.revisions,"destroyed":false,"server_owned":false
        }});
        mutation["actor_origin"] = json!(actor_id);
        world
            .effects
            .entry("mutations")
            .or_insert_with(|| Json::Array(Vec::new()))
            .as_array_mut()
            .ok_or_else(|| mlua::Error::RuntimeError("mutation effect list is invalid".into()))?
            .push(mutation);
    }
    hierarchy_changed(lua, root, id, None, parent)?;
    Ok(())
}

fn is_archivable(ctx: &WorldRef, id: &str) -> mlua::Result<bool> {
    match get_property(ctx, id, "Archivable") {
        Ok(value) => value.as_bool().ok_or_else(|| {
            mlua::Error::RuntimeError(
                "Archivable is unavailable; cannot faithfully clone this instance".into(),
            )
        }),
        Err(_) => {
            let class = ctx
                .borrow()
                .nodes
                .get(id)
                .map(|n| n.class_name.clone())
                .unwrap_or_default();
            constructors()
                .get("classes")
                .and_then(|all| all.get(&class))
                .and_then(|c| c.get("properties"))
                .and_then(|props| props.get("Archivable"))
                .and_then(|p| p.get("value"))
                .and_then(Json::as_bool)
                .ok_or_else(|| {
                    mlua::Error::RuntimeError(
                        "Archivable is unavailable; cannot faithfully clone this instance".into(),
                    )
                })
        }
    }
}

fn remap_clone_refs(
    value: &mut Json,
    id_map: &HashMap<String, String>,
    depth: usize,
) -> mlua::Result<()> {
    if depth > 128 {
        return Err(mlua::Error::RuntimeError(
            "clone reference nesting limit exceeded".into(),
        ));
    }
    if let Some(object) = value.as_object_mut() {
        if object.len() == 1 {
            for key in ["$ref", "$instance_handle"] {
                if let Some(id) = object
                    .get(key)
                    .and_then(Json::as_str)
                    .map(ToOwned::to_owned)
                {
                    if let Some(clone_id) = id_map.get(&id) {
                        object.insert(key.into(), json!(clone_id));
                    }
                    return Ok(());
                }
            }
        }
        for item in object.values_mut() {
            remap_clone_refs(item, id_map, depth + 1)?;
        }
    } else if let Some(array) = value.as_array_mut() {
        for item in array {
            remap_clone_refs(item, id_map, depth + 1)?;
        }
    }
    Ok(())
}

pub fn clone_instance(lua: &Lua, ctx: &WorldRef, root_id: &str) -> mlua::Result<Value> {
    if !ctx.borrow().nodes.contains_key(root_id) {
        return Err(mlua::Error::RuntimeError(
            "instance is not in this world".into(),
        ));
    }
    if !is_archivable(ctx, root_id)? {
        return Ok(Value::Nil);
    }
    let mut selected = Vec::new();
    let mut pending = vec![root_id.to_owned()];
    let mut seen = std::collections::HashSet::new();
    while let Some(id) = pending.pop() {
        if !seen.insert(id.clone()) {
            continue;
        }
        if !is_archivable(ctx, &id)? {
            continue;
        }
        let kids = children(ctx, &id);
        selected.push(id);
        pending.extend(kids.into_iter().rev());
        if selected.len() > 100_000 {
            return Err(mlua::Error::RuntimeError(
                "clone node limit exceeded".into(),
            ));
        }
    }
    let mut id_map = HashMap::with_capacity(selected.len());
    {
        let mut world = ctx.borrow_mut();
        if world.nodes.len().saturating_add(selected.len()) > 101_000 {
            return Err(mlua::Error::RuntimeError(
                "local instance limit exceeded".into(),
            ));
        }
        for source in &selected {
            let id = allocate_local_instance_id(&mut world)?;
            id_map.insert(source.clone(), id);
        }
    }
    let mut clones = Vec::with_capacity(selected.len());
    for source_id in &selected {
        let original = {
            let world = ctx.borrow();
            world
                .nodes
                .get(source_id)
                .cloned()
                .ok_or_else(|| mlua::Error::RuntimeError("clone source disappeared".into()))?
        };
        let mut properties = if let Some(projected) = &original.projected {
            match projected
                .section("properties")
                .map_err(mlua::Error::RuntimeError)?
            {
                ReadOnlyField::Value(Json::Object(map)) => map,
                _ => JsonMap::new(),
            }
        } else {
            original.properties.clone()
        };
        properties.extend(
            original
                .local_overrides
                .iter()
                .map(|(k, v)| (k.clone(), v.clone())),
        );
        properties.remove("Name");
        properties.remove("Parent");
        for value in properties.values_mut() {
            remap_clone_refs(value, &id_map, 0)?;
        }
        let attrs = serde_json::to_value(&original.attributes).map_err(mlua::Error::external)?;
        let mut attributes = attrs;
        remap_clone_refs(&mut attributes, &id_map, 0)?;
        let parent = if source_id == root_id {
            None
        } else {
            original
                .parent
                .as_ref()
                .and_then(|parent| id_map.get(parent))
                .cloned()
        };
        let clone_id = id_map[source_id].clone();
        let node = Node {
            id: clone_id,
            class_name: original.class_name,
            name: original.name,
            parent,
            destroyed: false,
            server_owned: false,
            properties,
            local_overrides: JsonMap::new(),
            attributes: attributes.as_object().cloned().unwrap_or_default(),
            attributes_known: original.attributes_known,
            revisions: JsonMap::new(),
            local_revision: 0,
            collection_tags: original.collection_tags,
            collection_tags_known: original.collection_tags_known,
            projected: None,
        };
        clones.push(node);
    }
    let root_clone = id_map[root_id].clone();
    {
        let mut world = ctx.borrow_mut();
        for node in &clones {
            world.node_order.push(node.id.clone());
            world.nodes.insert(node.id.clone(), node.clone());
        }
    }
    for node in clones {
        emit(
            ctx,
            "mutations",
            json!({"kind":"create","node":{
                "id":node.id,"class_name":node.class_name,"name":node.name,"parent":node.parent.map(Json::String).unwrap_or(Json::Null),
                "properties":node.properties,"attributes":node.attributes,"attributes_known":node.attributes_known,
                "collection_tags":node.collection_tags,"collection_tags_known":node.collection_tags_known,
                "property_revisions":node.revisions,"destroyed":false,"server_owned":false
            }}),
        );
    }
    instance(lua, ctx, &root_clone)
}

pub fn destroy_instance(lua: &Lua, ctx: &WorldRef, root_id: &str) -> mlua::Result<()> {
    if !ctx.borrow().nodes.contains_key(root_id) {
        return Ok(());
    }
    let mut selected = Vec::new();
    let mut pending = vec![root_id.to_owned()];
    let mut seen = std::collections::HashSet::new();
    while let Some(id) = pending.pop() {
        if !seen.insert(id.clone()) {
            continue;
        }
        pending.extend(children(ctx, &id).into_iter().rev());
        selected.push(id);
        if selected.len() > 100_000 {
            return Err(mlua::Error::RuntimeError(
                "destroy subtree limit exceeded".into(),
            ));
        }
    }
    let actor_internal_revision = uses_actor_namespace(ctx);
    let base_revisions = if actor_internal_revision {
        let world = ctx.borrow();
        selected
            .iter()
            .filter_map(|id| {
                world
                    .nodes
                    .get(id)
                    .map(|node| (id.clone(), json!(node.local_revision)))
            })
            .collect::<JsonMap<String, Json>>()
    } else {
        JsonMap::new()
    };
    for id in &selected {
        let old_parent = ctx.borrow().nodes.get(id).and_then(|n| n.parent.clone());
        super::signals::fire(lua, ctx, id, "Destroying", MultiValue::new())?;
        hierarchy_changed(lua, ctx, id, old_parent, None)?;
        let _ = super::bindables::cleanup_instance(lua, ctx, id);
        let _ = super::signals::cleanup_instance(lua, ctx, id);
        if let Some(node) = ctx.borrow_mut().nodes.get_mut(id) {
            node.destroyed = true;
            node.parent = None;
            node.local_overrides.insert("Parent".into(), Json::Null);
        }
    }
    let mut mutation = json!({"kind":"destroy","ids":selected});
    if actor_internal_revision {
        mutation["_base_local_revisions"] = Json::Object(base_revisions);
    }
    emit(ctx, "mutations", mutation);
    Ok(())
}

struct InstanceHandle {
    id: String,
    world: Weak<RefCell<World>>,
}
impl InstanceHandle {
    fn ctx(&self) -> mlua::Result<WorldRef> {
        self.world
            .upgrade()
            .ok_or_else(|| mlua::Error::RuntimeError("world has been released".into()))
    }
}
impl UserData for InstanceHandle {
    fn add_fields<F: UserDataFields<Self>>(fields: &mut F) {
        fields.add_meta_field("__type", "Instance");
        fields.add_field_method_get("Name", |_, this| {
            Ok(this
                .ctx()?
                .borrow()
                .nodes
                .get(&this.id)
                .map(|n| n.name.clone())
                .unwrap_or_default())
        });
        fields.add_field_method_set("Name", |lua, this, name: String| {
            set_property(lua, &this.ctx()?, &this.id, "Name", json!(name))
        });
        fields.add_field_method_get("ClassName", |_, this| {
            Ok(this
                .ctx()?
                .borrow()
                .nodes
                .get(&this.id)
                .map(|n| n.class_name.clone())
                .unwrap_or_default())
        });
        fields.add_field_method_get("Parent", |lua, this| {
            let ctx = this.ctx()?;
            let parent = ctx
                .borrow()
                .nodes
                .get(&this.id)
                .and_then(|n| n.parent.clone());
            match parent {
                Some(id) => instance(lua, &ctx, &id),
                None => Ok(Value::Nil),
            }
        });
        fields.add_field_method_set("Parent", |lua, this, parent: Value| {
            let ctx = this.ctx()?;
            let parent_id = instance_id(&parent);
            if !parent.is_nil() && !instance_belongs_to(&parent, &ctx) {
                return Err(mlua::Error::RuntimeError(
                    "Parent belongs to a different world snapshot".into(),
                ));
            }
            if let Some(ref id) = parent_id {
                if !ctx.borrow().nodes.contains_key(id) {
                    return Err(mlua::Error::RuntimeError(
                        "Parent belongs to a different world snapshot".into(),
                    ));
                }
            }
            set_property(
                lua,
                &ctx,
                &this.id,
                "Parent",
                parent_id.map(|id| json!({"$ref":id})).unwrap_or(Json::Null),
            )
        });
    }
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(MetaMethod::Index, |lua, this, raw_key: String| {
            let ctx = this.ctx()?;
            let class = ctx
                .borrow()
                .nodes
                .get(&this.id)
                .map(|n| n.class_name.clone())
                .unwrap_or_default();
            let key = canonical_member(&class, &raw_key);
            if matches!(key.as_str(), "Name" | "ClassName" | "Parent") {
                return Ok(Value::Nil);
            }
            let method = {
                let world = ctx.borrow();
                class_chain(&class)
                    .iter()
                    .find_map(|class| world.methods.get(&(class.clone(), key.clone())).cloned())
            };
            if let Some(method) = method {
                let id = this.id.clone();
                let ctx2 = ctx.clone();
                return Ok(Value::Function(lua.create_function(
                    move |lua, mut args: MultiValue| {
                        let receiver = args.pop_front().ok_or_else(|| {
                            mlua::Error::RuntimeError("Instance method requires a receiver".into())
                        })?;
                        if instance_id(&receiver).as_deref() != Some(id.as_str())
                            || !instance_belongs_to(&receiver, &ctx2)
                        {
                            return Err(mlua::Error::RuntimeError(
                                "Instance method receiver belongs to a different world or instance"
                                    .into(),
                            ));
                        }
                        method(lua, &ctx2, &id, args)
                    },
                )?));
            }
            let getters = ctx.borrow().getters.get(&key).cloned().unwrap_or_default();
            for getter in getters {
                if let Some(v) = getter(lua, &ctx, &this.id)? {
                    return Ok(v);
                }
            }
            let keyed_getters = ctx
                .borrow()
                .keyed_getters
                .get(&key)
                .cloned()
                .unwrap_or_default();
            for getter in keyed_getters {
                if let Some(v) = getter(lua, &ctx, &this.id, &key)? {
                    return Ok(v);
                }
            }
            if is_signal_name(&key) {
                return super::signals::signal(lua, &ctx, &this.id, &key);
            }
            match get_property(&ctx, &this.id, &key) {
                Ok(json) => value_from_json(lua, &ctx, &json),
                Err(error) => {
                    // Roblox also resolves direct children by name, after members.
                    let child = {
                        let world = ctx.borrow();
                        world
                            .node_order
                            .iter()
                            .find(|id| {
                                world.nodes.get(*id).is_some_and(|node| {
                                    node.parent.as_deref() == Some(this.id.as_str())
                                        && node.name == key
                                })
                            })
                            .cloned()
                    };
                    match child {
                        Some(id) => instance(lua, &ctx, &id),
                        None => Err(error),
                    }
                }
            }
        });
        methods.add_meta_method_mut(
            MetaMethod::NewIndex,
            |lua, this, (raw_key, value): (String, Value)| {
                let ctx = this.ctx()?;
                let class = ctx
                    .borrow()
                    .nodes
                    .get(&this.id)
                    .map(|n| n.class_name.clone())
                    .unwrap_or_default();
                let key = canonical_member(&class, &raw_key);
                if matches!(key.as_str(), "ClassName") {
                    return Err(mlua::Error::RuntimeError("ClassName is read-only".into()));
                }
                if let Some(enum_type) = local_part_enum_type(&class, &key) {
                    let Value::UserData(item) = &value else {
                        return Err(mlua::Error::RuntimeError(format!(
                            "{key} requires Enum.{enum_type} item"
                        )));
                    };
                    if !super::enums::enum_item_matches(item, enum_type)? {
                        return Err(mlua::Error::RuntimeError(format!(
                            "{key} requires Enum.{enum_type} item"
                        )));
                    }
                }
                let setters = ctx.borrow().setters.get(&key).cloned().unwrap_or_default();
                for setter in setters {
                    if setter(lua, &ctx, &this.id, value.clone())? {
                        return Ok(());
                    }
                }
                let keyed_setters = ctx
                    .borrow()
                    .keyed_setters
                    .get(&key)
                    .cloned()
                    .unwrap_or_default();
                for setter in keyed_setters {
                    if setter(lua, &ctx, &this.id, &key, value.clone())? {
                        return Ok(());
                    }
                }
                let json = super::values::to_json(lua, &value)?;
                set_property(lua, &ctx, &this.id, &key, json)
            },
        );
        methods.add_meta_method(MetaMethod::ToString, |_, this, ()| {
            let ctx = this.ctx()?;
            let world = ctx.borrow();
            let n = world.nodes.get(&this.id);
            Ok(
                n.map(|n| format!("{}.{}", n.parent.as_deref().unwrap_or("game"), n.name))
                    .unwrap_or_else(|| "<destroyed Instance>".into()),
            )
        });
        methods.add_method("GetChildren", |lua, this, ()| {
            let ctx = this.ctx()?;
            let ids = children(&ctx, &this.id);
            let table = lua.create_table_with_capacity(ids.len(), 0)?;
            for (i, id) in ids.iter().enumerate() {
                table.raw_set(i + 1, instance(lua, &ctx, id)?)?;
            }
            Ok(table)
        });
        methods.add_method("GetDescendants", |lua, this, ()| {
            let ctx = this.ctx()?;
            let out = descendants(&ctx, &this.id);
            if out.len() > 100_000 {
                return Err(mlua::Error::RuntimeError(
                    "descendant limit exceeded".into(),
                ));
            }
            let table = lua.create_table_with_capacity(out.len(), 0)?;
            for (i, id) in out.iter().enumerate() {
                table.raw_set(i + 1, instance(lua, &ctx, id)?)?;
            }
            Ok(table)
        });
        methods.add_method("QueryDescendants", |lua, this, selector: String| {
            let ctx = this.ctx()?;
            world_queries::query(lua, &ctx, &this.id, &selector)
        });
        methods.add_method("GetActor", |lua, this, ()| {
            let ctx = this.ctx()?;
            let world = ctx.borrow();
            let mut current = Some(this.id.clone());
            let mut seen = std::collections::HashSet::new();
            let mut found = None;
            while let Some(id) = current {
                if !seen.insert(id.clone()) {
                    break;
                }
                let Some(node) = world.nodes.get(&id).filter(|node| !node.destroyed) else {
                    break;
                };
                if class_chain(&node.class_name)
                    .iter()
                    .any(|class| class == "Actor")
                {
                    found = Some(id);
                    break;
                }
                current = node.parent.clone();
            }
            drop(world);
            match found {
                Some(id) => instance(lua, &ctx, &id),
                None => Ok(Value::Nil),
            }
        });
        methods.add_method("IsPropertyModified", |lua, this, raw_key: String| {
            let ctx = this.ctx()?;
            let class = ctx
                .borrow()
                .nodes
                .get(&this.id)
                .map(|node| node.class_name.clone())
                .ok_or_else(|| mlua::Error::RuntimeError("instance is not in this world".into()))?;
            let key = canonical_member(&class, &raw_key);
            let (default, _) = constructor_default(&class, &key).map_err(|error| {
                mlua::Error::RuntimeError(format!("IsPropertyModified failed: {error}"))
            })?;
            let Value::UserData(object) = instance(lua, &ctx, &this.id)? else {
                return Err(mlua::Error::RuntimeError(
                    "IsPropertyModified failed: instance is unavailable".into(),
                ));
            };
            let current: Value = object.get(key.as_str()).map_err(|error| {
                mlua::Error::RuntimeError(format!("IsPropertyModified failed: {error}"))
            })?;
            let expected = value_from_json(lua, &ctx, &default).map_err(|error| {
                mlua::Error::RuntimeError(format!("IsPropertyModified failed: {error}"))
            })?;
            let current = super::values::to_json(lua, &current).map_err(|error| {
                mlua::Error::RuntimeError(format!("IsPropertyModified failed: {error}"))
            })?;
            let expected = super::values::to_json(lua, &expected).map_err(|error| {
                mlua::Error::RuntimeError(format!("IsPropertyModified failed: {error}"))
            })?;
            Ok(!json_property_equal(&current, &expected))
        });
        methods.add_method("ResetPropertyToDefault", |lua, this, raw_key: String| {
            let ctx = this.ctx()?;
            let class = ctx
                .borrow()
                .nodes
                .get(&this.id)
                .map(|node| node.class_name.clone())
                .ok_or_else(|| mlua::Error::RuntimeError("instance is not in this world".into()))?;
            let key = canonical_member(&class, &raw_key);
            let (default, writable) = constructor_default(&class, &key).map_err(|error| {
                mlua::Error::RuntimeError(format!("ResetPropertyToDefault failed: {error}"))
            })?;
            if !writable {
                return Err(mlua::Error::RuntimeError(
                    "ResetPropertyToDefault failed: property is read-only".into(),
                ));
            }
            let value = value_from_json(lua, &ctx, &default).map_err(|error| {
                mlua::Error::RuntimeError(format!("ResetPropertyToDefault failed: {error}"))
            })?;
            let setters = ctx.borrow().setters.get(&key).cloned().unwrap_or_default();
            for setter in setters {
                if setter(lua, &ctx, &this.id, value.clone())? {
                    return Ok(());
                }
            }
            let keyed_setters = ctx
                .borrow()
                .keyed_setters
                .get(&key)
                .cloned()
                .unwrap_or_default();
            for setter in keyed_setters {
                if setter(lua, &ctx, &this.id, &key, value.clone())? {
                    return Ok(());
                }
            }
            let value = super::values::to_json(lua, &value).map_err(|error| {
                mlua::Error::RuntimeError(format!("ResetPropertyToDefault failed: {error}"))
            })?;
            set_property(lua, &ctx, &this.id, &key, value).map_err(|error| {
                mlua::Error::RuntimeError(format!("ResetPropertyToDefault failed: {error}"))
            })
        });
        methods.add_method(
            "FindFirstChild",
            |lua, this, (name, recursive): (String, Option<bool>)| {
                let ctx = this.ctx()?;
                let candidates = if recursive.unwrap_or(false) {
                    descendants(&ctx, &this.id)
                } else {
                    children(&ctx, &this.id)
                };
                let id = candidates
                    .into_iter()
                    .find(|id| ctx.borrow().nodes.get(id).is_some_and(|n| n.name == name));
                match id {
                    Some(id) => instance(lua, &ctx, &id),
                    None => Ok(Value::Nil),
                }
            },
        );
        methods.add_method(
            "FindFirstChildOfClass",
            |lua, this, (class, recursive): (String, Option<bool>)| {
                let ctx = this.ctx()?;
                let candidates = if recursive.unwrap_or(false) {
                    descendants(&ctx, &this.id)
                } else {
                    children(&ctx, &this.id)
                };
                let id = candidates.into_iter().find(|id| {
                    ctx.borrow()
                        .nodes
                        .get(id)
                        .is_some_and(|n| n.class_name == class)
                });
                match id {
                    Some(id) => instance(lua, &ctx, &id),
                    None => Ok(Value::Nil),
                }
            },
        );
        methods.add_method(
            "FindFirstChildWhichIsA",
            |lua, this, (class, recursive): (String, Option<bool>)| {
                let ctx = this.ctx()?;
                let candidates = if recursive.unwrap_or(false) {
                    descendants(&ctx, &this.id)
                } else {
                    children(&ctx, &this.id)
                };
                let id = candidates.into_iter().find(|id| is_a(&ctx, id, &class));
                match id {
                    Some(id) => instance(lua, &ctx, &id),
                    None => Ok(Value::Nil),
                }
            },
        );
        methods.add_method("GetService", |lua, this, name: String| {
            let ctx = this.ctx()?;
            let (receiver_class, receiver_is_game, receiver_is_settings, mapped) = {
                let world = ctx.borrow();
                let node = world.nodes.get(&this.id).ok_or_else(|| {
                    mlua::Error::RuntimeError("instance is not in this world".into())
                })?;
                let game_id = world
                    .nodes
                    .values()
                    .find(|n| n.class_name == "DataModel" && !n.destroyed)
                    .map(|n| n.id.as_str());
                (
                    node.class_name.clone(),
                    game_id == Some(this.id.as_str()),
                    node.class_name == "UserSettings",
                    world.services.get(&name).cloned(),
                )
            };
            if receiver_is_settings {
                if name != "UserGameSettings" {
                    return Err(mlua::Error::RuntimeError(
                        "unknown user settings service".into(),
                    ));
                }
                let id = mapped.ok_or_else(|| {
                    mlua::Error::RuntimeError("UserGameSettings model unavailable".into())
                })?;
                return instance(lua, &ctx, &id);
            }
            if !receiver_is_game {
                return Err(mlua::Error::RuntimeError(
                    "GetService requires ServiceProvider".into(),
                ));
            }
            if let Some(id) = mapped {
                if !ctx.borrow().nodes.contains_key(&id) {
                    return Err(mlua::Error::RuntimeError(
                        "snapshot service reference is invalid".into(),
                    ));
                }
                return instance(lua, &ctx, &id);
            }
            if local_service_supported(&name) {
                return ensure_client_service(lua, &ctx, &name);
            }
            let _ = receiver_class;
            Err(mlua::Error::RuntimeError(format!(
                "service unavailable in snapshot: {name}"
            )))
        });
        methods.add_method("FindService", |lua, this, name: String| {
            let ctx = this.ctx()?;
            let (receiver_is_game, receiver_is_settings, mapped, local_id) = {
                let world = ctx.borrow();
                let node = world.nodes.get(&this.id).ok_or_else(|| {
                    mlua::Error::RuntimeError("instance is not in this world".into())
                })?;
                let game_id = world
                    .nodes
                    .values()
                    .find(|n| n.class_name == "DataModel" && !n.destroyed)
                    .map(|n| n.id.as_str());
                let mapped = world.services.get(&name).cloned();
                let local_id = if mapped.is_none() && local_service_supported(&name) {
                    world.node_order.iter().find_map(|id| {
                        world
                            .nodes
                            .get(id)
                            .filter(|n| !n.server_owned && !n.destroyed && n.class_name == name)
                            .map(|n| n.id.clone())
                    })
                } else {
                    None
                };
                (
                    game_id == Some(this.id.as_str()),
                    node.class_name == "UserSettings",
                    mapped,
                    local_id,
                )
            };
            if receiver_is_settings {
                if name != "UserGameSettings" {
                    return Ok(Value::Nil);
                }
                return match mapped {
                    Some(id) => instance(lua, &ctx, &id),
                    None => Ok(Value::Nil),
                };
            }
            if !receiver_is_game {
                return Err(mlua::Error::RuntimeError(
                    "FindService requires ServiceProvider".into(),
                ));
            }
            let id = mapped.or(local_id);
            match id {
                Some(id) => instance(lua, &ctx, &id),
                None => Ok(Value::Nil),
            }
        });
        methods.add_method("GetFullName", |_, this, ()| {
            let ctx = this.ctx()?;
            let world = ctx.borrow();
            let mut names = Vec::new();
            let mut current = Some(this.id.clone());
            let mut seen = 0;
            while let Some(id) = current {
                let Some(node) = world.nodes.get(&id) else {
                    break;
                };
                names.push(node.name.clone());
                current = node.parent.clone();
                seen += 1;
                if seen > 100_000 {
                    return Err(mlua::Error::RuntimeError("parent cycle detected".into()));
                }
            }
            names.reverse();
            Ok(names.join("."))
        });
        methods.add_method("IsDescendantOf", |_, this, other: AnyUserData| {
            let ctx = this.ctx()?;
            let other_value = Value::UserData(other.clone());
            if !instance_belongs_to(&other_value, &ctx) {
                return Err(mlua::Error::RuntimeError(
                    "argument belongs to a different world snapshot".into(),
                ));
            }
            let parent = other
                .borrow::<InstanceHandle>()
                .map_err(|_| mlua::Error::RuntimeError("expected Instance".into()))?
                .id
                .clone();
            let world = ctx.borrow();
            let mut current = world.nodes.get(&this.id).and_then(|n| n.parent.clone());
            let mut seen = 0;
            while let Some(id) = current {
                if id == parent {
                    return Ok(true);
                }
                current = world.nodes.get(&id).and_then(|n| n.parent.clone());
                seen += 1;
                if seen > 100_000 {
                    break;
                }
            }
            Ok(false)
        });
        methods.add_method("IsAncestorOf", |_, this, other: AnyUserData| {
            let ctx = this.ctx()?;
            let other_value = Value::UserData(other.clone());
            if !instance_belongs_to(&other_value, &ctx) {
                return Err(mlua::Error::RuntimeError(
                    "argument belongs to a different world snapshot".into(),
                ));
            }
            let descendant = other
                .borrow::<InstanceHandle>()
                .map_err(|_| mlua::Error::RuntimeError("expected Instance".into()))?
                .id
                .clone();
            let world = ctx.borrow();
            let mut current = world.nodes.get(&descendant).and_then(|n| n.parent.clone());
            let mut seen = 0;
            while let Some(id) = current {
                if id == this.id {
                    return Ok(true);
                }
                current = world.nodes.get(&id).and_then(|n| n.parent.clone());
                seen += 1;
                if seen > 100_000 {
                    break;
                }
            }
            Ok(false)
        });
        methods.add_method("Destroy", |lua, this, ()| {
            destroy_instance(lua, &this.ctx()?, &this.id)
        });
        methods.add_method("Clone", |lua, this, ()| {
            clone_instance(lua, &this.ctx()?, &this.id)
        });
        methods.add_method("Remove", |lua, this, ()| {
            set_property(lua, &this.ctx()?, &this.id, "Parent", Json::Null)
        });
        methods.add_method("IsA", |_, this, class: String| {
            Ok(is_a(&this.ctx()?, &this.id, &class))
        });
        methods.add_method("FindFirstAncestor", |lua, this, name: String| {
            let ctx = this.ctx()?;
            let world = ctx.borrow();
            let mut current = world
                .nodes
                .get(&this.id)
                .and_then(|node| node.parent.clone());
            let mut seen = std::collections::HashSet::new();
            let mut found = None;
            while let Some(id) = current {
                if !seen.insert(id.clone()) {
                    break;
                }
                let Some(node) = world.nodes.get(&id) else {
                    break;
                };
                if node.name == name {
                    found = Some(id);
                    break;
                }
                current = node.parent.clone();
            }
            drop(world);
            match found {
                Some(id) => instance(lua, &ctx, &id),
                None => Ok(Value::Nil),
            }
        });
        methods.add_method("FindFirstAncestorOfClass", |lua, this, class: String| {
            let ctx = this.ctx()?;
            let world = ctx.borrow();
            let mut current = world
                .nodes
                .get(&this.id)
                .and_then(|node| node.parent.clone());
            let mut seen = std::collections::HashSet::new();
            let mut found = None;
            while let Some(id) = current {
                if !seen.insert(id.clone()) {
                    break;
                }
                let Some(node) = world.nodes.get(&id) else {
                    break;
                };
                if node.class_name == class {
                    found = Some(id);
                    break;
                }
                current = node.parent.clone();
            }
            drop(world);
            match found {
                Some(id) => instance(lua, &ctx, &id),
                None => Ok(Value::Nil),
            }
        });
        methods.add_method("FindFirstAncestorWhichIsA", |lua, this, class: String| {
            let ctx = this.ctx()?;
            let world = ctx.borrow();
            let mut current = world
                .nodes
                .get(&this.id)
                .and_then(|node| node.parent.clone());
            let mut seen = std::collections::HashSet::new();
            let mut found = None;
            while let Some(id) = current {
                if !seen.insert(id.clone()) {
                    break;
                }
                let Some(node) = world.nodes.get(&id) else {
                    break;
                };
                if class_chain(&node.class_name)
                    .iter()
                    .any(|ancestor| ancestor == &class)
                {
                    found = Some(id);
                    break;
                }
                current = node.parent.clone();
            }
            drop(world);
            match found {
                Some(id) => instance(lua, &ctx, &id),
                None => Ok(Value::Nil),
            }
        });
        methods.add_method("GetAttribute", |lua, this, name: String| {
            let ctx = this.ctx()?;
            let (value, known, class_name) = {
                let world = ctx.borrow();
                let node = world.nodes.get(&this.id).ok_or_else(|| {
                    mlua::Error::RuntimeError("instance is not in this world".into())
                })?;
                (
                    node.attributes.get(&name).cloned(),
                    node.attributes_known,
                    node.class_name.clone(),
                )
            };
            if let Some(value) = value {
                return value_from_json(lua, &ctx, &value);
            }
            if !known {
                return Err(mlua::Error::RuntimeError(format!(
                    "attributes unavailable in snapshot for {}",
                    class_name
                )));
            }
            Ok(Value::Nil)
        });
        methods.add_method("GetPropertyChangedSignal", |lua, this, property: String| {
            super::signals::signal(lua, &this.ctx()?, &this.id, &format!("property:{property}"))
        });
        methods.add_method(
            "GetAttributeChangedSignal",
            |lua, this, attribute: String| {
                super::signals::signal(
                    lua,
                    &this.ctx()?,
                    &this.id,
                    &format!("attribute:{attribute}"),
                )
            },
        );
        methods.add_method("GetAttributes", |lua, this, ()| {
            let ctx = this.ctx()?;
            let (attributes, known) = {
                let world = ctx.borrow();
                let node = world.nodes.get(&this.id).ok_or_else(|| {
                    mlua::Error::RuntimeError("instance is not in this world".into())
                })?;
                (node.attributes.clone(), node.attributes_known)
            };
            if !known {
                return Err(mlua::Error::RuntimeError(format!(
                    "attributes unavailable in snapshot for {}",
                    ctx.borrow().nodes[&this.id].class_name
                )));
            }
            let table = lua.create_table_with_capacity(attributes.len(), 0)?;
            for (key, value) in &attributes {
                table.set(key.as_str(), value_from_json(lua, &ctx, value)?)?;
            }
            Ok(table)
        });
        methods.add_method(
            "SetAttribute",
            |lua, this, (name, value): (String, Value)| {
                if name.is_empty()
                    || name.len() > 100
                    || name.starts_with("RBX")
                    || !name
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || "_./-".contains(c))
                {
                    return Err(mlua::Error::RuntimeError(
                        "invalid or reserved attribute name".into(),
                    ));
                }
                let ctx = this.ctx()?;
                let mut json = super::values::to_json(lua, &value)?;
                if let Some(id) = instance_id(&value) {
                    json = json!({"$instance_handle":id});
                }
                let mut world = ctx.borrow_mut();
                let actor_internal_revision = world.local_namespace.starts_with("local:actor:");
                let node = world.nodes.get_mut(&this.id).ok_or_else(|| {
                    mlua::Error::RuntimeError("instance is not in this world".into())
                })?;
                let base_local_revision = node.local_revision;
                let mut attributes = crate::engine_attributes::AttributeStore::from_snapshot(
                    Json::Object(node.attributes.clone()),
                    node.attributes_known,
                )
                .map_err(|error| mlua::Error::RuntimeError(error.to_string()))?;
                let changed = attributes
                    .set(
                        &name,
                        if matches!(value, Value::Nil) {
                            None
                        } else {
                            Some(json.clone())
                        },
                    )
                    .map_err(|error| mlua::Error::RuntimeError(error.to_string()))?;
                node.attributes = attributes
                    .snapshot()
                    .as_object()
                    .cloned()
                    .unwrap_or_default();
                node.attributes_known = attributes.is_known();
                if changed {
                    world.local_revision = world.local_revision.wrapping_add(1).max(1);
                    let revision = world.local_revision;
                    if let Some(node) = world.nodes.get_mut(&this.id) {
                        node.local_revision = revision;
                    }
                }
                drop(world);
                let mut mutation =
                    json!({"kind":"attribute","id":this.id,"name":name,"value":json});
                if actor_internal_revision {
                    mutation["_base_local_revision"] = json!(base_local_revision);
                }
                emit(&ctx, "mutations", mutation);
                if changed {
                    fire_attribute_changed(lua, &ctx, &this.id, &name)?;
                }
                Ok(())
            },
        );
    }
}

fn is_signal_name(key: &str) -> bool {
    matches!(
        key,
        "Changed"
            | "Loaded"
            | "ChildAdded"
            | "ChildRemoved"
            | "DescendantAdded"
            | "DescendantRemoving"
            | "AncestryChanged"
            | "Destroying"
            | "Touched"
            | "TouchStarted"
            | "TouchMoved"
            | "TouchEnded"
            | "TouchLongPress"
            | "TouchTap"
            | "TouchPan"
            | "ComputerCameraMovementModeRegistered"
            | "ComputerMovementModeRegistered"
            | "TouchCameraMovementModeRegistered"
            | "TouchMovementModeRegistered"
            | "RecordingCompleted"
            | "PlaybackCompleted"
            | "Heartbeat"
            | "Stepped"
            | "RenderStepped"
            | "PreRender"
            | "PostSimulation"
            | "CharacterAdded"
            | "CharacterRemoving"
            | "PlayerAdded"
            | "PlayerRemoving"
            | "InputBegan"
            | "InputChanged"
            | "InputEnded"
            | "MouseButton1Click"
            | "MouseButton1Down"
            | "MouseButton1Up"
            | "MouseEnter"
            | "MouseLeave"
            | "Activated"
            | "Focused"
            | "FocusLost"
            | "TextChanged"
            | "OnClientEvent"
            | "OnServerEvent"
            | "LastInputTypeChanged"
            | "WindowFocused"
            | "WindowFocusReleased"
            | "TextBoxFocused"
            | "TextBoxFocusReleased"
            | "Button1Down"
            | "Button1Up"
            | "Button2Down"
            | "Button2Up"
            | "Move"
            | "WheelForward"
            | "WheelBackward"
    )
}

fn refresh_globals(lua: &Lua, ctx: &WorldRef) -> mlua::Result<()> {
    let (game_id, workspace_id) = {
        let world = ctx.borrow();
        let game = world
            .node_order
            .iter()
            .filter(|id| {
                world
                    .nodes
                    .get(*id)
                    .is_some_and(|n| n.class_name == "DataModel" && !n.destroyed)
            })
            .min_by_key(|id| id.as_str() == "__world_snapshot_datamodel__")
            .cloned();
        (game, world.services.get("Workspace").cloned())
    };
    let globals = lua.globals();
    globals.set(
        "game",
        match game_id {
            Some(id) => instance(lua, ctx, &id)?,
            None => Value::Nil,
        },
    )?;
    globals.set(
        "workspace",
        match workspace_id {
            Some(id) => instance(lua, ctx, &id)?,
            None => Value::Nil,
        },
    )?;
    Ok(())
}

pub fn install(lua: &Lua, ctx: &WorldRef) -> mlua::Result<()> {
    lua.set_app_data(Rc::downgrade(ctx));
    if lua.app_data_ref::<InstanceHandleWaiters>().is_none() {
        lua.set_app_data(Rc::new(RefCell::new(InstanceHandleWaitState::default())));
    }
    if lua.app_data_ref::<super::signals::SignalHubRef>().is_none() {
        super::signals::install(lua, ctx)?;
    }
    super::values::register_codec(
        lua,
        super::values::Codec {
            key: "$ref",
            type_name: "Instance",
            decode: decode_instance_ref,
            encode: encode_instance_ref,
        },
    )?;
    super::values::register_codec(
        lua,
        super::values::Codec {
            key: "$instance_handle",
            type_name: "InstanceHandle",
            decode: decode_instance_handle,
            encode: encode_instance_handle,
        },
    )?;
    let globals = lua.globals();
    refresh_globals(lua, ctx)?;
    for service in ["UserInputService", "RunService", "CoreGui", "CorePackages"] {
        if !ctx.borrow().services.contains_key(service) {
            ensure_client_service(lua, ctx, service)?;
        }
    }
    let instance_table = lua.create_table()?;
    let weak = Rc::downgrade(ctx);
    instance_table.set(
        "new",
        lua.create_function(move |lua, (class, parent): (String, Option<AnyUserData>)| {
            let ctx = weak
                .upgrade()
                .ok_or_else(|| mlua::Error::RuntimeError("world has been released".into()))?;
            let parent_id = if let Some(parent) = parent {
                let parent = parent
                    .borrow::<InstanceHandle>()
                    .map_err(|_| mlua::Error::RuntimeError("Parent must be an Instance".into()))?;
                let other = parent
                    .world
                    .upgrade()
                    .ok_or_else(|| mlua::Error::RuntimeError("world has been released".into()))?;
                if !Rc::ptr_eq(&other, &ctx) {
                    return Err(mlua::Error::RuntimeError(
                        "Parent belongs to a different world snapshot".into(),
                    ));
                }
                Some(parent.id.clone())
            } else {
                None
            };
            create_instance(lua, &ctx, &class, parent_id.as_deref())
        })?,
    )?;
    globals.set("Instance", instance_table)?;
    let handle_table = lua.create_table()?;
    let weak = Rc::downgrade(ctx);
    handle_table.set(
        "new",
        lua.create_function(move |lua, value: Option<Value>| {
            let ctx = weak
                .upgrade()
                .ok_or_else(|| mlua::Error::RuntimeError("world has been released".into()))?;
            let id = match value.unwrap_or(Value::Nil) {
                Value::Nil => String::new(),
                Value::UserData(data) => {
                    let handle = data.borrow::<InstanceHandle>().map_err(|_| {
                        mlua::Error::RuntimeError("expected an Instance or nil".into())
                    })?;
                    let other = handle.world.upgrade().ok_or_else(|| {
                        mlua::Error::RuntimeError("world has been released".into())
                    })?;
                    if !Rc::ptr_eq(&other, &ctx) {
                        return Err(mlua::Error::RuntimeError(
                            "Instance belongs to a different snapshot".into(),
                        ));
                    }
                    handle.id.clone()
                }
                _ => {
                    return Err(mlua::Error::RuntimeError(
                        "expected an Instance or nil".into(),
                    ));
                }
            };
            Ok(Value::UserData(lua.create_userdata(InstanceRefValue {
                id,
                world: Rc::downgrade(&ctx),
            })?))
        })?,
    )?;
    handle_table.set_readonly(true);
    globals.set("InstanceHandle", handle_table)?;
    let client: mlua::Table = globals.get("client").unwrap_or(lua.create_table()?);
    {
        let world = ctx.borrow();
        let persistent = world
            .snapshot
            .get("persistent")
            .and_then(Json::as_bool)
            .unwrap_or(false);
        client.set("snapshot", !persistent)?;
        client.set("persistent", persistent)?;
        if world.sequence > 0 {
            client.set("sequence", world.sequence)?;
        }
    }
    let weak = Rc::downgrade(ctx);
    client.set(
        "propertyOrigin",
        lua.create_function(move |_, (object, property): (Value, String)| {
            let ctx = weak
                .upgrade()
                .ok_or_else(|| mlua::Error::RuntimeError("world has been released".into()))?;
            let id = instance_id(&object)
                .filter(|_| instance_belongs_to(&object, &ctx))
                .ok_or_else(|| {
                    mlua::Error::RuntimeError(
                        "propertyOrigin expects an Instance from this world".into(),
                    )
                })?;
            let class = ctx
                .borrow()
                .nodes
                .get(&id)
                .map(|node| node.class_name.clone())
                .ok_or_else(|| mlua::Error::RuntimeError("instance is not in this world".into()))?;
            let key = canonical_member(&class, &property);
            property_origin(&ctx, &id, &key)
        })?,
    )?;
    globals.set("client", client)?;
    Ok(())
}
