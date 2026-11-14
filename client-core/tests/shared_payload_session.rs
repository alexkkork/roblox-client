use blake2::{
    digest::consts::{U16, U64},
    Blake2b, Digest,
};
use rove_client_core::schema::{ClassDesc, EnumDesc, PropertyDesc, Schema};
use rove_client_core::session::{SessionCore, SessionError};
use serde_json::{json, Value};

fn schema() -> Schema {
    Schema {
        version: 3,
        flags: 0,
        enums: Vec::<EnumDesc>::new(),
        classes: vec![ClassDesc {
            id: 0,
            name: "Part".into(),
            properties: vec![0],
            events: vec![],
            isr_properties: vec![],
            unknown_u16: 0,
        }],
        properties: vec![PropertyDesc {
            id: 0,
            class: 0,
            name: "SharedPayload".into(),
            type_id: 46,
            enum_id: None,
            lr_type: None,
        }],
        events: vec![],
        strings: vec![],
        names: vec![],
        lr_types: vec![],
        parent_property_id: 1,
        decoded_bytes: 0,
    }
}

fn varuint(mut value: usize, output: &mut Vec<u8>) {
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        output.push(byte);
        if value == 0 {
            break;
        }
    }
}

fn node_key(output: &mut Vec<u8>, scope: u64, index: u32) {
    varuint(scope as usize, output);
    if scope != 0 {
        output.extend_from_slice(&index.to_le_bytes());
    }
}

fn digest(payload: &[u8]) -> [u8; 16] {
    let hash = Blake2b::<U16>::digest(payload);
    let mut result = [0; 16];
    result.copy_from_slice(&hash);
    result
}

fn shared_entry(output: &mut Vec<u8>, payload: &[u8]) -> [u8; 16] {
    let key = digest(payload);
    output.extend_from_slice(&key);
    output.push(0); // inline payload
    varuint(payload.len(), output);
    output.extend_from_slice(payload);
    key
}

fn create_packet(payload: &[u8], index: u32) -> Vec<u8> {
    let key = digest(payload);
    let mut packet = vec![131, 2]; // replication, create
    node_key(&mut packet, 1, index);
    packet.extend_from_slice(&0_u16.to_le_bytes()); // class id Part
    packet.push(0); // property-order index
    packet.extend_from_slice(&key); // shared-string value reference
    packet.push(255); // first property list end
    packet.push(255); // second property list end
    let _ = shared_entry(&mut packet, payload);
    packet.push(0); // null parent
    packet.push(0); // replication packet terminator
    packet
}

fn create_reference_packet(payload: &[u8], index: u32) -> Vec<u8> {
    let key = digest(payload);
    let mut packet = vec![131, 2]; // replication, create
    node_key(&mut packet, 1, index);
    packet.extend_from_slice(&0_u16.to_le_bytes()); // class id Part
    packet.push(0); // property-order index
    packet.extend_from_slice(&key); // reference to an already committed shared string
    packet.push(255); // first property list end
    packet.push(255); // second property list end
    packet.push(0); // null parent
    packet.push(0); // replication packet terminator
    packet
}

fn update_packet(payload: &[u8], index: u32) -> Vec<u8> {
    let key = digest(payload);
    let mut packet = vec![131, 3]; // replication, property change
    node_key(&mut packet, 1, index);
    packet.extend_from_slice(&0_u16.to_le_bytes()); // property id
    packet.push(0); // not a reset
    packet.extend_from_slice(&key); // shared-string value reference
    let _ = shared_entry(&mut packet, payload);
    packet.push(0); // replication packet terminator
    packet
}

fn binary_payload(seed: u8, length: usize) -> Vec<u8> {
    (0..length)
        .map(|index| if index % 2 == 0 { 0xff } else { seed })
        .collect() // invalid UTF-8, highly compressible, exact raw shared bytes
}

fn expected_bytes(payload: &[u8]) -> Value {
    let mut hex = String::with_capacity(payload.len() * 2);
    for byte in payload {
        use std::fmt::Write;
        write!(&mut hex, "{byte:02x}").unwrap();
    }
    json!({"$bytes_hex": hex})
}

fn empty_globals() -> [u8; 10] {
    [129, 0, 0, 0, 0, 0, 0, 0, 1, 0]
}

fn hex(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write;
        write!(&mut output, "{byte:02x}").unwrap();
    }
    output
}

fn digest_snapshot(snapshot: &Value) -> String {
    let mut digest = Blake2b::<U64>::new();
    digest.update(snapshot["sequence"].as_u64().unwrap().to_le_bytes());
    for node in snapshot["nodes"].as_array().unwrap() {
        let bytes = serde_json::to_vec(node).unwrap();
        digest.update((bytes.len() as u64).to_le_bytes());
        digest.update(bytes);
    }
    hex(&digest.finalize())
}

fn part_node(index: u32, property: Value) -> Value {
    json!({
        "id": format!("1:{index}"),
        "class_name": "Part",
        "parent": null,
        "properties": {"SharedPayload": property},
        "origins": {},
        // this packet created the node completely: omitted properties are
        // constructor defaults
        "received_creation_complete": true,
        "constructor_defaults_known": true
    })
}

#[test]
fn shared_packet_payload_survives_world_cow_and_failed_ingest() {
    let mut session = SessionCore::with_schema(schema(), 16, 1024 * 1024).unwrap();
    session.ingest(&empty_globals()).unwrap();

    let first_payload = binary_payload(0x21, 4096);
    session.ingest(&create_packet(&first_payload, 7)).unwrap();
    let first_value = expected_bytes(&first_payload);
    let first_snapshot = session.snapshot().unwrap();
    assert_eq!(
        first_snapshot,
        json!({
            "version": 1,
            "sequence": 1,
            "nodes": [part_node(7, first_value.clone())]
        })
    );
    assert_eq!(
        session
            .world_view()
            .unwrap()
            .get_property("1:7", "SharedPayload"),
        Some(first_value.clone())
    );
    let first_profile = session.storage_profile();
    assert!(
        first_profile["shared_payload_backed_property_count"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert_eq!(
        first_profile["shared_property_unique_payload_bytes"],
        json!(first_payload.len())
    );
    assert_eq!(
        first_profile["world_state_blake2b512"],
        json!(digest_snapshot(&first_snapshot))
    );

    // A second record references the already committed digest, with no inline
    // payload. Both world properties must pin the exact same raw bytes.
    session
        .ingest(&create_reference_packet(&first_payload, 8))
        .unwrap();
    let duplicate_snapshot = session.snapshot().unwrap();
    assert_eq!(
        duplicate_snapshot,
        json!({"version":1,"sequence":2,"nodes":[
            part_node(7, first_value.clone()),part_node(8, first_value.clone())
        ]})
    );
    let duplicate_profile = session.storage_profile();
    assert_eq!(
        duplicate_profile["shared_payload_backed_property_count"],
        json!(2)
    );
    assert_eq!(
        duplicate_profile["shared_property_unique_payload_bytes"],
        json!(first_payload.len())
    );
    assert_eq!(duplicate_profile["shared_string_count"], json!(1));
    assert_eq!(
        duplicate_profile["shared_string_payload_bytes"],
        json!(first_payload.len())
    );
    assert_eq!(
        duplicate_profile["world_state_blake2b512"],
        json!(digest_snapshot(&duplicate_snapshot))
    );
    assert!(
        duplicate_profile["shared_string_stored_bytes"]
            .as_u64()
            .unwrap()
            < duplicate_profile["shared_string_payload_bytes"]
                .as_u64()
                .unwrap()
    );
    assert_eq!(
        duplicate_profile["shared_string_stored_bytes"],
        duplicate_profile["shared_property_unique_stored_bytes"]
    );

    let old_view = session.world_view().unwrap();
    let second_payload = binary_payload(0x42, 3072);
    session.ingest(&update_packet(&second_payload, 7)).unwrap();
    let second_value = expected_bytes(&second_payload);
    let new_view = session.world_view().unwrap();
    assert_eq!(
        old_view.get_property("1:7", "SharedPayload"),
        Some(first_value.clone())
    );
    assert_eq!(
        new_view.get_property("1:7", "SharedPayload"),
        Some(second_value.clone())
    );
    assert_eq!(
        new_view.get_property("1:8", "SharedPayload"),
        Some(first_value.clone())
    );
    let updated_snapshot = session.snapshot().unwrap();
    assert_eq!(
        updated_snapshot,
        json!({"version":1,"sequence":3,"nodes":[
            part_node(7, second_value.clone()),part_node(8, first_value.clone())
        ]})
    );
    let updated_profile = session.storage_profile();
    assert!(
        updated_profile["shared_payload_backed_property_count"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert_eq!(
        updated_profile["shared_property_unique_payload_bytes"],
        json!(first_payload.len() + second_payload.len())
    );
    assert_eq!(updated_profile["shared_string_count"], json!(2));
    assert_eq!(
        updated_profile["shared_string_payload_bytes"],
        json!(first_payload.len() + second_payload.len())
    );
    assert!(
        updated_profile["shared_string_stored_bytes"]
            .as_u64()
            .unwrap()
            < updated_profile["shared_string_payload_bytes"]
                .as_u64()
                .unwrap()
    );
    assert_eq!(
        updated_profile["shared_string_stored_bytes"],
        updated_profile["shared_property_unique_stored_bytes"]
    );
    assert_eq!(
        updated_profile["world_state_blake2b512"],
        json!(digest_snapshot(&updated_snapshot))
    );

    // Actor/local WorldView writes detach just this property and leave both
    // the session and the other record's shared payload unchanged.
    let mut local_view = new_view.clone();
    let local_value = json!({"$bytes_hex": "ff00".repeat(1024)});
    local_view
        .set_property("1:7", "SharedPayload", local_value.clone())
        .unwrap();
    assert_eq!(
        local_view.get_property("1:7", "SharedPayload"),
        Some(local_value)
    );
    assert_eq!(
        new_view.get_property("1:7", "SharedPayload"),
        Some(second_value.clone())
    );
    assert_eq!(
        new_view.get_property("1:8", "SharedPayload"),
        Some(first_value.clone())
    );

    let before_rejection = session.snapshot().unwrap();
    let profile_before_rejection = session.storage_profile();
    assert!(matches!(
        session.ingest(&[131, 3]),
        Err(SessionError::InvalidPacket(_))
    ));
    assert_eq!(session.snapshot().unwrap(), before_rejection);
    assert_eq!(session.storage_profile(), profile_before_rejection);

    let retained_node = new_view.read_only_node("1:7").unwrap();
    let retained_duplicate = new_view.read_only_node("1:8").unwrap();
    drop(session);
    drop(old_view);
    drop(new_view);
    drop(local_view);
    assert_eq!(
        retained_node.property("SharedPayload"),
        Some(expected_bytes(&second_payload))
    );
    assert_eq!(
        retained_duplicate.property("SharedPayload"),
        Some(expected_bytes(&first_payload))
    );
}
