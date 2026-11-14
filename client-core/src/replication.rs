//! decode the data-only replication stream with size limits.
//! each decode owns its delta, events, ping records and new shared strings.
//! apply the world delta first, then commit. bad or rejected data must never
//! move the decoder forward.
use crate::schema::Schema;
use crate::wire::{decompress_bytes, decompress_stream, Reader, WireError};
use crate::{
    packed_properties::CompactSharedProperty,
    shared_payload::{digest_raw, SharedPayload},
};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

const MAX_PACKET: usize = 16 * 1024 * 1024;
const MAX_PROPERTIES: usize = 100_000;
const MAX_SHARED_BYTES: usize = 32 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq)]
pub struct Globals {
    pub scope: u64,
    pub flags: Vec<bool>,
    /// Root-store normalized create actions; classes are schema names.
    pub world_delta: Value,
    base_sequence: u64,
    next_sequence: u64,
    base_generation: u64,
    next_region: Option<[i32; 3]>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PacketTransaction {
    pub world_delta: Value,
    pub events: Vec<Value>,
    pub pings: Vec<Value>,
    pub tags: Vec<i32>,
    pub regions: Vec<Value>,
    pub metadata: Vec<Value>,
    pub shared_additions: Vec<([u8; 16], Arc<SharedPayload>)>,
    pub property_changes: Vec<Value>,
    next_sequence: u64,
    base_sequence: u64,
    base_generation: u64,
    next_region: Option<[i32; 3]>,
}

/// Internal SessionCore transaction. Direct shared scalar properties remain
/// typed references until the staged world has accepted the packet.
#[derive(Clone, Debug)]
pub(crate) struct CompactPacketTransaction {
    pub(crate) packet: PacketTransaction,
    pub(crate) shared_properties: Vec<CompactSharedProperty>,
}

impl CompactPacketTransaction {
    pub(crate) fn into_eager(mut self) -> Result<PacketTransaction, WireError> {
        let actions = self
            .packet
            .world_delta
            .get_mut("actions")
            .and_then(Value::as_array_mut)
            .ok_or_else(|| WireError::new("Compact transaction actions are invalid"))?;
        for binding in self.shared_properties {
            let action = actions
                .get_mut(binding.action_index)
                .ok_or_else(|| WireError::new("Compact property action is missing"))?;
            materialize_shared_property(action, &binding)?;
        }
        Ok(self.packet)
    }
}

pub(crate) fn materialize_shared_property(
    action: &mut Value,
    binding: &CompactSharedProperty,
) -> Result<(), WireError> {
    let object = action
        .as_object_mut()
        .ok_or_else(|| WireError::new("Compact property action is invalid"))?;
    let kind = object.get("kind").and_then(Value::as_str);
    let target = match kind {
        Some("create") => object
            .get_mut("node")
            .and_then(Value::as_object_mut)
            .and_then(|node| node.get_mut("properties"))
            .and_then(Value::as_object_mut),
        Some("set") => object.get_mut("fields").and_then(Value::as_object_mut),
        _ => None,
    }
    .ok_or_else(|| WireError::new("Compact property target is invalid"))?;
    let value = target
        .get_mut(&binding.property)
        .ok_or_else(|| WireError::new("Compact property placeholder is missing"))?;
    if !value.is_null() {
        return Err(WireError::new("Compact property placeholder was changed"));
    }
    *value = if binding.force_bytes {
        bytes_value(binding.payload.raw())
    } else {
        bytes_or_text(binding.payload.raw())
    };
    Ok(())
}

impl PacketTransaction {
    pub fn world_action_count(&self) -> usize {
        self.world_delta["actions"].as_array().map_or(0, Vec::len)
    }
}

/// Session-local decoding state. It contains only shared strings and the
/// action sequence; instance ownership belongs to `WorldStore`.
pub struct ReplicationDecoder {
    pub schema: Schema,
    client_scope: Option<u64>,
    global_flags: Option<Vec<bool>>,
    shared: HashMap<[u8; 16], Arc<SharedPayload>>,
    shared_bytes: usize,
    shared_stored_bytes: usize,
    sequence: u64,
    generation: u64,
    pub preserve_remote_success_variants: bool,
    last_region: Option<[i32; 3]>,
}

impl ReplicationDecoder {
    pub fn new(schema: Schema) -> Self {
        Self {
            schema,
            client_scope: None,
            global_flags: None,
            shared: HashMap::new(),
            shared_bytes: 0,
            shared_stored_bytes: 0,
            sequence: 0,
            generation: 0,
            preserve_remote_success_variants: false,
            last_region: None,
        }
    }

    pub fn shared_string_count(&self) -> usize {
        self.shared.len()
    }
    /// Logical bytes in retained shared-string payloads; this is payload size,
    /// not an estimate of allocator overhead or process RSS.
    pub fn shared_string_payload_bytes(&self) -> usize {
        self.shared_bytes
    }
    /// Retained raw-or-compressed payload bytes, excluding allocator overhead.
    pub fn shared_string_stored_bytes(&self) -> usize {
        self.shared_stored_bytes
    }
    /// Resolve an exact shared-string payload against committed state and the
    /// additions staged by `transaction`. Building the staged digest index is
    /// O(additions); each property lookup is then O(1) average and clones only
    /// the matching `Arc`, never its payload bytes.
    pub(crate) fn shared_payload_resolver<'a>(
        &'a self,
        transaction: &'a PacketTransaction,
    ) -> impl Fn(&[u8]) -> Option<Arc<SharedPayload>> + 'a {
        let mut staged: HashMap<[u8; 16], Vec<Arc<SharedPayload>>> = HashMap::new();
        for (digest, payload) in &transaction.shared_additions {
            staged.entry(*digest).or_default().push(Arc::clone(payload));
        }
        move |bytes| {
            let digest = digest_raw(bytes);
            self.shared
                .get(&digest)
                .filter(|payload| payload.matches_exact(bytes))
                .cloned()
                .or_else(|| {
                    staged.get(&digest).and_then(|candidates| {
                        candidates
                            .iter()
                            .find(|payload| payload.matches_exact(bytes))
                            .cloned()
                    })
                })
        }
    }
    pub fn sequence(&self) -> u64 {
        self.sequence
    }
    pub fn client_scope(&self) -> Option<u64> {
        self.client_scope
    }
    pub fn global_flags(&self) -> Option<&[bool]> {
        self.global_flags.as_deref()
    }

    pub fn validate_globals(&self, globals: &Globals) -> Result<(), WireError> {
        if globals.base_sequence != self.sequence || globals.base_generation != self.generation {
            return Err(WireError::new("Globals transaction is stale"));
        }
        self.generation
            .checked_add(1)
            .ok_or_else(|| WireError::new("Replication generation overflow"))?;
        Ok(())
    }

    pub fn validate_commit(&self, transaction: &PacketTransaction) -> Result<(), WireError> {
        if transaction.base_sequence != self.sequence
            || transaction.base_generation != self.generation
        {
            return Err(WireError::new("Replication transaction is stale"));
        }
        self.generation
            .checked_add(1)
            .ok_or_else(|| WireError::new("Replication generation overflow"))?;
        let mut staged = HashMap::<[u8; 16], &Arc<SharedPayload>>::new();
        for (key, value) in &transaction.shared_additions {
            if value.digest() != *key {
                return Err(WireError::new("Shared-string digest mismatch at commit"));
            }
            if let Some(old) = self.shared.get(key) {
                if !old.matches_exact(value.raw().as_ref()) {
                    return Err(WireError::new("Shared-string key collision"));
                }
            }
            if let Some(old) = staged.insert(*key, value) {
                if !old.matches_exact(value.raw().as_ref()) {
                    return Err(WireError::new("Shared-string key collision"));
                }
            }
        }
        let additional = transaction
            .shared_additions
            .iter()
            .filter(|(key, _)| !self.shared.contains_key(key))
            .try_fold(0usize, |sum, (_, value)| sum.checked_add(value.len()))
            .ok_or_else(|| WireError::new("Shared-string memory limit exceeded"))?;
        if self
            .shared_bytes
            .checked_add(additional)
            .filter(|n| *n <= MAX_SHARED_BYTES)
            .is_none()
        {
            return Err(WireError::new("Shared-string memory limit exceeded"));
        }
        Ok(())
    }

    pub fn globals(&self, payload: &[u8]) -> Result<Globals, WireError> {
        if payload.len() > MAX_PACKET {
            return Err(WireError::new("Globals packet exceeds limit"));
        }
        let mut r = Reader::new(payload);
        if r.u8()? != 129 {
            return Err(WireError::new("Not a globals packet"));
        }
        let raw_flags = r.take(7)?;
        if raw_flags.iter().any(|v| *v > 1) {
            return Err(WireError::new("Invalid globals boolean"));
        }
        let flags = raw_flags.iter().map(|v| *v != 0).collect::<Vec<_>>();
        let scope = r.varuint(32)?;
        if scope == 0 {
            return Err(WireError::new("Invalid client scope"));
        }
        let count = r.count(1000)?;
        let mut ids = HashSet::with_capacity(count);
        let mut actions = Vec::with_capacity(count);
        // Parsing is all local until the trailing-byte and duplicate checks pass.
        for _ in 0..count {
            let key = Some(
                read_key(&mut r)?.ok_or_else(|| WireError::new("Null top-container reference"))?,
            );
            let class_id = usize::from(r.u16()?);
            let first = r.u8()?;
            let second = r.u8()?;
            let class = self
                .schema
                .classes
                .get(class_id)
                .ok_or_else(|| WireError::new("Invalid top container class"))?;
            if !ids.insert(key.clone()) || first > 1 || second > 1 {
                return Err(WireError::new("Invalid or duplicate top container"));
            }
            let id = key_to_string(&key);
            let node = json!({
                "id": id,
                "class_name": class.name,
                "parent": null,
                "properties": {},
                "top_container_flags": [first != 0, second != 0],
                "constructor_defaults_known": false
            });
            let seq = self
                .sequence
                .checked_add(actions.len() as u64 + 1)
                .ok_or_else(|| WireError::new("Replication sequence overflow"))?;
            actions.push(json!({"sequence": seq, "kind":"create", "id": id, "node": node}));
        }
        r.finish()?;
        let final_sequence = self
            .sequence
            .checked_add(actions.len() as u64)
            .ok_or_else(|| WireError::new("Replication sequence overflow"))?;
        Ok(Globals {
            scope,
            flags,
            world_delta: json!({"version":1,"delta":true,"sequence":final_sequence,"actions":actions}),
            base_sequence: self.sequence,
            next_sequence: final_sequence,
            base_generation: self.generation,
            next_region: self.last_region,
        })
    }

    /// Commit global scope only after `WorldStore::apply_delta` succeeds.
    pub fn commit_globals(&mut self, globals: &Globals) -> Result<(), WireError> {
        self.validate_globals(globals)?;
        let next_generation = self
            .generation
            .checked_add(1)
            .ok_or_else(|| WireError::new("Replication generation overflow"))?;
        self.sequence = globals.next_sequence;
        self.last_region = globals.next_region;
        self.client_scope = Some(globals.scope);
        self.global_flags = Some(globals.flags.clone());
        self.generation = next_generation;
        Ok(())
    }

    /// Decode one ordinary replication packet without committing any state.
    pub fn packet(&self, payload: &[u8]) -> Result<PacketTransaction, WireError> {
        Ok(self.packet_inner(payload, false)?.packet)
    }

    /// Session-only path: direct shared scalar properties stay as typed Arc
    /// bindings through atomic world application. Public packet() stays eager.
    pub(crate) fn packet_compact(
        &self,
        payload: &[u8],
    ) -> Result<CompactPacketTransaction, WireError> {
        self.packet_inner(payload, true)
    }

    fn packet_inner(
        &self,
        payload: &[u8],
        compact_direct_properties: bool,
    ) -> Result<CompactPacketTransaction, WireError> {
        if payload.len() > MAX_PACKET {
            return Err(WireError::new("Replication packet exceeds limit"));
        }
        let mut r = Reader::new(payload);
        if r.u8()? != 131 {
            return Err(WireError::new("Not a replication packet"));
        }
        let mut decoder = PacketParser {
            schema: &self.schema,
            known_shared: &self.shared,
            shared_additions: Vec::new(),
            newly_shared: HashMap::new(),
            actions: Vec::new(),
            events: Vec::new(),
            pings: Vec::new(),
            tags: Vec::new(),
            regions: Vec::new(),
            metadata: Vec::new(),
            property_changes: Vec::new(),
            pending_shared: HashSet::new(),
            next_sequence: self.sequence,
            last_region: self.last_region,
            preserve_remote_success_variants: self.preserve_remote_success_variants,
            shared_bytes: 0,
            compact_direct_properties,
            compact_property_keys: Vec::new(),
        };
        let mut terminated = false;
        let mut items = 0usize;
        while r.remaining() > 0 {
            items += 1;
            if items > 100_000 {
                return Err(WireError::new("Too many replication items"));
            }
            match r.u8()? {
                0 => {
                    terminated = true;
                    break;
                }
                1 => {
                    if let Some(key) = read_key(&mut r)? {
                        let id = key_to_string(&Some(key));
                        decoder.push_action("remove", &id, None)?;
                    }
                }
                2 => decoder.new_instance(&mut r, 2, true)?,
                3 => decoder.property_change(&mut r)?,
                7 => decoder.remote_event(&mut r)?,
                kind @ (5 | 6) => decoder.ping(&mut r, kind)?,
                kind @ (11 | 13) => decoder.stream_instances(&mut r, kind)?,
                17 => decoder.statistics(&mut r)?,
                16 => {
                    let tag = i32::from_le_bytes(
                        r.take(4)?
                            .try_into()
                            .map_err(|_| WireError::new("Invalid tag"))?,
                    );
                    decoder.tags.push(tag);
                    decoder.metadata.push(json!({"kind":"tag","value":tag}));
                }
                19 => {
                    let key = read_key(&mut r)?;
                    let parent = key_to_value(read_key(&mut r)?);
                    if let Some(key) = key {
                        let id = key_to_string(&Some(key));
                        decoder.push_action("set", &id, Some(json!({"Parent":parent})))?;
                    }
                }
                20 => decoder.region(&mut r)?,
                21 => decoder.attribute(&mut r)?,
                22 => decoder.cached_instances(&mut r)?,
                26 => decoder.focus(&mut r)?,
                27 => decoder.batched_remove(&mut r)?,
                24 => decoder.prefetch_info(&mut r)?,
                25 => decoder.prefetch_request(&mut r)?,
                kind @ (29 | 30) => {
                    let key = key_to_value(read_key(&mut r)?);
                    decoder.metadata.push(json!({"kind":if kind == 29 {"persistent_player_add"} else {"persistent_player_remove"},"key":key}));
                }
                35 => {
                    let value = i32::from_le_bytes(
                        r.take(4)?
                            .try_into()
                            .map_err(|_| WireError::new("Invalid server step"))?,
                    );
                    decoder
                        .metadata
                        .push(json!({"kind":"server_step","value":value}));
                }
                31 => decoder.qos(&mut r)?,
                32 => decoder.replicator_data(&mut r)?,
                kind @ (36 | 40) => {
                    let focus = r.u32()?;
                    decoder.metadata.push(json!({"kind":if kind == 36 {"remove_focus"} else {"remove_frustum"},"id":focus}));
                }
                39 => decoder.focus_frustum(&mut r)?,
                37 => decoder.stream_pin(&mut r)?,
                41 => decoder.instance_intents(&mut r)?,
                _ => return Err(WireError::new("Unsupported replication item")),
            }
        }
        if !terminated {
            return Err(WireError::new("Missing replication terminator"));
        }
        r.finish()?;
        if !decoder.pending_shared.is_empty() {
            return Err(WireError::new("Unresolved shared-string reference"));
        }
        let staged_shared_bytes = decoder.shared_bytes;
        if self
            .shared_bytes
            .checked_add(staged_shared_bytes)
            .filter(|n| *n <= MAX_SHARED_BYTES)
            .is_none()
        {
            return Err(WireError::new(
                "Session shared-string memory limit exceeded",
            ));
        }
        let actions = std::mem::take(&mut decoder.actions);
        let packet = PacketTransaction {
            world_delta: json!({"version":1,"delta":true,"sequence":decoder.next_sequence,"actions":actions}),
            events: decoder.events,
            pings: decoder.pings,
            tags: decoder.tags,
            regions: decoder.regions,
            metadata: decoder.metadata,
            shared_additions: decoder.shared_additions,
            property_changes: decoder.property_changes,
            next_sequence: decoder.next_sequence,
            base_sequence: self.sequence,
            base_generation: self.generation,
            next_region: decoder.last_region,
        };
        let shared_properties = decoder
            .compact_property_keys
            .into_iter()
            .map(|binding| {
                let payload = decoder
                    .newly_shared
                    .get(&binding.key)
                    .or_else(|| self.shared.get(&binding.key))
                    .cloned()
                    .ok_or_else(|| WireError::new("Unresolved compact shared property"))?;
                Ok(CompactSharedProperty {
                    action_index: binding.action_index,
                    property: binding.property,
                    payload,
                    force_bytes: binding.force_bytes,
                })
            })
            .collect::<Result<Vec<_>, WireError>>()?;
        Ok(CompactPacketTransaction {
            packet,
            shared_properties,
        })
    }

    /// Call only after the normalized world delta has been accepted.
    pub fn commit(&mut self, transaction: &PacketTransaction) -> Result<(), WireError> {
        self.validate_commit(transaction)?;
        let next_generation = self
            .generation
            .checked_add(1)
            .ok_or_else(|| WireError::new("Replication generation overflow"))?;
        for (key, value) in &transaction.shared_additions {
            if !self.shared.contains_key(key) {
                self.shared_bytes += value.len();
                self.shared_stored_bytes += value.stored_bytes();
                self.shared.insert(*key, value.clone());
            }
        }
        self.sequence = transaction.next_sequence;
        self.last_region = transaction.next_region;
        self.generation = next_generation;
        Ok(())
    }
}

struct PacketParser<'a> {
    schema: &'a Schema,
    known_shared: &'a HashMap<[u8; 16], Arc<SharedPayload>>,
    shared_additions: Vec<([u8; 16], Arc<SharedPayload>)>,
    newly_shared: HashMap<[u8; 16], Arc<SharedPayload>>,
    actions: Vec<Value>,
    events: Vec<Value>,
    pings: Vec<Value>,
    tags: Vec<i32>,
    regions: Vec<Value>,
    metadata: Vec<Value>,
    property_changes: Vec<Value>,
    pending_shared: HashSet<[u8; 16]>,
    next_sequence: u64,
    last_region: Option<[i32; 3]>,
    preserve_remote_success_variants: bool,
    shared_bytes: usize,
    compact_direct_properties: bool,
    compact_property_keys: Vec<CompactPropertyKey>,
}

struct CompactPropertyKey {
    action_index: usize,
    property: String,
    key: [u8; 16],
    force_bytes: bool,
}

impl PacketParser<'_> {
    fn push_action(
        &mut self,
        kind: &str,
        id: &str,
        fields: Option<Value>,
    ) -> Result<(), WireError> {
        self.next_sequence = self
            .next_sequence
            .checked_add(1)
            .ok_or_else(|| WireError::new("Replication sequence overflow"))?;
        let mut item = json!({"sequence":self.next_sequence,"kind":kind,"id":id});
        if let Some(fields) = fields {
            item["fields"] = fields;
        }
        self.actions.push(item);
        if self.actions.len() > MAX_PROPERTIES {
            return Err(WireError::new("Too many world actions"));
        }
        Ok(())
    }

    fn new_instance(
        &mut self,
        r: &mut Reader<'_>,
        lists: usize,
        inline_shared: bool,
    ) -> Result<(), WireError> {
        if !(1..=2).contains(&lists) {
            return Err(WireError::new("Invalid instance property list count"));
        }
        let key = read_key(r)?.ok_or_else(|| WireError::new("Null instance reference"))?;
        let class_id = usize::from(r.u16()?);
        let class = self
            .schema
            .classes
            .get(class_id)
            .ok_or_else(|| WireError::new("Invalid instance class"))?;
        let mut properties = Map::new();
        let mut pending_compact = HashMap::<String, ([u8; 16], bool)>::new();
        for list in 0..lists {
            let compressed = lists == 2 && list == 0;
            let mut previous_index: Option<u8> = None;
            loop {
                let index = r.u8()?;
                if index == 255 {
                    break;
                }
                if previous_index.is_some_and(|previous| index <= previous) {
                    return Err(WireError::new("Invalid property order/index"));
                }
                previous_index = Some(index);
                let property_id = *class
                    .properties
                    .get(usize::from(index))
                    .ok_or_else(|| WireError::new("Invalid property order/index"))?;
                let property = self
                    .schema
                    .properties
                    .get(property_id)
                    .ok_or_else(|| WireError::new("Invalid schema property index"))?;
                let (value, compact_key) =
                    self.direct_property_value(r, property.type_id, compressed, true)?;
                properties.insert(property.name.clone(), value);
                if let Some((key, force_bytes)) = compact_key {
                    pending_compact.insert(property.name.clone(), (key, force_bytes));
                } else {
                    pending_compact.remove(&property.name);
                }
            }
        }
        if inline_shared {
            self.resolve_shared(r)?;
            for value in properties.values_mut() {
                self.materialize_shared(value)?;
            }
        }
        let parent = key_to_value(read_key(r)?);
        let id = key_to_string(&Some(key));
        let node = json!({"id":id,"class_name":class.name,
            "parent":parent,"properties":Value::Object(properties),"origins":{},
            "received_creation_complete":true,
            // A decoded creation record is complete (decoding is
            // transactional): properties it omits are the class's constructor
            // defaults (BoolValue.Value false, ...). The world store clears this
            // when the instance already existed.
            "constructor_defaults_known":true});
        self.next_sequence = self
            .next_sequence
            .checked_add(1)
            .ok_or_else(|| WireError::new("Replication sequence overflow"))?;
        let action_index = self.actions.len();
        self.actions
            .push(json!({"sequence":self.next_sequence,"kind":"create","id":id,"node":node}));
        for (property, (key, force_bytes)) in pending_compact {
            self.compact_property_keys.push(CompactPropertyKey {
                action_index,
                property,
                key,
                force_bytes,
            });
        }
        if self.actions.len() > MAX_PROPERTIES {
            return Err(WireError::new("Too many world actions"));
        }
        Ok(())
    }

    fn direct_property_value(
        &mut self,
        r: &mut Reader<'_>,
        type_id: u8,
        compressed: bool,
        allow_compact: bool,
    ) -> Result<(Value, Option<([u8; 16], bool)>), WireError> {
        if self.compact_direct_properties && allow_compact && matches!(type_id, 46 | 47) {
            let key: [u8; 16] = r
                .take(16)?
                .try_into()
                .map_err(|_| WireError::new("Invalid shared string key"))?;
            let force_bytes =
                !self.known_shared.contains_key(&key) && !self.newly_shared.contains_key(&key);
            if force_bytes {
                self.pending_shared.insert(key);
            }
            return Ok((Value::Null, Some((key, force_bytes))));
        }
        Ok((self.value(r, type_id, compressed, 0, false)?, None))
    }

    fn stream_instances(&mut self, r: &mut Reader<'_>, kind: u8) -> Result<(), WireError> {
        let count = r.u32()?;
        if count > 100_000 {
            return Err(WireError::new("Too many streamed instances"));
        }
        if count != 0 {
            let bytes = decompress_stream(r, MAX_PACKET)?;
            let mut content = Reader::new(&bytes);
            for _ in 0..count {
                self.new_instance(&mut content, 1, false)?;
            }
            while !self.pending_shared.is_empty() {
                self.shared_entry(&mut content)?;
            }
            content.finish()?;
            // Parsing is complete before shared values are expanded.
            drop(content);
            drop(bytes);
            let mut actions = std::mem::take(&mut self.actions);
            for action in &mut actions {
                self.materialize_shared(action)?;
            }
            self.actions = actions;
        }
        if kind == 13 {
            let mut terminated = false;
            for _ in 0..100_000 {
                let flags = r.u8()?;
                let mut region = json!({"flags":flags});
                let coordinate = match flags & !8 {
                    0 => {
                        let wide = r.u8()?;
                        if wide > 1 {
                            return Err(WireError::new("Invalid region coordinate width"));
                        }
                        let mut xyz = [0i32; 3];
                        for item in &mut xyz {
                            *item = if wide == 1 {
                                r.i32()?
                            } else {
                                i32::from(r.i8()?)
                            };
                        }
                        Some(xyz)
                    }
                    axis_code @ (1..=3) => {
                        let mut xyz = self.last_region.ok_or_else(|| {
                            WireError::new("Relative region before absolute region")
                        })?;
                        let axis = match axis_code {
                            1 => 1,
                            2 => 2,
                            _ => 0,
                        };
                        xyz[axis] = xyz[axis]
                            .checked_add(1)
                            .ok_or_else(|| WireError::new("Relative region coordinate overflow"))?;
                        Some(xyz)
                    }
                    4 => None,
                    _ => return Err(WireError::new("Unsupported streamed region flag")),
                };
                if let Some(xyz) = coordinate {
                    self.last_region = Some(xyz);
                    region["coordinate"] = json!(xyz);
                }
                self.regions.push(region);
                if flags & 8 == 0 {
                    terminated = true;
                    break;
                }
            }
            if !terminated {
                return Err(WireError::new("Too many streamed regions"));
            }
        }
        Ok(())
    }

    fn cached_instances(&mut self, r: &mut Reader<'_>) -> Result<(), WireError> {
        let subtype = r.u8()?;
        let bytes = decompress_stream(r, MAX_PACKET)?;
        let mut content = Reader::new(&bytes);
        match subtype {
            1 => {
                let mut count = 0usize;
                while content.remaining() > 0 {
                    self.shared_entry(&mut content)?;
                    count += 1;
                    if count > 100_000 {
                        return Err(WireError::new("Too many cached shared strings"));
                    }
                }
            }
            2..=4 => {
                let count = r.u32()?;
                if count > 100_000 {
                    return Err(WireError::new("Too many cached instances"));
                }
                let offset_count = r.count(100_000)?;
                let mut offsets = HashMap::with_capacity(offset_count);
                for _ in 0..offset_count {
                    let offset = r.varuint(64)?;
                    let size = r.varuint(64)?;
                    if offsets.insert(offset, size).is_some() {
                        return Err(WireError::new("Duplicate cached instance offset"));
                    }
                }
                for _ in 0..count {
                    while let Some(size) = offsets.remove(&(content.position() as u64)) {
                        let skip = usize::try_from(size)
                            .map_err(|_| WireError::new("Cached skip exceeds limit"))?;
                        if skip > content.remaining() {
                            return Err(WireError::new("Cached skip exceeds content"));
                        }
                        content.take(skip)?;
                    }
                    self.new_instance(&mut content, 1, false)?;
                    if !self.pending_shared.is_empty() {
                        return Err(WireError::new(
                            "Cached instance references unknown shared string",
                        ));
                    }
                }
                while let Some(size) = offsets.remove(&(content.position() as u64)) {
                    let skip = usize::try_from(size)
                        .map_err(|_| WireError::new("Cached skip exceeds limit"))?;
                    if skip > content.remaining() {
                        return Err(WireError::new("Cached skip exceeds content"));
                    }
                    content.take(skip)?;
                }
                if !offsets.is_empty() {
                    return Err(WireError::new("Cached instance offset not reached"));
                }
                content.finish()?;
            }
            _ => return Err(WireError::new("Unsupported cached replication subtype")),
        }
        Ok(())
    }

    fn attribute(&mut self, r: &mut Reader<'_>) -> Result<(), WireError> {
        let key = key_to_string(&read_key(r)?);
        let name_size = r.count(100)?;
        let raw_name = r.take(name_size)?;
        let name = std::str::from_utf8(raw_name)
            .map_err(|_| WireError::new("Invalid received attribute name"))?;
        if name.is_empty() || name.as_bytes().contains(&0) {
            return Err(WireError::new("Invalid received attribute name"));
        }
        let data_size = r.count(1024 * 1024)?;
        let data = r.take(data_size)?;
        let update = match decode_received_attribute(data) {
            Ok(value) => json!({"name":name,"known":true,"value":value}),
            Err(_) => json!({"name":name,"known":false}),
        };
        self.next_sequence = self
            .next_sequence
            .checked_add(1)
            .ok_or_else(|| WireError::new("Replication sequence overflow"))?;
        self.actions.push(json!({"sequence":self.next_sequence,"kind":"attribute","id":key,"attribute_update":update}));
        if self.actions.len() > MAX_PROPERTIES {
            return Err(WireError::new("Too many world actions"));
        }
        Ok(())
    }

    fn focus(&mut self, r: &mut Reader<'_>) -> Result<(), WireError> {
        let id = r.u32()?;
        let floating = r.u8()?;
        if floating > 1 {
            return Err(WireError::new(
                "Invalid replication focus coordinate encoding",
            ));
        }
        let mut center = Vec::with_capacity(3);
        for _ in 0..3 {
            center.push(if floating != 0 {
                finite_f32(r.f32()?)?
            } else {
                json!(r.i8()?)
            });
        }
        let radius = finite_f32(r.f32()?)?;
        let optional = finite_f32(r.f32()?)?;
        let first = r.u8()?;
        let second = r.u8()?;
        if first > 1 || second > 1 {
            return Err(WireError::new("Invalid replication focus boolean"));
        }
        let timeout = r.u32()?;
        self.metadata.push(
            json!({"kind":"replication_focus","id":id,"center":center,"radius":radius,
            "optional_float":if optional.as_f64()==Some(-1.0) {Value::Null} else {optional},
            "flags":[first != 0,second != 0],"timeout":timeout}),
        );
        Ok(())
    }

    fn batched_remove(&mut self, r: &mut Reader<'_>) -> Result<(), WireError> {
        let count = r.u32()?;
        if count > 100_000 {
            return Err(WireError::new("Too many batched instance references"));
        }
        let mut keys = Vec::with_capacity(count as usize);
        for _ in 0..count {
            let key = key_to_value(read_key(r)?);
            if let Some(id) = key.get("$ref").and_then(Value::as_str) {
                self.push_action("remove", id, None)?;
            }
            keys.push(key);
        }
        self.metadata.push(json!({"kind":"guid_batch","keys":keys}));
        Ok(())
    }

    fn qos(&mut self, r: &mut Reader<'_>) -> Result<(), WireError> {
        let kind = r.u8()?;
        let value = r.u8()?;
        let text = if kind == 0 || kind == 3 {
            Value::Null
        } else {
            let length = usize::try_from(r.u32()?)
                .map_err(|_| WireError::new("QoS text length overflow"))?;
            if length > MAX_PACKET {
                return Err(WireError::new("QoS text exceeds limit"));
            }
            bytes_value(r.take(length)?)
        };
        self.metadata
            .push(json!({"kind":"qos_change","type":kind,"value":value,"text":text}));
        Ok(())
    }

    fn replicator_data(&mut self, r: &mut Reader<'_>) -> Result<(), WireError> {
        let id = r.u32()?;
        let length = r.u32()? as usize;
        if length > MAX_PACKET {
            return Err(WireError::new("Replicator data exceeds limit"));
        }
        self.metadata
            .push(json!({"kind":"replicator_data","id":id,"data":bytes_value(r.take(length)?)}));
        Ok(())
    }

    fn stream_pin(&mut self, r: &mut Reader<'_>) -> Result<(), WireError> {
        let count = r.u32()?;
        if count > 100_000 {
            return Err(WireError::new("Too many pinned stream instances"));
        }
        if count > 0 {
            let bytes = decompress_stream(r, MAX_PACKET)?;
            let mut content = Reader::new(&bytes);
            for _ in 0..count {
                self.new_instance(&mut content, 1, false)?;
            }
            while !self.pending_shared.is_empty() {
                self.shared_entry(&mut content)?;
            }
            content.finish()?;
            // Parsing is complete before shared values are expanded.
            drop(content);
            drop(bytes);
            let mut actions = std::mem::take(&mut self.actions);
            for action in &mut actions {
                self.materialize_shared(action)?;
            }
            self.actions = actions;
        }
        let mut points = Vec::new();
        let mut terminated = false;
        for _ in 0..100_000 {
            let flags = r.u8()?;
            if flags & !8 == 0 {
                let floating = r.u8()?;
                if floating > 1 {
                    return Err(WireError::new("Invalid stream pin coordinate encoding"));
                }
                let mut xyz = Vec::with_capacity(3);
                for _ in 0..3 {
                    xyz.push(if floating != 0 {
                        finite_f32(r.f32()?)?
                    } else {
                        json!(r.i8()?)
                    });
                }
                points.push(Value::Array(xyz));
            } else {
                points.push(Value::Null);
            }
            if flags & 8 == 0 {
                terminated = true;
                break;
            }
        }
        if !terminated {
            return Err(WireError::new("Too many stream pin points"));
        }
        let key = key_to_value(read_key(r)?);
        let value = finite_f32(r.f32()?)?;
        let pinned = r.u8()?;
        if pinned > 1 {
            return Err(WireError::new("Invalid stream pin boolean"));
        }
        self.metadata.push(json!({"kind":"stream_pin","points":points,"key":key,"value":value,"pinned":pinned != 0}));
        Ok(())
    }

    fn instance_intents(&mut self, r: &mut Reader<'_>) -> Result<(), WireError> {
        let count = r.u32()?;
        if count > 100_000 {
            return Err(WireError::new("Too many instance intents"));
        }
        let mut intents = Vec::with_capacity(count as usize);
        for _ in 0..count {
            intents.push(json!({"key":key_to_value(read_key(r)?),"intent":r.u8()?}));
        }
        self.metadata
            .push(json!({"kind":"instance_intents","items":intents}));
        Ok(())
    }

    fn statistics(&mut self, r: &mut Reader<'_>) -> Result<(), WireError> {
        let version = r.u32()?;
        let mut initial_values = Vec::new();
        if version >= 9 {
            initial_values.extend([finite_f32(r.f32()?)?, finite_f32(r.f32()?)?]);
        }
        let mut version8_text = Value::Null;
        if version >= 8 && read_bool(r)? {
            let bytes = decompress_stream(r, MAX_PACKET)?;
            let mut inner = Reader::new(&bytes);
            version8_text = stats_string(&mut inner)?;
            inner.finish()?;
        }
        let mut version7_text = Value::Null;
        if version >= 7 {
            let expected = r.u32()? as usize;
            let bytes = decompress_stream(r, MAX_PACKET)?;
            let mut inner = Reader::new(&bytes);
            version7_text = stats_string(&mut inner)?;
            inner.finish()?;
            if version7_text["$bytes_hex"]
                .as_str()
                .map_or(0, |v| v.len() / 2)
                != expected
            {
                return Err(WireError::new("Stats text length mismatch"));
            }
        }
        let mut memory = Value::Null;
        if version >= 6 {
            let total = finite_f64(r.f64()?)?;
            let mut categories = Vec::with_capacity(2);
            for _ in 0..2 {
                let count = r.u32()? as usize;
                if count > 100_000 {
                    return Err(WireError::new("Too many memory stats entries"));
                }
                let mut entries = Vec::with_capacity(count);
                for _ in 0..count {
                    entries.push(json!({"name":stats_string(r)?,"memory":finite_f64(r.f64()?)?}));
                }
                categories.push(Value::Array(entries));
            }
            memory = json!({"total":total,"categories":categories});
        }
        let mut datastore = Value::Null;
        if version >= 3 {
            let enabled = read_bool(r)?;
            let mut counters = Vec::new();
            if enabled {
                for _ in 0..6 {
                    counters.push(r.u32()?);
                }
            }
            datastore = json!({"enabled":enabled,"counters":counters});
        }
        let mut jobs = Vec::new();
        let mut jobs_done = false;
        for _ in 0..100_000 {
            if read_bool(r)? {
                jobs_done = true;
                break;
            }
            jobs.push(json!({"name":stats_string(r)?,"values":[finite_f32(r.f32()?)?,finite_f32(r.f32()?)?,finite_f32(r.f32()?)?]}));
        }
        if !jobs_done {
            return Err(WireError::new("Too many job stats entries"));
        }
        let mut scripts = Vec::new();
        let mut scripts_done = false;
        for _ in 0..100_000 {
            if read_bool(r)? {
                scripts_done = true;
                break;
            }
            scripts.push(
                json!({"name":stats_string(r)?,"value":finite_f32(r.f32()?)?,"count":r.u32()?}),
            );
        }
        if !scripts_done {
            return Err(WireError::new("Too many script stats entries"));
        }
        let mut final_values = Vec::with_capacity(5);
        for _ in 0..5 {
            final_values.push(finite_f32(r.f32()?)?);
        }
        self.metadata.push(json!({"kind":"statistics","version":version,"initial_values":initial_values,
            "version8_text":version8_text,"version7_text":version7_text,"memory":memory,"datastore":datastore,
            "jobs":jobs,"scripts":scripts,"final_values":final_values}));
        Ok(())
    }

    fn property_change(&mut self, r: &mut Reader<'_>) -> Result<(), WireError> {
        let id = key_to_string(&Some(
            read_key(r)?.ok_or_else(|| WireError::new("Null property instance reference"))?,
        ));
        let property_index = usize::from(r.u16()?);
        let reset = r.u8()?;
        if reset > 1 || property_index > self.schema.parent_property_id {
            return Err(WireError::new("Invalid property change header"));
        }
        let mut compact_property = None;
        let fields = if property_index == self.schema.parent_property_id {
            json!({"Parent":key_to_value(read_key(r)?)})
        } else {
            let property = self
                .schema
                .properties
                .get(property_index)
                .ok_or_else(|| WireError::new("Invalid property change index"))?;
            let (value, compact_key) = self.direct_property_value(
                r,
                property.type_id,
                true,
                !matches!(property.name.as_str(), "Name" | "Parent"),
            )?;
            if let Some((key, force_bytes)) = compact_key {
                compact_property = Some((property.name.clone(), key, force_bytes));
            }
            json!({(property.name.clone()):value})
        };
        self.resolve_shared(r)?;
        let mut fields = fields;
        self.materialize_shared(&mut fields)?;
        self.property_changes
            .push(json!({"id":id,"property_index":property_index,"reset_version":reset!=0}));
        let action_index = self.actions.len();
        self.push_action("set", &id, Some(fields))?;
        if let Some((property, key, force_bytes)) = compact_property {
            self.compact_property_keys.push(CompactPropertyKey {
                action_index,
                property,
                key,
                force_bytes,
            });
        }
        Ok(())
    }

    fn remote_event(&mut self, r: &mut Reader<'_>) -> Result<(), WireError> {
        let key = key_to_string(&Some(
            read_key(r)?.ok_or_else(|| WireError::new("Null remote event reference"))?,
        ));
        let event_id = usize::from(r.u16()?);
        let event = self
            .schema
            .events
            .get(event_id)
            .ok_or_else(|| WireError::new("Invalid event ID"))?;
        let mut arguments = Vec::with_capacity(event.arguments.len());
        let preserve_variants = matches!(
            event.name.as_str(),
            "RemoteOnInvokeClient" | "OnClientEvent"
        ) || (event.name == "RemoteOnInvokeSuccess"
            && self.preserve_remote_success_variants);
        for argument in &event.arguments {
            let value = self.value(r, argument.type_id, true, 0, preserve_variants)?;
            arguments.push(value);
        }
        self.resolve_shared(r)?;
        for value in &mut arguments {
            self.materialize_shared(value)?;
        }
        self.events
            .push(json!({"key":key,"event":event_id,"name":event.name,"arguments":arguments}));
        // Remote events are world receipts too. Give them a sequence and put
        // them into the same ordered action stream as creates, sets, and
        // removals so a runtime can replay wire order without guessing.
        self.next_sequence = self
            .next_sequence
            .checked_add(1)
            .ok_or_else(|| WireError::new("Replication sequence overflow"))?;
        let action = json!({"sequence":self.next_sequence,"kind":"event","id":key,
            "name":event.name,"arguments":arguments});
        self.actions.push(action);
        if self.actions.len() > MAX_PROPERTIES {
            return Err(WireError::new("Too many world actions"));
        }
        Ok(())
    }

    fn region(&mut self, r: &mut Reader<'_>) -> Result<(), WireError> {
        let wide = r.u8()?;
        if wide > 1 {
            return Err(WireError::new("Invalid region coordinate width"));
        }
        let mut coordinate = Vec::with_capacity(3);
        for _ in 0..3 {
            coordinate.push(if wide != 0 {
                i64::from(i32::from_le_bytes(
                    r.take(4)?
                        .try_into()
                        .map_err(|_| WireError::new("Invalid region coordinate"))?,
                ))
            } else {
                i64::from(r.u8()? as i8)
            });
        }
        let radius = i32::from_le_bytes(
            r.take(4)?
                .try_into()
                .map_err(|_| WireError::new("Invalid region radius"))?,
        );
        let version = r.u32()?;
        let region = json!({"coordinate":coordinate,"radius":radius,"version":version});
        self.regions.push(region.clone());
        Ok(())
    }

    fn prefetch_info(&mut self, r: &mut Reader<'_>) -> Result<(), WireError> {
        let floating = r.u8()?;
        if floating > 1 {
            return Err(WireError::new("Invalid prefetch coordinate encoding"));
        }
        let mut center = Vec::with_capacity(3);
        for _ in 0..3 {
            center.push(if floating != 0 {
                json!(finite_f32(r.f32()?)?)
            } else {
                json!(r.u8()? as i8)
            });
        }
        let values = [finite_f32(r.f32()?)?, finite_f32(r.f32()?)?];
        self.metadata
            .push(json!({"kind":"prefetch_info","center":center,"values":values}));
        Ok(())
    }

    fn prefetch_request(&mut self, r: &mut Reader<'_>) -> Result<(), WireError> {
        let center = [
            finite_f32(r.f32()?)?,
            finite_f32(r.f32()?)?,
            finite_f32(r.f32()?)?,
        ];
        let radius = finite_f64(r.f64()?)?;
        let timeout = finite_f32(r.f32()?)?;
        self.metadata.push(
            json!({"kind":"prefetch_request","center":center,"radius":radius,"timeout":timeout}),
        );
        Ok(())
    }

    fn focus_frustum(&mut self, r: &mut Reader<'_>) -> Result<(), WireError> {
        let id = r.u32()?;
        let mut values = Vec::with_capacity(15);
        for _ in 0..15 {
            values.push(finite_f32(r.f32()?)?);
        }
        self.metadata
            .push(json!({"kind":"focus_frustum","id":id,"values":values}));
        Ok(())
    }

    fn ping(&mut self, r: &mut Reader<'_>, kind: u8) -> Result<(), WireError> {
        let flags = r.u8()?;
        if flags & !5 != 0 {
            return Err(WireError::new("Unsupported replication ping flags"));
        }
        let timestamp = r.u64()?;
        let first_word = r.u32()?;
        let mut reply_words = Vec::new();
        if flags & 1 != 0 {
            for _ in 0..6 {
                reply_words.push(r.u32()?);
            }
        }
        let trailer_words = [r.u32()?, r.u32()?];
        let unreliable = r.u8()?;
        if unreliable > 1 {
            return Err(WireError::new("Invalid ping channel flag"));
        }
        let extra_word = if flags & 4 != 0 { Some(r.u32()?) } else { None };
        let mut ping = json!({"kind":kind,"flags":flags,"timestamp":timestamp,
            "first_word":first_word,"reply_words":reply_words,
            "trailer_words":trailer_words,"unreliable":unreliable!=0});
        if let Some(value) = extra_word {
            ping["extra_word"] = json!(value);
        }
        self.pings.push(ping);
        if self.pings.len() > 64 {
            return Err(WireError::new("Too many ping items"));
        }
        Ok(())
    }

    fn shared_entry(&mut self, r: &mut Reader<'_>) -> Result<[u8; 16], WireError> {
        let key: [u8; 16] = r
            .take(16)?
            .try_into()
            .map_err(|_| WireError::new("Invalid shared string key"))?;
        let marker = r.u8()?;
        if marker == 0 {
            let len = r.count(16 * 1024 * 1024)?;
            let raw = r.take(len)?;
            let digest = blake2b_128(raw);
            if digest != key {
                return Err(WireError::new("Shared string digest mismatch"));
            }
            if self
                .shared_bytes
                .checked_add(raw.len())
                .filter(|n| *n <= MAX_SHARED_BYTES)
                .is_none()
            {
                return Err(WireError::new("Packet shared-string memory limit exceeded"));
            }
            self.shared_bytes += raw.len();
            if let Some(old) = self
                .newly_shared
                .get(&key)
                .or_else(|| self.known_shared.get(&key))
            {
                if !old.matches_exact(raw) {
                    return Err(WireError::new("Shared-string key collision"));
                }
            } else {
                // Check digest + exact bytes before allocating or compressing;
                // duplicate committed/staged payloads reuse their existing Arc.
                let value = SharedPayload::from_raw(raw.to_vec());
                self.newly_shared.insert(key, Arc::clone(&value));
                self.shared_additions.push((key, value));
            }
        } else if marker != 1
            || (!self.known_shared.contains_key(&key) && !self.newly_shared.contains_key(&key))
        {
            return Err(WireError::new("Unknown shared string marker/reference"));
        }
        self.pending_shared.remove(&key);
        Ok(key)
    }

    fn resolve_shared(&mut self, r: &mut Reader<'_>) -> Result<(), WireError> {
        let mut count = 0;
        while !self.pending_shared.is_empty() {
            self.shared_entry(r)?;
            count += 1;
            if count > 100_000 {
                return Err(WireError::new("Too many shared strings"));
            }
        }
        Ok(())
    }

    fn materialize_shared(&self, value: &mut Value) -> Result<(), WireError> {
        match value {
            Value::Array(values) => {
                for value in values {
                    self.materialize_shared(value)?;
                }
            }
            Value::Object(object) => {
                if object.len() == 1 && object.contains_key("$shared_string") {
                    let text = object
                        .get("$shared_string")
                        .and_then(Value::as_str)
                        .ok_or_else(|| WireError::new("Invalid shared string marker"))?;
                    let raw = decode_hex_16(text)?;
                    let bytes = self
                        .newly_shared
                        .get(&raw)
                        .or_else(|| self.known_shared.get(&raw))
                        .ok_or_else(|| WireError::new("Unresolved shared-string reference"))?;
                    *value = bytes_value(bytes.raw());
                } else {
                    for child in object.values_mut() {
                        self.materialize_shared(child)?;
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn value(
        &mut self,
        r: &mut Reader<'_>,
        kind: u8,
        compressed: bool,
        depth: usize,
        preserve_variants: bool,
    ) -> Result<Value, WireError> {
        if depth > 16 {
            return Err(WireError::new("Value nesting exceeds limit"));
        }
        let v = match kind {
            0 | 3 => Value::Null,
            2 | 8 => {
                let len = r.count(16 * 1024 * 1024)?;
                bytes_or_text(r.take(len)?)
            }
            7 => json!(r.varuint(32)?),
            9 => {
                let b = r.u8()?;
                if b > 1 {
                    return Err(WireError::new("Invalid boolean"));
                }
                json!(b != 0)
            }
            10 => json!(r.signed(32)?),
            11 => finite_f32(r.f32()?)?,
            12 => finite_f64(r.f64()?)?,
            13 => {
                let scale = finite_f32(r.f32()?)?;
                let offset = i32::from_le_bytes(
                    r.take(4)?
                        .try_into()
                        .map_err(|_| WireError::new("Invalid UDim offset"))?,
                );
                json!({"$udim":[scale,offset]})
            }
            14 => {
                let sx = finite_f32(r.f32()?)?;
                let ox = i32::from_le_bytes(
                    r.take(4)?
                        .try_into()
                        .map_err(|_| WireError::new("Invalid UDim offset"))?,
                );
                let sy = finite_f32(r.f32()?)?;
                let oy = i32::from_le_bytes(
                    r.take(4)?
                        .try_into()
                        .map_err(|_| WireError::new("Invalid UDim offset"))?,
                );
                json!({"$udim2":[{"$udim":[sx,ox]},{"$udim":[sy,oy]}]})
            }
            18 => json!({"$brickcolor":r.u16()?}),
            19 | 22 | 36 => {
                let items = [
                    finite_f32(r.f32()?)?,
                    finite_f32(r.f32()?)?,
                    finite_f32(r.f32()?)?,
                ];
                if kind == 19 {
                    json!({"$color3":items})
                } else if kind == 22 {
                    json!({"$vector3":items})
                } else {
                    json!(items)
                }
            }
            20 => {
                let rgb = r.take(3)?;
                json!({"$color3":[f64::from(rgb[0])/255.0,f64::from(rgb[1])/255.0,f64::from(rgb[2])/255.0]})
            }
            21 => json!({"$vector2":[finite_f32(r.f32()?)?,finite_f32(r.f32()?)?]}),
            27 => {
                let p = [
                    finite_f32(r.f32()?)?,
                    finite_f32(r.f32()?)?,
                    finite_f32(r.f32()?)?,
                ];
                let rotation = r.u8()?;
                let packed = if rotation == 0 {
                    hex(&r.take(6)?)
                } else {
                    String::new()
                };
                json!({"position":p,"rotation_id":rotation,"packed_rotation":if rotation==0 {Some(packed)} else {None}})
            }
            51 => {
                let flag = r.u8()?;
                if flag & 128 == 0 {
                    Value::Null
                } else {
                    let rotation = if flag == 128 {
                        json!(hex(&r.take(6)?))
                    } else {
                        json!(flag & 127)
                    };
                    json!({"rotation":rotation,"position":[finite_f32(r.f32()?)?,finite_f32(r.f32()?)?,finite_f32(r.f32()?)?]})
                }
            }
            54 => {
                let id = r.u16()?;
                let kind = r.u8()?;
                let first = r.u32()?;
                if first > MAX_PACKET as u32 {
                    return Err(WireError::new("Replicated data field exceeds limit"));
                }
                let a = r.take(first as usize)?;
                let second = r.u32()?;
                if second > MAX_PACKET as u32 {
                    return Err(WireError::new("Replicated data field exceeds limit"));
                }
                let b = r.take(second as usize)?;
                json!([id, kind, bytes_or_text(a), bytes_or_text(b)])
            }
            28 | 69 => key_to_value(read_key(r)?),
            33 => bytes_or_text(read_content_id(r, self.schema, compressed)?),
            34 => json!(r.varuint(32)?),
            61 => match r.varuint(32)? {
                0 => Value::Null,
                1 => {
                    let bytes = read_content_id(r, self.schema, compressed)?;
                    bytes_or_text(bytes)
                }
                _ => return Err(WireError::new("Unsupported content marker")),
            },
            37 => json!({"$number_range":[finite_f32(r.f32()?)?,finite_f32(r.f32()?)?]}),
            40 => {
                json!({"$rect":[[finite_f32(r.f32()?)?,finite_f32(r.f32()?)?],[finite_f32(r.f32()?)?,finite_f32(r.f32()?)?]]})
            }
            35 | 38 => {
                let count =
                    usize::try_from(r.u32()?).map_err(|_| WireError::new("Sequence too long"))?;
                if count > 20 {
                    return Err(WireError::new("Sequence too long"));
                }
                let width = if kind == 35 { 3 } else { 5 };
                let mut rows = Vec::with_capacity(count);
                for _ in 0..count {
                    let mut row = Vec::with_capacity(width);
                    for _ in 0..width {
                        row.push(finite_f32(r.f32()?)?);
                    }
                    rows.push(Value::Array(row));
                }
                if kind == 35 {
                    let points = rows
                        .into_iter()
                        .map(|v| json!({"$number_keypoint":[v[0],v[1],v[2]]}))
                        .collect::<Vec<_>>();
                    json!({"$number_sequence":points})
                } else {
                    let mut points = Vec::with_capacity(rows.len());
                    for v in rows {
                        if v[4].as_f64() != Some(0.0) {
                            return Err(WireError::new("Unsupported color sequence envelope"));
                        }
                        points.push(json!({"$color_keypoint":[v[0],{"$color3":[v[1],v[2],v[3]]}]}));
                    }
                    json!({"$color_sequence":points})
                }
            }
            41 => {
                let flag = r.u8()?;
                if flag & 1 == 0 {
                    Value::Null
                } else {
                    let width = if flag >= 2 { 6 } else { 5 };
                    let mut values = Vec::with_capacity(width);
                    for _ in 0..width {
                        values.push(finite_f32(r.f32()?)?);
                    }
                    Value::Array(values)
                }
            }
            44 => json!(r.signed(64)?),
            46 | 47 => {
                let key: [u8; 16] = r
                    .take(16)?
                    .try_into()
                    .map_err(|_| WireError::new("Invalid shared string key"))?;
                if let Some(value) = self
                    .newly_shared
                    .get(&key)
                    .or_else(|| self.known_shared.get(&key))
                {
                    bytes_or_text(value.raw())
                } else {
                    self.pending_shared.insert(key);
                    json!({"$shared_string":hex(&key)})
                }
            }
            48 => {
                let n = i64::from_le_bytes(
                    r.take(8)?
                        .try_into()
                        .map_err(|_| WireError::new("Invalid DateTime timestamp"))?,
                );
                if !(-17_987_443_200_000..=253_402_300_799_999).contains(&n) {
                    return Err(WireError::new("DateTime timestamp outside supported range"));
                }
                json!({"$datetime":n})
            }
            49 => {
                let marker = r.u8()?;
                let bytes = if marker & 128 != 0 {
                    let mut i = usize::from(marker & 127);
                    if i == 127 {
                        i = i
                            .checked_add(r.varuint(32)? as usize)
                            .ok_or_else(|| WireError::new("Name index overflow"))?;
                    }
                    self.schema
                        .names
                        .get(i)
                        .ok_or_else(|| WireError::new("Invalid string name index"))?
                        .as_bytes()
                        .to_vec()
                } else {
                    let size = if marker == 127 {
                        r.count(1024 * 1024)?
                    } else {
                        usize::from(marker)
                    };
                    r.take(size)?.to_vec()
                };
                bytes_or_text(bytes)
            }
            52 | 55 => json!(r.u64()?),
            56 => {
                let encoded = r.count(1024 * 1024)?;
                let expanded = r.count(1024 * 1024)?;
                let data = if encoded == 0 {
                    r.take(expanded)?.to_vec()
                } else {
                    decompress_bytes(r.take(encoded)?, expanded, 1024 * 1024)?
                };
                json!({"$buffer_hex":hex(&data)})
            }
            60 => {
                json!({"lr_reference":{"identity":r.u64()?,"index":r.varuint(32)?,"type_word":r.u64()?}})
            }
            57 => {
                let flags = r.u8()?;
                let pos = if flags & 128 != 0 {
                    [json!(0.0), json!(0.0), json!(0.0)]
                } else {
                    compact_position(r)?
                };
                let rot = flags & 31;
                let rotation = if rot == 31 {
                    let mut x = Vec::with_capacity(7);
                    for _ in 0..7 {
                        x.push(finite_f32(r.f32()?)?);
                    }
                    json!(x)
                } else if rot == 0 {
                    json!(hex(&r.take(6)?))
                } else {
                    json!(rot)
                };
                let mut velocities = Vec::with_capacity(2);
                for mask in [64u8, 32u8] {
                    if flags & mask != 0 {
                        velocities.push(Value::Null)
                    } else {
                        let b = r.u8()?;
                        velocities.push(json!([b, if b == 0 { 0 } else { r.u32()? }]));
                    }
                }
                json!({"position":pos,"rotation":rotation,"packed_velocities":velocities,"position_present":flags&128==0})
            }
            29 | 30 => {
                let count = r.count(10_000)?;
                let mut values = Vec::with_capacity(count);
                for _ in 0..count {
                    values.push(self.variant(r, depth + 1, preserve_variants)?);
                }
                Value::Array(values)
            }
            31 | 32 => {
                let count = r.count(10_000)?;
                let mut values = Vec::with_capacity(count);
                for _ in 0..count {
                    let size = r.count(16 * 1024 * 1024)?;
                    let key = r.take(size)?;
                    values.push(json!([
                        bytes_or_text(key),
                        self.variant(r, depth + 1, preserve_variants)?
                    ]));
                }
                Value::Array(values)
            }
            65 => {
                let playing = r.u8()? != 0;
                let playback_speed = finite_f32(r.f32()?)?;
                let ref_step = finite_f64(f64::from_le_bytes(
                    r.take(8)?
                        .try_into()
                        .map_err(|_| WireError::new("Invalid animation reference step"))?,
                ))?;
                json!({"anim_track_play_state":{"playing":playing,"playback_speed":playback_speed,"ref_step_id_for_time_position":ref_step}})
            }
            66 => {
                let looped = r.u8()? != 0;
                let priority = i16::from_le_bytes(
                    r.take(2)?
                        .try_into()
                        .map_err(|_| WireError::new("Invalid animation priority"))?,
                );
                json!({"anim_track_metadata":{"looped":looped,"priority":priority}})
            }
            67 => {
                let weight = finite_f32(r.f32()?)?;
                let fade = finite_f32(r.f32()?)?;
                let step = finite_f64(f64::from_le_bytes(
                    r.take(8)?
                        .try_into()
                        .map_err(|_| WireError::new("Invalid animation reference step"))?,
                ))?;
                json!({"anim_track_weight":{"weight_target":weight,"weighted_fade_time":fade,"ref_step_id_for_weight":step}})
            }
            68 => json!([r.signed(64)?, r.varuint(32)?, r.signed(64)?, r.signed(64)?]),
            _ => return Err(WireError::new("Unsupported property type")),
        };
        Ok(v)
    }

    fn variant(
        &mut self,
        r: &mut Reader<'_>,
        depth: usize,
        preserve_variants: bool,
    ) -> Result<Value, WireError> {
        if depth > 16 {
            return Err(WireError::new("Value nesting exceeds limit"));
        }
        let kind = r.u8()?;
        if kind == 7 {
            let _enum_id = r.u16()?;
        }
        let value = self.value(r, kind, false, depth, preserve_variants)?;
        if preserve_variants {
            Ok(json!({"$remote_variant":{"type":kind,"value":value}}))
        } else {
            Ok(value)
        }
    }
}

fn read_key(r: &mut Reader<'_>) -> Result<Option<(u64, u32)>, WireError> {
    let scope = r.varuint(32)?;
    if scope == 0 {
        Ok(None)
    } else {
        Ok(Some((
            scope,
            u32::from_le_bytes(
                r.take(4)?
                    .try_into()
                    .map_err(|_| WireError::new("Invalid key"))?,
            ),
        )))
    }
}
fn compact_position(r: &mut Reader<'_>) -> Result<[Value; 3], WireError> {
    let b = r.u8()?;
    let sx = b & 4 != 0;
    let sy = b & 2 != 0;
    let sz = b & 1 != 0;
    let xyz: [i32; 3] = if b <= 0x27 {
        let n = r.u32()?;
        let x = n.wrapping_mul(2) | ((sx as u32) << 31);
        let y = (n.wrapping_shl(11) & 0x7fe0_0000) | ((sy as u32) << 31);
        let z = ((sz as u32) << 31) | ((n & 1023) << 21);
        [(x as i32) >> 21, (y as i32) >> 21, (z as i32) >> 21]
    } else if b <= 0x57 {
        let mut xyz = [0i32; 3];
        for (slot, sign) in xyz.iter_mut().zip([sx, sy, sz]) {
            *slot = ((((sign as u32) << 31) | (u32::from(r.u16()?) << 15)) as i32) >> 15;
        }
        xyz
    } else {
        let n = r.u32()?;
        let m = r.u32()?;
        let raw_x = (((u64::from(b >> 2)) << 32) | u64::from(n)) >> 1;
        let raw_y = ((sy as u32) << 31) | ((((n & 2047) << 10) | (m >> 21)) << 10);
        let raw_z = ((sz as u32) << 31) | ((m & 0x1f_ffff) << 10);
        [
            raw_x as u32 as i32 >> 10,
            (raw_y as i32) >> 10,
            (raw_z as i32) >> 10,
        ]
    };
    let divisor = if b <= 0x27 {
        1023.0
    } else if b <= 0x57 {
        65535.0
    } else {
        2097151.0
    };
    let scale = 2f64.powi(i32::from(b >> 3));
    Ok(xyz.map(|v| json!((f64::from(v) / divisor) * scale)))
}
fn key_to_string(key: &Option<(u64, u32)>) -> String {
    key.map_or_else(|| String::new(), |(s, i)| format!("{s}:{i}"))
}
fn read_content_id(
    r: &mut Reader<'_>,
    schema: &Schema,
    compressed: bool,
) -> Result<Vec<u8>, WireError> {
    if !compressed {
        let len = r.count(16 * 1024 * 1024)?;
        return Ok(r.take(len)?.to_vec());
    }
    let marker = r.u8()?;
    let index = usize::from(marker >> 1);
    let prefix = schema
        .strings
        .get(index)
        .ok_or_else(|| WireError::new("Invalid content prefix index"))?
        .as_bytes();
    let mut suffix = if marker & 1 != 0 {
        r.signed(64)?.to_string().into_bytes()
    } else {
        let len = r.count(16 * 1024 * 1024)?;
        r.take(len)?.to_vec()
    };
    let mut result = prefix.to_vec();
    result.append(&mut suffix);
    Ok(result)
}
fn read_bool(r: &mut Reader<'_>) -> Result<bool, WireError> {
    match r.u8()? {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(WireError::new("Invalid boolean")),
    }
}
fn stats_string(r: &mut Reader<'_>) -> Result<Value, WireError> {
    let size = r.u32()? as usize;
    if size > MAX_PACKET {
        return Err(WireError::new("Stats string exceeds limit"));
    }
    Ok(bytes_value(r.take(size)?))
}
fn decode_received_attribute(data: &[u8]) -> Result<Value, WireError> {
    if data.len() > 1024 * 1024 {
        return Err(WireError::new("Invalid attribute value blob"));
    }
    if data == [1] {
        return Ok(Value::Null);
    }
    let mut r = Reader::new(data);
    let kind = r.u8()?;
    let result = match kind {
        2 => {
            let size = r.count(65_536)?;
            let bytes = r.take(size)?;
            match std::str::from_utf8(bytes) {
                Ok(text) => json!(text),
                Err(_) => bytes_value(bytes),
            }
        }
        3 => json!(read_bool(&mut r)?),
        4 => json!(r.i32()?),
        5 => {
            let n = r.f32()?;
            if !n.is_finite() {
                return Err(WireError::new("Nonfinite attribute value"));
            }
            json!(n)
        }
        6 => {
            let n = r.f64()?;
            if !n.is_finite() {
                return Err(WireError::new("Nonfinite attribute value"));
            }
            json!(n)
        }
        9 => {
            let scale = r.f32()?;
            let offset = r.i32()?;
            if !scale.is_finite() {
                return Err(WireError::new("Nonfinite attribute value"));
            }
            json!({"$udim":[scale,offset]})
        }
        10 => {
            let mut values = Vec::with_capacity(2);
            for _ in 0..2 {
                let scale = r.f32()?;
                let offset = r.i32()?;
                if !scale.is_finite() {
                    return Err(WireError::new("Nonfinite attribute value"));
                }
                values.push(json!({"$udim":[scale,offset]}));
            }
            json!({"$udim2":values})
        }
        14 => json!({"$brickcolor":r.u32()?}),
        15 | 16 | 17 => {
            let count = if kind == 16 { 2 } else { 3 };
            let mut values = Vec::with_capacity(count);
            for _ in 0..count {
                let n = r.f32()?;
                if !n.is_finite() {
                    return Err(WireError::new("Nonfinite attribute value"));
                }
                values.push(n);
            }
            let tag = match kind {
                15 => "$color3",
                16 => "$vector2",
                _ => "$vector3",
            };
            json!({tag:values})
        }
        23 | 25 => {
            let count = r.u32()? as usize;
            if !(2..=20).contains(&count) {
                return Err(WireError::new("Invalid attribute sequence count"));
            }
            if count < 2 {
                return Err(WireError::new("Invalid attribute sequence count"));
            }
            let mut points = Vec::with_capacity(count);
            for _ in 0..count {
                let envelope = r.f32()?;
                let time = r.f32()?;
                if !envelope.is_finite() || !time.is_finite() {
                    return Err(WireError::new("Nonfinite attribute sequence"));
                }
                if kind == 23 {
                    let value = r.f32()?;
                    if !value.is_finite() {
                        return Err(WireError::new("Nonfinite attribute sequence"));
                    }
                    points.push(json!({"$number_keypoint":[time,value,envelope]}));
                } else {
                    if envelope != 0.0 {
                        return Err(WireError::new("Unsupported color sequence envelope"));
                    }
                    let mut rgb = Vec::with_capacity(3);
                    for _ in 0..3 {
                        let n = r.f32()?;
                        if !n.is_finite() {
                            return Err(WireError::new("Nonfinite attribute sequence"));
                        }
                        rgb.push(n);
                    }
                    points.push(json!({"$color_keypoint":[time,{"$color3":rgb}]}));
                }
            }
            if kind == 23 {
                json!({"$number_sequence":points})
            } else {
                json!({"$color_sequence":points})
            }
        }
        27 => {
            let mut v = Vec::with_capacity(2);
            for _ in 0..2 {
                let n = r.f32()?;
                if !n.is_finite() {
                    return Err(WireError::new("Nonfinite attribute value"));
                }
                v.push(n);
            }
            json!({"$number_range":v})
        }
        28 => {
            let mut points = Vec::with_capacity(2);
            for _ in 0..2 {
                let mut pair = Vec::with_capacity(2);
                for _ in 0..2 {
                    let n = r.f32()?;
                    if !n.is_finite() {
                        return Err(WireError::new("Nonfinite attribute value"));
                    }
                    pair.push(n);
                }
                points.push(json!({"$vector2":pair}));
            }
            json!({"$rect":points})
        }
        _ => return Err(WireError::new("Unsupported received attribute type")),
    };
    r.finish()?;
    Ok(result)
}
fn key_to_value(key: Option<(u64, u32)>) -> Value {
    key.map_or(Value::Null, |(s, i)| json!({"$ref":format!("{s}:{i}")}))
}
fn bytes_value(bytes: impl AsRef<[u8]>) -> Value {
    json!({"$bytes_hex":hex(bytes.as_ref())})
}
fn bytes_or_text(bytes: impl AsRef<[u8]>) -> Value {
    match std::str::from_utf8(bytes.as_ref()) {
        Ok(text) => json!(text),
        Err(_) => bytes_value(bytes),
    }
}
fn decode_hex_16(text: &str) -> Result<[u8; 16], WireError> {
    if text.len() != 32 {
        return Err(WireError::new("Invalid shared string digest key"));
    }
    let mut bytes = [0u8; 16];
    for index in 0..16 {
        bytes[index] = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16)
            .map_err(|_| WireError::new("Invalid shared string digest key"))?;
    }
    Ok(bytes)
}
fn hex(bytes: &[u8]) -> String {
    const TABLE: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(TABLE[usize::from(b >> 4)] as char);
        out.push(TABLE[usize::from(b & 15)] as char);
    }
    out
}
fn finite_f32(value: f32) -> Result<Value, WireError> {
    if value.is_nan() {
        Ok(json!({"$number":"nan"}))
    } else if value == f32::INFINITY {
        Ok(json!({"$number":"inf"}))
    } else if value == f32::NEG_INFINITY {
        Ok(json!({"$number":"-inf"}))
    } else {
        Ok(json!(value))
    }
}
fn finite_f64(value: f64) -> Result<Value, WireError> {
    if value.is_nan() {
        Ok(json!({"$number":"nan"}))
    } else if value == f64::INFINITY {
        Ok(json!({"$number":"inf"}))
    } else if value == f64::NEG_INFINITY {
        Ok(json!({"$number":"-inf"}))
    } else {
        Ok(json!(value))
    }
}
fn blake2b_128(value: &[u8]) -> [u8; 16] {
    digest_raw(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{ClassDesc, EnumDesc, PropertyDesc};

    fn schema(with_float: bool) -> Schema {
        Schema {
            version: 3,
            flags: 0,
            enums: Vec::<EnumDesc>::new(),
            classes: vec![ClassDesc {
                id: 0,
                name: "Part".into(),
                properties: if with_float { vec![0] } else { vec![] },
                events: vec![],
                isr_properties: vec![],
                unknown_u16: 0,
            }],
            properties: if with_float {
                vec![PropertyDesc {
                    id: 0,
                    class: 0,
                    name: "Transparency".into(),
                    type_id: 11,
                    enum_id: None,
                    lr_type: None,
                }]
            } else {
                vec![]
            },
            events: vec![],
            strings: vec![],
            names: vec![],
            lr_types: vec![],
            parent_property_id: 1,
            decoded_bytes: 0,
        }
    }

    fn parser(schema: &Schema) -> PacketParser<'_> {
        static EMPTY: std::sync::OnceLock<HashMap<[u8; 16], Arc<SharedPayload>>> =
            std::sync::OnceLock::new();
        PacketParser {
            schema,
            known_shared: EMPTY.get_or_init(HashMap::new),
            shared_additions: Vec::new(),
            newly_shared: HashMap::new(),
            actions: Vec::new(),
            events: Vec::new(),
            pings: Vec::new(),
            tags: Vec::new(),
            regions: Vec::new(),
            metadata: Vec::new(),
            property_changes: Vec::new(),
            pending_shared: HashSet::new(),
            next_sequence: 0,
            last_region: None,
            preserve_remote_success_variants: false,
            shared_bytes: 0,
            compact_direct_properties: false,
            compact_property_keys: Vec::new(),
        }
    }

    fn decode_hex(text: &str) -> Vec<u8> {
        let compact = text
            .chars()
            .filter(|c| !c.is_ascii_whitespace())
            .collect::<String>();
        compact
            .as_bytes()
            .chunks_exact(2)
            .map(|pair| {
                let byte = std::str::from_utf8(pair).unwrap();
                u8::from_str_radix(byte, 16).unwrap()
            })
            .collect()
    }

    fn instance_packet(float: Option<f32>) -> Vec<u8> {
        let mut bytes = vec![131, 2, 1]; // packet, create, scope 1
        bytes.extend_from_slice(&7u32.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        if let Some(value) = float {
            bytes.push(0); // property order index
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        bytes.push(255); // end first property list
        bytes.push(255); // end second property list
        bytes.push(0); // null parent scope
        bytes.push(0); // packet terminator
        bytes
    }

    fn append_varuint(mut value: u64, out: &mut Vec<u8>) {
        while value >= 0x80 {
            out.push((value as u8) | 0x80);
            value >>= 7;
        }
        out.push(value as u8);
    }

    fn append_instance_key(id: u32, out: &mut Vec<u8>) {
        append_varuint(1, out);
        out.extend_from_slice(&id.to_le_bytes());
    }

    fn append_shared_entry(raw: &[u8], out: &mut Vec<u8>) -> [u8; 16] {
        let key = digest_raw(raw);
        out.extend_from_slice(&key);
        out.push(0);
        append_varuint(raw.len() as u64, out);
        out.extend_from_slice(raw);
        key
    }

    fn append_shared_create(id: u32, raw: &[u8], define: bool, out: &mut Vec<u8>) {
        out.push(2);
        append_instance_key(id, out);
        out.extend_from_slice(&0u16.to_le_bytes());
        out.push(0); // property order index
        let key = digest_raw(raw);
        out.extend_from_slice(&key);
        out.extend_from_slice(&[255, 255]);
        if define {
            append_shared_entry(raw, out);
        }
        out.push(0); // nil parent
    }

    fn append_shared_set(id: u32, raw: &[u8], define: bool, out: &mut Vec<u8>) {
        out.push(3);
        append_instance_key(id, out);
        out.extend_from_slice(&0u16.to_le_bytes());
        out.push(0); // reset version
        out.extend_from_slice(&digest_raw(raw));
        if define {
            append_shared_entry(raw, out);
        }
    }

    #[test]
    fn event_receipts_share_sequence_and_action_order_with_world_mutations() {
        let mut packet_schema = schema(true);
        packet_schema.classes[0].events.push(0);
        packet_schema.events.push(crate::schema::EventDesc {
            id: 0,
            class: 0,
            name: "Changed".into(),
            arguments: vec![],
        });
        let mut decoder = ReplicationDecoder::new(packet_schema);

        // create -> event -> property set -> event -> remove, all received in
        // one ordinary replication packet. Each action must keep its wire order.
        let mut bytes = vec![131, 2, 1];
        bytes.extend_from_slice(&7u32.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(&[255, 255, 0]); // no properties, nil parent
        bytes.extend_from_slice(&[7, 1]);
        bytes.extend_from_slice(&7u32.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes()); // event 0
        bytes.extend_from_slice(&[3, 1]);
        bytes.extend_from_slice(&7u32.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes()); // Transparency property
        bytes.push(0); // no reset
        bytes.extend_from_slice(&0.25f32.to_le_bytes());
        bytes.extend_from_slice(&[7, 1]);
        bytes.extend_from_slice(&7u32.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(&[1, 1]);
        bytes.extend_from_slice(&7u32.to_le_bytes());
        bytes.push(0);

        let transaction = decoder.packet(&bytes).unwrap();
        let actions = transaction.world_delta["actions"].as_array().unwrap();
        assert_eq!(
            actions
                .iter()
                .map(|action| action["kind"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["create", "event", "set", "event", "remove"]
        );
        assert_eq!(
            actions
                .iter()
                .map(|action| action["sequence"].as_u64().unwrap())
                .collect::<Vec<_>>(),
            [1, 2, 3, 4, 5]
        );
        assert_eq!(transaction.world_delta["sequence"], 5);
        assert_eq!(transaction.events.len(), 2);
        assert_eq!(transaction.property_changes.len(), 1);
    }

    fn framed_stream_packet(
        kind: u8,
        instance_count: u32,
        content: &[u8],
        suffix: &[u8],
    ) -> Vec<u8> {
        let mut compressed = Vec::new();
        zstd::stream::copy_encode(content, &mut compressed, 0).unwrap();
        let mut bytes = vec![131, kind];
        bytes.extend_from_slice(&instance_count.to_le_bytes());
        bytes.extend_from_slice(&(compressed.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&(content.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&compressed);
        bytes.extend_from_slice(suffix);
        bytes.push(0);
        bytes
    }

    #[test]
    fn ordinary_create_and_nonfinite_property_preserve_wire_value() {
        let mut decoder = ReplicationDecoder::new(schema(true));
        let tx = decoder
            .packet(&instance_packet(Some(f32::INFINITY)))
            .unwrap();
        let node = &tx.world_delta["actions"][0]["node"];
        assert_eq!(node["id"], "1:7");
        assert_eq!(node["properties"]["Transparency"], json!({"$number":"inf"}));
        assert_eq!(decoder.sequence(), 0); // parse is staged until commit
        decoder.commit(&tx).unwrap();
        assert_eq!(decoder.sequence(), 1);
    }

    #[test]
    fn malformed_packet_does_not_advance_decoder() {
        let mut decoder = ReplicationDecoder::new(schema(false));
        let mut truncated = instance_packet(None);
        truncated.pop();
        assert!(decoder.packet(&truncated).is_err());
        assert_eq!(decoder.sequence(), 0);
    }

    #[test]
    fn retained_shared_payload_bytes_are_exact_and_failed_commit_rolls_back() {
        let mut decoder = ReplicationDecoder::new(schema(false));
        let payload = SharedPayload::from_raw(b"shared bytes".to_vec());
        let key = payload.digest();
        let transaction = PacketTransaction {
            world_delta: json!({"version":1,"delta":true,"sequence":1,"actions":[]}),
            events: vec![],
            pings: vec![],
            tags: vec![],
            regions: vec![],
            metadata: vec![],
            shared_additions: vec![(key, Arc::clone(&payload))],
            property_changes: vec![],
            next_sequence: 1,
            base_sequence: 0,
            base_generation: 0,
            next_region: None,
        };
        decoder.commit(&transaction).unwrap();
        assert_eq!(decoder.shared_string_count(), 1);
        assert_eq!(decoder.shared_string_payload_bytes(), payload.len());
        assert_eq!(decoder.shared_string_stored_bytes(), payload.stored_bytes());

        let duplicate = PacketTransaction {
            world_delta: json!({"version":1,"delta":true,"sequence":2,"actions":[]}),
            events: vec![],
            pings: vec![],
            tags: vec![],
            regions: vec![],
            metadata: vec![],
            shared_additions: vec![(key, Arc::clone(&payload))],
            property_changes: vec![],
            next_sequence: 2,
            base_sequence: 1,
            base_generation: 1,
            next_region: None,
        };
        decoder.commit(&duplicate).unwrap();
        assert_eq!(decoder.shared_string_count(), 1);
        assert_eq!(decoder.shared_string_payload_bytes(), payload.len());
        assert_eq!(decoder.shared_string_stored_bytes(), payload.stored_bytes());

        let rejected_payload = SharedPayload::from_raw(b"not committed".to_vec());
        let rejected_key = rejected_payload.digest();
        let rejected = PacketTransaction {
            world_delta: json!({"version":1,"delta":true,"sequence":3,"actions":[]}),
            events: vec![],
            pings: vec![],
            tags: vec![],
            regions: vec![],
            metadata: vec![],
            shared_additions: vec![(rejected_key, rejected_payload)],
            property_changes: vec![],
            next_sequence: 3,
            base_sequence: 0,
            base_generation: 2,
            next_region: None,
        };
        assert!(decoder.commit(&rejected).is_err());
        assert_eq!(decoder.shared_string_count(), 1);
        assert_eq!(decoder.shared_string_payload_bytes(), payload.len());
        assert_eq!(decoder.shared_string_stored_bytes(), payload.stored_bytes());
    }

    #[test]
    fn compression_does_not_bypass_logical_shared_payload_limit() {
        let mut decoder = ReplicationDecoder::new(schema(false));
        let large = SharedPayload::from_raw(vec![0x33; MAX_SHARED_BYTES]);
        assert!(large.is_compressed());
        assert_eq!(large.len(), MAX_SHARED_BYTES);
        assert!(large.stored_bytes() < 1024 * 1024);
        let first = PacketTransaction {
            world_delta: json!({"version":1,"delta":true,"sequence":1,"actions":[]}),
            events: vec![],
            pings: vec![],
            tags: vec![],
            regions: vec![],
            metadata: vec![],
            shared_additions: vec![(large.digest(), Arc::clone(&large))],
            property_changes: vec![],
            next_sequence: 1,
            base_sequence: 0,
            base_generation: 0,
            next_region: None,
        };
        decoder.commit(&first).unwrap();
        assert_eq!(decoder.shared_string_payload_bytes(), MAX_SHARED_BYTES);
        assert_eq!(decoder.shared_string_stored_bytes(), large.stored_bytes());

        let extra = SharedPayload::from_raw(vec![0x44]);
        let rejected = PacketTransaction {
            world_delta: json!({"version":1,"delta":true,"sequence":2,"actions":[]}),
            events: vec![],
            pings: vec![],
            tags: vec![],
            regions: vec![],
            metadata: vec![],
            shared_additions: vec![(extra.digest(), extra)],
            property_changes: vec![],
            next_sequence: 2,
            base_sequence: 1,
            base_generation: 1,
            next_region: None,
        };
        assert!(decoder.commit(&rejected).is_err());
        assert_eq!(decoder.shared_string_count(), 1);
        assert_eq!(decoder.shared_string_payload_bytes(), MAX_SHARED_BYTES);
        assert_eq!(decoder.shared_string_stored_bytes(), large.stored_bytes());
        assert_eq!(decoder.sequence(), 1);
    }

    #[test]
    fn shared_payload_resolver_reads_committed_and_staged_arcs_without_mutation() {
        fn transaction(
            sequence: u64,
            generation: u64,
            additions: Vec<([u8; 16], Arc<SharedPayload>)>,
        ) -> PacketTransaction {
            PacketTransaction {
                world_delta: json!({"version":1,"delta":true,"sequence":sequence,"actions":[]}),
                events: vec![],
                pings: vec![],
                tags: vec![],
                regions: vec![],
                metadata: vec![],
                shared_additions: additions,
                property_changes: vec![],
                next_sequence: sequence,
                base_sequence: sequence - 1,
                base_generation: generation,
                next_region: None,
            }
        }

        let mut decoder = ReplicationDecoder::new(schema(false));
        let committed_raw = vec![0x5au8; 8192];
        let committed = SharedPayload::from_raw(committed_raw.clone());
        assert!(committed.is_compressed());
        let committed_key = committed.digest();
        decoder
            .commit(&transaction(
                1,
                0,
                vec![(committed_key, Arc::clone(&committed))],
            ))
            .unwrap();

        let staged_raw = b"staged in this packet".to_vec();
        let staged = SharedPayload::from_raw(staged_raw.clone());
        let staged_key = staged.digest();
        let tx = transaction(2, 1, vec![(staged_key, Arc::clone(&staged))]);
        let count_before = decoder.shared_string_count();
        let bytes_before = decoder.shared_string_payload_bytes();
        let stored_before = decoder.shared_string_stored_bytes();
        let sequence_before = decoder.sequence();

        let retained = {
            let resolve = decoder.shared_payload_resolver(&tx);
            let committed_match = resolve(&committed_raw).unwrap();
            let staged_match = resolve(&staged_raw).unwrap();
            assert!(Arc::ptr_eq(&committed, &committed_match));
            assert!(Arc::ptr_eq(&staged, &staged_match));
            let mut wrong = committed_raw.clone();
            wrong[0] ^= 1;
            assert!(resolve(&wrong).is_none());
            assert!(resolve(b"missing payload").is_none());
            (committed_match, staged_match)
        };

        assert_eq!(decoder.shared_string_count(), count_before);
        assert_eq!(decoder.shared_string_payload_bytes(), bytes_before);
        assert_eq!(decoder.shared_string_stored_bytes(), stored_before);
        assert_eq!(decoder.sequence(), sequence_before);
        assert_eq!(tx.shared_additions.len(), 1);
        assert_eq!(retained.0.raw().as_ref(), committed_raw);
        assert_eq!(retained.1.raw().as_ref(), staged_raw);
    }

    #[test]
    fn globals_are_staged_and_accept_empty_container_set() {
        let mut decoder = ReplicationDecoder::new(schema(false));
        let tx = decoder.globals(&[129, 0, 0, 0, 0, 0, 0, 0, 1, 0]).unwrap();
        assert_eq!(tx.scope, 1);
        assert_eq!(tx.world_delta["actions"].as_array().unwrap().len(), 0);
        decoder.commit_globals(&tx).unwrap();
        assert_eq!(decoder.sequence(), 0);
        assert_eq!(decoder.client_scope(), Some(1));
        assert_eq!(decoder.global_flags(), Some(&[false; 7][..]));
    }

    #[test]
    fn streamed_create_and_absolute_region_are_decoded() {
        let mut content = vec![1];
        content.extend_from_slice(&7u32.to_le_bytes());
        content.extend_from_slice(&0u16.to_le_bytes());
        content.push(255);
        content.push(0); // null parent
        let mut decoder = ReplicationDecoder::new(schema(false));
        let stream = framed_stream_packet(13, 1, &content, &[0, 0, 4, 5, 6]);
        let tx = decoder.packet(&stream).unwrap();
        assert_eq!(tx.world_delta["actions"][0]["id"], "1:7");
        assert_eq!(tx.regions, vec![json!({"flags":0,"coordinate":[4,5,6]})]);
    }

    #[test]
    fn compact_direct_shared_create_set_matches_public_eager_receipt() {
        use crate::world::WorldStore;
        use serde_json::json;

        let mut property_schema = schema(true);
        property_schema.properties[0].type_id = 46;
        let mut decoder = ReplicationDecoder::new(property_schema);
        let text = "initial text payload ".repeat(40).into_bytes();
        let binary = (0..256).map(|value| value as u8).collect::<Vec<_>>();
        let updated = "updated text payload ".repeat(32).into_bytes();
        let mut wire = vec![131];
        append_shared_create(7, &text, true, &mut wire);
        append_shared_create(8, &binary, true, &mut wire);
        append_shared_create(9, &text, false, &mut wire);
        append_shared_set(7, &updated, true, &mut wire);
        append_shared_set(7, &updated, false, &mut wire);
        wire.push(0);

        let public = decoder.packet(&wire).unwrap();
        let compact = decoder.packet_compact(&wire).unwrap();
        assert_eq!(
            compact.packet.world_delta["actions"][0]["node"]["properties"]["Transparency"],
            Value::Null
        );
        assert_eq!(
            compact.packet.world_delta["actions"][2]["fields"]["Transparency"],
            Value::Null
        );
        assert_eq!(compact.shared_properties.len(), 5);
        let eager = compact.clone().into_eager().unwrap();
        assert_eq!(eager.world_delta, public.world_delta);
        assert_eq!(
            public.world_delta["actions"][0]["node"]["properties"]["Transparency"],
            bytes_value(&text)
        );
        assert_eq!(
            public.world_delta["actions"][2]["node"]["properties"]["Transparency"],
            json!(String::from_utf8(text.clone()).unwrap())
        );
        assert_eq!(eager.events, public.events);
        assert_eq!(eager.pings, public.pings);
        assert_eq!(eager.tags, public.tags);
        assert_eq!(eager.regions, public.regions);
        assert_eq!(eager.metadata, public.metadata);
        assert_eq!(eager.property_changes, public.property_changes);

        let empty = json!({"version":1,"sequence":0,"nodes":[]});
        let mut compact_world = WorldStore::from_snapshot(&empty).unwrap();
        let mut eager_world = WorldStore::from_snapshot(&empty).unwrap();
        {
            let resolver = decoder.shared_payload_resolver(&compact.packet);
            compact_world
                .apply_compact_delta(
                    &compact.packet.world_delta,
                    Some(&resolver),
                    &compact.shared_properties,
                )
                .unwrap();
        }
        {
            let resolver = decoder.shared_payload_resolver(&public);
            eager_world
                .apply_delta_with_shared(&public.world_delta, Some(&resolver))
                .unwrap();
        }
        assert_eq!(compact_world.to_snapshot(), eager_world.to_snapshot());
        assert_eq!(
            compact_world
                .view()
                .read_only_node("1:7")
                .unwrap()
                .property("Transparency"),
            Some(json!(String::from_utf8(updated).unwrap()))
        );
    }

    #[test]
    fn compact_shared_encoding_preserves_known_vs_pending_utf8_refs() {
        use serde_json::json;

        let mut property_schema = schema(true);
        property_schema.properties[0].type_id = 46;
        let mut decoder = ReplicationDecoder::new(property_schema);
        let text = "known cache text ".repeat(32).into_bytes();
        let mut first = vec![131];
        append_shared_create(31, &text, true, &mut first);
        first.push(0);
        let accepted = decoder.packet(&first).unwrap();
        decoder.validate_commit(&accepted).unwrap();
        decoder.commit(&accepted).unwrap();

        let mut next = vec![131];
        append_shared_create(32, &text, false, &mut next);
        next.push(0);
        let eager = decoder.packet(&next).unwrap();
        let compact = decoder.packet_compact(&next).unwrap();
        assert_eq!(
            compact.clone().into_eager().unwrap().world_delta,
            eager.world_delta
        );
        assert_eq!(
            eager.world_delta["actions"][0]["node"]["properties"]["Transparency"],
            json!(String::from_utf8(text).unwrap())
        );
    }

    #[test]
    fn compact_create_duplicate_property_keeps_last_list_assignment() {
        let mut property_schema = schema(true);
        property_schema.properties[0].type_id = 46;
        let decoder = ReplicationDecoder::new(property_schema);
        let first = "first shared assignment ".repeat(20).into_bytes();
        let last = "last shared assignment ".repeat(20).into_bytes();
        let mut wire = vec![131, 2];
        append_instance_key(41, &mut wire);
        wire.extend_from_slice(&0u16.to_le_bytes());
        wire.push(0);
        wire.extend_from_slice(&digest_raw(&first));
        wire.push(255); // end first property list
        wire.push(0);
        wire.extend_from_slice(&digest_raw(&last));
        wire.push(255); // end second property list
        append_shared_entry(&first, &mut wire);
        append_shared_entry(&last, &mut wire);
        wire.push(0); // nil parent
        wire.push(0); // packet terminator

        let public = decoder.packet(&wire).unwrap();
        let compact = decoder.packet_compact(&wire).unwrap();
        assert_eq!(compact.shared_properties.len(), 1);
        assert_eq!(
            compact.clone().into_eager().unwrap().world_delta,
            public.world_delta
        );
        assert_eq!(
            public.world_delta["actions"][0]["node"]["properties"]["Transparency"],
            bytes_value(&last)
        );
    }

    #[test]
    fn compact_direct_shared_stream_create_matches_public_eager_receipt() {
        use crate::world::WorldStore;
        use serde_json::json;

        let mut property_schema = schema(true);
        property_schema.properties[0].type_id = 47;
        // The streamed item kind selects one property list; content starts
        // directly with the instance key, without a list-count byte.
        let mut content = Vec::new();
        append_instance_key(19, &mut content);
        content.extend_from_slice(&0u16.to_le_bytes());
        content.push(0); // property order index
        let binary = (0..256).map(|value| (value * 37) as u8).collect::<Vec<_>>();
        content.extend_from_slice(&digest_raw(&binary));
        content.push(255);
        content.push(0); // nil parent
        append_shared_entry(&binary, &mut content);
        let wire = framed_stream_packet(11, 1, &content, &[]);
        let decoder = ReplicationDecoder::new(property_schema);
        let public = decoder.packet(&wire).unwrap();
        let compact = decoder.packet_compact(&wire).unwrap();
        assert_eq!(
            compact.clone().into_eager().unwrap().world_delta,
            public.world_delta
        );
        assert_eq!(compact.shared_properties.len(), 1);

        let empty = json!({"version":1,"sequence":0,"nodes":[]});
        let mut world = WorldStore::from_snapshot(&empty).unwrap();
        let resolver = decoder.shared_payload_resolver(&compact.packet);
        world
            .apply_compact_delta(
                &compact.packet.world_delta,
                Some(&resolver),
                &compact.shared_properties,
            )
            .unwrap();
        assert_eq!(
            world
                .view()
                .read_only_node("1:19")
                .unwrap()
                .property("Transparency"),
            Some(
                json!({"$bytes_hex":binary.iter().map(|byte| format!("{byte:02x}")).collect::<String>()})
            )
        );
    }

    #[test]
    fn compact_path_keeps_remote_event_shared_values_eager() {
        use crate::schema::EventArgument;

        let mut event_schema = schema(false);
        event_schema.classes[0].events.push(0);
        event_schema.events.push(crate::schema::EventDesc {
            id: 0,
            class: 0,
            name: "OnClientEvent".into(),
            arguments: vec![EventArgument {
                type_id: 46,
                enum_id: 0,
            }],
        });
        let decoder = ReplicationDecoder::new(event_schema);
        let bytes = "event payload ".repeat(32).into_bytes();
        let mut wire = vec![131, 7];
        append_instance_key(7, &mut wire);
        wire.extend_from_slice(&0u16.to_le_bytes());
        wire.extend_from_slice(&digest_raw(&bytes));
        append_shared_entry(&bytes, &mut wire);
        wire.push(0);

        let eager = decoder.packet(&wire).unwrap();
        let compact = decoder.packet_compact(&wire).unwrap();
        assert!(compact.shared_properties.is_empty());
        let compact_eager = compact.into_eager().unwrap();
        assert_eq!(compact_eager.world_delta, eager.world_delta);
        assert_eq!(compact_eager.events, eager.events);
        assert_eq!(compact_eager.events[0]["arguments"][0], bytes_value(&bytes));
    }

    #[test]
    fn compact_path_keeps_nested_shared_property_values_eager() {
        use serde_json::json;

        let mut nested_schema = schema(true);
        nested_schema.properties[0].type_id = 29; // array of variants
        let decoder = ReplicationDecoder::new(nested_schema);
        let raw = "nested shared property ".repeat(32).into_bytes();
        let digest = digest_raw(&raw);
        let mut wire = vec![131, 2];
        append_instance_key(22, &mut wire);
        wire.extend_from_slice(&0u16.to_le_bytes());
        wire.push(0); // property order index
        append_varuint(1, &mut wire); // one variant
        wire.push(46); // shared string variant
        wire.extend_from_slice(&digest);
        wire.extend_from_slice(&[255, 255]);
        append_shared_entry(&raw, &mut wire);
        wire.push(0); // nil parent
        wire.push(0); // packet terminator

        let eager = decoder.packet(&wire).unwrap();
        let compact = decoder.packet_compact(&wire).unwrap();
        assert!(compact.shared_properties.is_empty());
        let compact_eager = compact.into_eager().unwrap();
        assert_eq!(compact_eager.world_delta, eager.world_delta);
        assert_eq!(
            compact_eager.world_delta["actions"][0]["node"]["properties"]["Transparency"],
            json!([bytes_value(&raw)])
        );
    }

    #[test]
    fn relative_stream_region_without_absolute_anchor_is_rejected() {
        let mut decoder = ReplicationDecoder::new(schema(false));
        let stream = framed_stream_packet(13, 0, &[], &[1]);
        assert!(decoder.packet(&stream).is_err());
        assert_eq!(decoder.sequence(), 0);
    }

    #[test]
    fn python_reference_value_vectors_match_normalized_json() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../tests/fixtures/replication/python_reference_vectors.json"
        ))
        .unwrap();
        let schema = schema(false);
        for vector in fixture["vectors"].as_array().unwrap() {
            let bytes = decode_hex(vector["wire_hex"].as_str().unwrap());
            let mut reader = Reader::new(&bytes);
            let mut parser = parser(&schema);
            let kind = vector["type"].as_u64().unwrap() as u8;
            let actual = parser
                .value(
                    &mut reader,
                    kind,
                    vector["compressed"].as_bool().unwrap(),
                    0,
                    false,
                )
                .unwrap();
            assert_eq!(reader.remaining(), 0, "Python reference vector type {kind}");
            assert_eq!(
                actual, vector["expected"],
                "Python reference vector type {kind}"
            );
        }
    }

    #[test]
    fn packet_17_21_26_27_have_bounded_structured_decodes() {
        let mut decoder = ReplicationDecoder::new(schema(false));
        let mut stats = vec![131, 17];
        stats.extend_from_slice(&2u32.to_le_bytes());
        stats.extend([1, 1]);
        for value in [1.0f32, 2.0, 3.0, 4.0, 5.0] {
            stats.extend_from_slice(&value.to_le_bytes());
        }
        stats.push(0);
        let tx = decoder.packet(&stats).unwrap();
        assert_eq!(tx.metadata[0]["version"], 2);
        assert_eq!(tx.metadata[0]["final_values"][4], 5.0);

        let mut attribute = vec![131, 21, 1];
        attribute.extend_from_slice(&7u32.to_le_bytes());
        attribute.extend([1, b'x', 5, 4]);
        attribute.extend_from_slice(&42i32.to_le_bytes());
        attribute.push(0);
        let tx = decoder.packet(&attribute).unwrap();
        assert_eq!(tx.world_delta["actions"][0]["kind"], "attribute");
        assert_eq!(
            tx.world_delta["actions"][0]["attribute_update"]["value"],
            42
        );

        let mut focus = vec![131, 26];
        focus.extend_from_slice(&9u32.to_le_bytes());
        focus.extend([0, 1, 2, 3]);
        focus.extend_from_slice(&4.5f32.to_le_bytes());
        focus.extend_from_slice(&(-1.0f32).to_le_bytes());
        focus.extend([1, 0]);
        focus.extend_from_slice(&500u32.to_le_bytes());
        focus.push(0);
        let tx = decoder.packet(&focus).unwrap();
        assert_eq!(tx.metadata[0]["kind"], "replication_focus");
        assert_eq!(tx.metadata[0]["optional_float"], Value::Null);

        let mut remove = vec![131, 27];
        remove.extend_from_slice(&1u32.to_le_bytes());
        remove.push(1);
        remove.extend_from_slice(&7u32.to_le_bytes());
        remove.push(0);
        let tx = decoder.packet(&remove).unwrap();
        assert_eq!(tx.world_delta["actions"][0]["kind"], "remove");
        assert_eq!(tx.world_delta["actions"][0]["id"], "1:7");
    }

    #[test]
    fn cached_replication_subtype_two_validates_empty_batch_frame() {
        let mut compressed = Vec::new();
        zstd::stream::copy_encode(&[][..], &mut compressed, 0).unwrap();
        let mut packet = vec![131, 22, 2];
        packet.extend_from_slice(&(compressed.len() as u32).to_le_bytes());
        packet.extend_from_slice(&0u32.to_le_bytes());
        packet.extend(compressed);
        packet.extend_from_slice(&0u32.to_le_bytes()); // no instances
        packet.push(0); // no offsets
        packet.push(0); // terminator
        let decoder = ReplicationDecoder::new(schema(false));
        let tx = decoder.packet(&packet).unwrap();
        assert_eq!(tx.world_action_count(), 0);
    }

    #[test]
    fn cached_replication_consumes_consecutive_skip_ranges() {
        let mut content = vec![0xaa, 0xbb, 0xcc, 0xdd, 0xee];
        content.push(1);
        content.extend(7u32.to_le_bytes());
        content.extend(0u16.to_le_bytes());
        content.extend([255, 0]);
        let end = content.len() as u8;
        content.extend([0xab, 0xcd, 0xef]);
        let compressed = zstd::bulk::compress(&content, 1).unwrap();
        let mut packet = vec![131, 22, 2];
        packet.extend((compressed.len() as u32).to_le_bytes());
        packet.extend((content.len() as u32).to_le_bytes());
        packet.extend(compressed);
        packet.extend(1u32.to_le_bytes());
        packet.extend([4, 0, 3, 3, 2, end, 1, end + 1, 2, 0]);
        let decoder = ReplicationDecoder::new(schema(false));
        let tx = decoder.packet(&packet).unwrap();
        assert_eq!(tx.world_action_count(), 1);
        assert_eq!(tx.world_delta["actions"][0]["id"], "1:7");
        assert_eq!(tx.world_delta["actions"][0]["node"]["parent"], Value::Null);
    }
}
