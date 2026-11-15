//! Data-only conversion of bounded RBXM KeyframeSequence assets. No scripts run.
use rbx_dom_weak::{
    types::{CFrame, Ref, Variant},
    Instance, WeakDom,
};
use serde_json::{json, Value};
use std::io::Read;

pub const MAX_INPUT: usize = 4 * 1024 * 1024;
const MAX_NODES: usize = 65_536;

fn word(data: &[u8], at: usize) -> Result<usize, &'static str> {
    let bytes = data.get(at..at + 4).ok_or("truncated_header")?;
    Ok(u32::from_le_bytes(bytes.try_into().unwrap()) as usize)
}

fn validate_container(data: &[u8]) -> Result<(), &'static str> {
    if data.len() > MAX_INPUT
        || data.len() < 32
        || &data[..14] != b"<roblox!\x89\xff\r\n\x1a\n"
        || data[14..16] != [0, 0]
    {
        return Err("unsupported_container");
    }
    if word(data, 16)? > 128 || word(data, 20)? > MAX_NODES {
        return Err("instance_limit");
    }
    let mut cursor = 32;
    let mut expanded = 0usize;
    while cursor < data.len() {
        let compressed = word(data, cursor + 4)?;
        let plain = word(data, cursor + 8)?;
        expanded = expanded.checked_add(plain).ok_or("expansion_limit")?;
        if plain > 8 * 1024 * 1024 || expanded > 32 * 1024 * 1024 {
            return Err("expansion_limit");
        }
        let size = if compressed == 0 { plain } else { compressed };
        let end = cursor.checked_add(16 + size).ok_or("truncated_chunk")?;
        if end > data.len() {
            return Err("truncated_chunk");
        }
        if data[cursor..cursor + 4] == *b"END\0" {
            return if end == data.len() {
                Ok(())
            } else {
                Err("trailing_data")
            };
        }
        cursor = end;
    }
    Err("missing_end_chunk")
}

fn number(node: &Instance, key: &str, default: f64) -> Result<f64, &'static str> {
    let value = match node.properties.get(&key.into()) {
        Some(Variant::Float32(v)) => *v as f64,
        Some(Variant::Float64(v)) => *v,
        Some(_) => return Err("invalid_numeric_property"),
        None => default,
    };
    if value.is_finite() {
        Ok(value)
    } else {
        Err("nonfinite_property")
    }
}

fn enumeration(node: &Instance, key: &str) -> Result<Value, &'static str> {
    match node.properties.get(&key.into()) {
        Some(Variant::Enum(v)) => Ok(json!(v.to_u32())),
        Some(Variant::EnumItem(v)) => Ok(json!(v.value)),
        None => Ok(Value::Null),
        _ => Err("invalid_enum_property"),
    }
}

fn rig_data(node: &Instance) -> Result<Value, &'static str> {
    if !node.children().is_empty() {
        return Err("unsupported_rig_data_children");
    }
    let mut fields = serde_json::Map::new();
    for key in [
        "label",
        "name",
        "parent",
        "preTransform",
        "postTransform",
        "transform",
    ] {
        if let Some(value) = node.properties.get(&key.into()) {
            let bytes: &[u8] = match value {
                Variant::BinaryString(v) => v.as_ref(),
                Variant::String(v) => v.as_bytes(),
                _ => return Err("unsupported_rig_data_field"),
            };
            if bytes.len() > MAX_INPUT {
                return Err("rig_data_size_limit");
            }
            const HEX: &[u8] = b"0123456789abcdef";
            let mut encoded = String::with_capacity(bytes.len() * 2);
            for byte in bytes {
                encoded.push(HEX[(byte >> 4) as usize] as char);
                encoded.push(HEX[(byte & 15) as usize] as char);
            }
            fields.insert(key.to_owned(), json!({"bytes_hex":encoded}));
        }
    }
    // Retargeting must consume these fields before claiming rig-data support.
    // Preserve exact bytes rather than silently dropping the auxiliary record.
    Ok(json!({"name":node.name,"fields":fields,"interpreted":false}))
}

fn pose(
    dom: &WeakDom,
    reference: Ref,
    parent: &[String],
    depth: usize,
    output: &mut Vec<Value>,
    numbers: &mut Vec<Value>,
) -> Result<(), &'static str> {
    if depth > 64 || output.len() + numbers.len() >= MAX_NODES {
        return Err("pose_limit");
    }
    let node = dom.get_by_ref(reference).ok_or("missing_instance")?;
    if node.class.as_str() != "Pose" {
        return Err("unsupported_pose_child");
    }
    if node.name.len() > 256 {
        return Err("pose_name_limit");
    }
    let frame = match node.properties.get(&"CFrame".into()) {
        Some(Variant::CFrame(v)) => *v,
        None => CFrame::identity(),
        _ => return Err("invalid_pose_frame"),
    };
    let p = frame.position;
    let m = frame.orientation;
    let position = [p.x, p.y, p.z];
    let rotation = [
        m.x.x, m.x.y, m.x.z, m.y.x, m.y.y, m.y.z, m.z.x, m.z.y, m.z.z,
    ];
    if !position
        .iter()
        .chain(rotation.iter())
        .all(|v| v.is_finite())
    {
        return Err("nonfinite_pose_frame");
    }
    let mut path = parent.to_vec();
    path.push(node.name.clone());
    let mut sample = json!({"path":path,"position":position,"rotation":rotation});
    // Version-1 consumers default to weight 1 and Linear/In. Omitting those
    // values avoids repeating them tens of thousands of times in real clips.
    let weight = number(node, "Weight", 1.0)?;
    if weight != 1.0 {
        sample["weight"] = json!(weight);
    }
    let mask_weight = number(node, "MaskWeight", 0.0)?;
    if mask_weight != 0.0 {
        sample["mask_weight"] = json!(mask_weight);
    }
    for key in ["EasingStyle", "EasingDirection"] {
        let value = enumeration(node, key)?;
        if !value.is_null() && value != 0 {
            sample[if key == "EasingStyle" {
                "easing_style"
            } else {
                "easing_direction"
            }] = value;
        }
    }
    output.push(sample);
    for reference in node.children() {
        let child = dom.get_by_ref(*reference).ok_or("missing_pose_child")?;
        if child.class.as_str() == "Pose" {
            pose(dom, *reference, &path, depth + 1, output, numbers)?;
        } else if child.class.as_str() == "Folder"
            && child.name == "FaceControls"
            && node.name == "Head"
        {
            // NumberPose is the documented facial channel hierarchy beneath a
            // Head Pose. Preserve scalar samples separately from joint CFrames.
            // Clamping belongs to evaluation; decoding retains the stored value.
            for reference in child.children() {
                if output.len() + numbers.len() >= MAX_NODES {
                    return Err("pose_limit");
                }
                let scalar = dom.get_by_ref(*reference).ok_or("missing_number_pose")?;
                if scalar.class.as_str() != "NumberPose" || !scalar.children().is_empty() {
                    return Err("unsupported_face_controls_child");
                }
                if scalar.name.len() > 256 {
                    return Err("pose_name_limit");
                }
                let mut channel = path.clone();
                channel.push("FaceControls".to_owned());
                channel.push(scalar.name.clone());
                numbers.push(json!({"path":channel,"value":number(scalar,"Value",0.0)?,
                    "weight":number(scalar,"Weight",1.0)?,
                    "easing_style":enumeration(scalar,"EasingStyle")?,
                    "easing_direction":enumeration(scalar,"EasingDirection")?}));
            }
        } else {
            return Err("unsupported_pose_child");
        }
    }
    Ok(())
}

pub fn decode(data: &[u8]) -> Result<Value, &'static str> {
    if data.is_empty() || data.len() > MAX_INPUT {
        return Err("input_size_limit");
    }
    if data.starts_with(&[0x1f, 0x8b]) {
        let mut plain = Vec::new();
        flate2::read::GzDecoder::new(data)
            .take((MAX_INPUT + 1) as u64)
            .read_to_end(&mut plain)
            .map_err(|_| "invalid_compression")?;
        if plain.len() > MAX_INPUT {
            return Err("expansion_limit");
        }
        return decode_plain(&plain);
    }
    decode_plain(data)
}

fn decode_plain(data: &[u8]) -> Result<Value, &'static str> {
    let dom = if data.starts_with(b"<roblox!") {
        validate_container(data)?;
        rbx_binary::from_reader(data).map_err(|_| "model_decode_failed")?
    } else {
        validate_xml(data)?;
        rbx_xml::from_reader_default(data).map_err(|_| "model_decode_failed")?
    };
    decode_dom(&dom)
}

fn validate_xml(data: &[u8]) -> Result<(), &'static str> {
    let text = std::str::from_utf8(data).map_err(|_| "unsupported_container")?;
    if !text
        .trim_start_matches('\u{feff}')
        .trim_start()
        .starts_with('<')
        || text.contains("<!DOCTYPE")
        || text.contains("<!ENTITY")
    {
        return Err("unsupported_container");
    }
    let mut depth = 0usize;
    let mut nodes = 0usize;
    let mut events = 0usize;
    for event in xml::reader::EventReader::new(data) {
        events += 1;
        if events > 400_000 {
            return Err("xml_event_limit");
        }
        match event.map_err(|_| "model_decode_failed")? {
            xml::reader::XmlEvent::StartElement { name, .. } => {
                depth += 1;
                if depth > 128 {
                    return Err("pose_limit");
                }
                if name.local_name == "Item" {
                    nodes += 1;
                    if nodes > MAX_NODES {
                        return Err("instance_limit");
                    }
                }
            }
            xml::reader::XmlEvent::EndElement { .. } => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    Ok(())
}

fn decode_dom(dom: &WeakDom) -> Result<Value, &'static str> {
    if dom.root().children().len() != 1 {
        return Err("expected_one_animation_clip");
    }
    let clip = dom
        .get_by_ref(dom.root().children()[0])
        .ok_or("missing_clip")?;
    if clip.class.as_str() != "KeyframeSequence" {
        return Err("unsupported_animation_clip");
    }
    let looped = match clip.properties.get(&"Loop".into()) {
        Some(Variant::Bool(v)) => *v,
        None => true, // Measured KeyframeSequence constructor default.
        _ => return Err("invalid_loop_property"),
    };
    let mut frames = Vec::new();
    let mut rigs = Vec::new();
    for reference in clip.children() {
        let node = dom.get_by_ref(*reference).ok_or("missing_keyframe")?;
        if node.class.as_str() == "AnimationRigData" {
            rigs.push(rig_data(node)?);
            continue;
        }
        if node.class.as_str() != "Keyframe" {
            return Err("unsupported_clip_child");
        }
        let time = number(node, "Time", 0.0)?;
        if !(0.0..=86400.0).contains(&time) {
            return Err("keyframe_time_limit");
        }
        let mut poses = Vec::new();
        let mut numbers = Vec::new();
        let mut markers = Vec::new();
        for child_ref in node.children() {
            let child = dom.get_by_ref(*child_ref).ok_or("missing_keyframe_child")?;
            if child.class.as_str() == "Pose" {
                pose(&dom, *child_ref, &[], 0, &mut poses, &mut numbers)?;
            } else if child.class.as_str() == "KeyframeMarker" {
                let value = match child.properties.get(&"Value".into()) {
                    Some(Variant::String(v)) => v.as_str(),
                    None => "",
                    _ => return Err("invalid_marker_value"),
                };
                if child.name.len() > 256 || value.len() > 4096 {
                    return Err("marker_size_limit");
                }
                markers.push(json!({"name":child.name,"value":value}));
            } else {
                return Err("unsupported_keyframe_child");
            }
        }
        frames.push(json!({"time":time,"name":node.name,"poses":poses,"number_poses":numbers,"markers":markers}));
    }
    if frames.is_empty() {
        return Err("empty_keyframe_sequence");
    }
    frames.sort_by(|a, b| {
        a["time"]
            .as_f64()
            .unwrap()
            .total_cmp(&b["time"].as_f64().unwrap())
    });
    let length = frames.last().unwrap()["time"].clone();
    Ok(
        json!({"version":1,"kind":"KeyframeSequence","name":clip.name,"looped":looped,
        "authored_hip_height":number(clip,"AuthoredHipHeight",1.35)?,
        "priority":enumeration(clip,"Priority")?,"length":length,"keyframes":frames,"rig_data":rigs}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use rbx_dom_weak::{
        types::{BinaryString, Enum, Matrix3, Vector3},
        InstanceBuilder,
    };

    #[test]
    fn xml_and_gzip_decode_the_same_pose_channels() {
        use std::io::Write;
        let xml = include_bytes!("../../tests/fixtures/animations/two-keyframes.rbxmx");
        let plain = decode(xml).unwrap();
        assert_eq!(plain["length"], 2.0);
        assert_eq!(
            plain["keyframes"][1]["poses"][1]["position"],
            json!([4.0, 0.0, 0.0])
        );
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(xml).unwrap();
        assert_eq!(decode(&encoder.finish().unwrap()).unwrap(), plain);
    }

    #[test]
    fn xml_preflight_and_gzip_expansion_are_bounded() {
        use std::io::Write;
        assert_eq!(
            decode(b"<!DOCTYPE roblox [<!ENTITY x 'boom'>]><roblox/>"),
            Err("unsupported_container")
        );
        let deep = format!("{}{}", "<x>".repeat(129), "</x>".repeat(129));
        assert_eq!(validate_xml(deep.as_bytes()), Err("pose_limit"));
        let nodes = format!("<roblox>{}</roblox>", "<Item/>".repeat(MAX_NODES + 1));
        assert_eq!(validate_xml(nodes.as_bytes()), Err("instance_limit"));
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(&vec![b'x'; MAX_INPUT + 1]).unwrap();
        assert_eq!(decode(&encoder.finish().unwrap()), Err("expansion_limit"));
        assert_eq!(decode(&[0x1f, 0x8b, 0]), Err("invalid_compression"));
    }

    fn decode_authored(builder: InstanceBuilder) -> Result<Value, &'static str> {
        let dom = WeakDom::new(InstanceBuilder::new("DataModel").with_child(builder));
        let mut bytes = Vec::new();
        rbx_binary::to_writer(&mut bytes, &dom, dom.root().children()).unwrap();
        decode(&bytes)
    }

    #[test]
    fn authored_body_clip_can_exceed_twenty_thousand_pose_samples() {
        let mut sequence = InstanceBuilder::new("KeyframeSequence");
        for time in 0..210 {
            let mut frame =
                InstanceBuilder::new("Keyframe").with_property("Time", time as f32 / 30.0);
            for channel in 0..100 {
                frame = frame
                    .with_child(InstanceBuilder::new("Pose").with_name(format!("Joint{channel}")));
            }
            sequence = sequence.with_child(frame);
        }
        let clip = decode_authored(sequence).unwrap();
        assert_eq!(
            clip["keyframes"]
                .as_array()
                .unwrap()
                .iter()
                .map(|f| f["poses"].as_array().unwrap().len())
                .sum::<usize>(),
            21_000
        );
        let pose = &clip["keyframes"][0]["poses"][0];
        assert!(pose.get("weight").is_none());
        assert!(pose.get("easing_style").is_none());
        assert!(pose.get("easing_direction").is_none());
    }

    #[test]
    fn facial_channels_preserve_values_paths_weights_and_easing() {
        let channels = InstanceBuilder::new("Folder")
            .with_name("FaceControls")
            .with_child(
                InstanceBuilder::new("NumberPose")
                    .with_name("JawDrop")
                    .with_property("Value", 1.25f64)
                    .with_property("Weight", 0.5f32)
                    .with_property("EasingStyle", Enum::from_u32(1))
                    .with_property("EasingDirection", Enum::from_u32(2)),
            )
            .with_child(InstanceBuilder::new("NumberPose").with_name("LeftEyeClosed"));
        let head = InstanceBuilder::new("Pose")
            .with_name("Head")
            .with_child(channels);
        let root = InstanceBuilder::new("Pose")
            .with_name("HumanoidRootPart")
            .with_child(head);
        let frame = InstanceBuilder::new("Keyframe")
            .with_property("Time", 0.25f32)
            .with_child(root);
        let clip = InstanceBuilder::new("KeyframeSequence").with_child(frame);
        let out = decode_authored(clip).unwrap();
        let frame = &out["keyframes"][0];
        assert_eq!(frame["poses"].as_array().unwrap().len(), 2);
        let samples = frame["number_poses"].as_array().unwrap();
        assert_eq!(samples.len(), 2);
        assert_eq!(
            samples[0]["path"],
            json!(["HumanoidRootPart", "Head", "FaceControls", "JawDrop"])
        );
        assert_eq!(samples[0]["value"], 1.25);
        assert_eq!(samples[0]["weight"], 0.5);
        assert_eq!(samples[0]["easing_style"], 1);
        assert_eq!(samples[0]["easing_direction"], 2);
        assert_eq!(samples[1]["value"], 0.0);
    }

    #[test]
    fn facial_channels_reject_nested_unknown_and_nonfinite_data() {
        for child in [
            InstanceBuilder::new("NumberPose").with_child(InstanceBuilder::new("NumberPose")),
            InstanceBuilder::new("StringValue"),
        ] {
            let clip = InstanceBuilder::new("KeyframeSequence").with_child(
                InstanceBuilder::new("Keyframe").with_child(
                    InstanceBuilder::new("Pose").with_name("Head").with_child(
                        InstanceBuilder::new("Folder")
                            .with_name("FaceControls")
                            .with_child(child),
                    ),
                ),
            );
            assert_eq!(
                decode_authored(clip),
                Err("unsupported_face_controls_child")
            );
        }
        let clip = InstanceBuilder::new("KeyframeSequence").with_child(
            InstanceBuilder::new("Keyframe").with_child(
                InstanceBuilder::new("Pose").with_name("Head").with_child(
                    InstanceBuilder::new("Folder")
                        .with_name("FaceControls")
                        .with_child(
                            InstanceBuilder::new("NumberPose")
                                .with_property("Value", f64::INFINITY),
                        ),
                ),
            ),
        );
        assert_eq!(decode_authored(clip), Err("nonfinite_property"));
    }

    #[test]
    fn authored_clip_preserves_pose_timing_marker_and_priority() {
        let clip = InstanceBuilder::new("KeyframeSequence")
            .with_property("Loop", false)
            .with_property("Priority", Enum::from_u32(2))
            .with_child(
                InstanceBuilder::new("Keyframe")
                    .with_property("Time", 0.75f32)
                    .with_child(
                        InstanceBuilder::new("Pose")
                            .with_name("Root")
                            .with_property(
                                "CFrame",
                                CFrame::new(
                                    Vector3::new(1.0, 2.0, 3.0),
                                    Matrix3::new(
                                        Vector3::new(0.0, -1.0, 0.0),
                                        Vector3::new(1.0, 0.0, 0.0),
                                        Vector3::new(0.0, 0.0, 1.0),
                                    ),
                                ),
                            ),
                    )
                    .with_child(
                        InstanceBuilder::new("KeyframeMarker")
                            .with_name("Step")
                            .with_property("Value", "left"),
                    ),
            )
            .with_child(InstanceBuilder::new("Keyframe").with_property("Time", 0.0f32))
            .with_child(
                InstanceBuilder::new("AnimationRigData")
                    .with_property("label", BinaryString::from(vec![0, 255, 1])),
            );
        let dom = WeakDom::new(InstanceBuilder::new("DataModel").with_child(clip));
        let mut bytes = Vec::new();
        rbx_binary::to_writer(&mut bytes, &dom, dom.root().children()).unwrap();
        let out = decode(&bytes).unwrap();
        assert_eq!(out["length"], 0.75);
        assert_eq!(out["looped"], false);
        assert_eq!(out["priority"], 2);
        assert_eq!(out["keyframes"][0]["time"], 0.0);
        assert_eq!(
            out["keyframes"][1]["poses"][0]["position"],
            json!([1.0, 2.0, 3.0])
        );
        assert_eq!(
            out["keyframes"][1]["poses"][0]["rotation"],
            json!([0.0, -1.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0])
        );
        assert_eq!(out["keyframes"][1]["markers"][0]["value"], "left");
        assert_eq!(out["rig_data"][0]["fields"]["label"]["bytes_hex"], "00ff01");
        assert_eq!(out["rig_data"][0]["interpreted"], false);
    }

    #[test]
    fn rejects_truncated_or_excessive_chunk_before_dom_decode() {
        assert!(decode(b"not a model").is_err());
        let mut bytes = b"<roblox!\x89\xff\r\n\x1a\n\0\0".to_vec();
        bytes.resize(48, 0);
        bytes[40..44].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(validate_container(&bytes), Err("expansion_limit"));
        bytes[40..44].copy_from_slice(&40u32.to_le_bytes());
        assert_eq!(validate_container(&bytes), Err("truncated_chunk"));
    }
}
