//! End-to-end checks for the Rust-owned Instance attribute store through the
//! production Rust runtime and official Luau VM bridge.
use headless_luau::runtime_owner::{successful, Runtime};
use serde_json::{json, Value};

fn snapshot() -> Value {
    serde_json::from_str(include_str!(
        "../../tools/fixtures/rust-rewrite/compact/snapshot.json"
    ))
    .unwrap()
}

fn runtime_with_attributes(attributes: Value, known: bool) -> Runtime {
    let mut world = snapshot();
    let root = world["nodes"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|node| node["id"] == "scope:root-1")
        .unwrap();
    root["attributes"] = attributes;
    root["attributes_known"] = json!(known);
    Runtime::new(world, 32).unwrap()
}

fn exec(runtime: &mut Runtime, source: &str) -> Value {
    let result = runtime
        .call(json!({"op":"execute","source":source,"timeout_ms":500}))
        .unwrap();
    successful(&result).unwrap_or_else(|error| panic!("Luau failed: {error}; result={result}"));
    result
}

fn update(runtime: &mut Runtime, sequence: u64, actions: Value) -> Value {
    runtime
        .call(json!({"op":"update","timeout_ms":500,"delta":{
            "version":1,"delta":true,"sequence":sequence,"actions":actions
        }}))
        .unwrap()
}

#[test]
fn known_and_unknown_attribute_reads_keep_their_distinct_luau_results() {
    let mut known = runtime_with_attributes(json!({"Present":false}), true);
    exec(
        &mut known,
        r#"
        local p=game:GetService("Players").LocalPlayer.Character.HumanoidRootPart
        assert(p:GetAttribute("Present")==false)
        assert(p:GetAttribute("Missing")==nil)
        assert(p:GetAttributes().Present==false)
    "#,
    );

    let mut unknown = runtime_with_attributes(json!({"Present":false}), false);
    exec(
        &mut unknown,
        r#"
        local p=game:GetService("Players").LocalPlayer.Character.HumanoidRootPart
        assert(p:GetAttribute("Present")==false)
        assert(not pcall(function() p:GetAttribute("Missing") end))
        assert(not pcall(function() p:GetAttributes() end))
    "#,
    );
}

#[test]
fn received_null_and_unknown_updates_preserve_explicit_presence_and_signal_rules() {
    let mut runtime = runtime_with_attributes(json!({}), false);
    exec(
        &mut runtime,
        r#"
        p=game:GetService("Players").LocalPlayer.Character.HumanoidRootPart
        changes=0
        p:GetAttributeChangedSignal("RBX_probe"):Connect(function() changes+=1 end)
    "#,
    );

    let first = update(
        &mut runtime,
        8,
        json!([{
            "sequence":8,"kind":"attribute","id":"scope:root-1",
            "attribute_update":{"name":"RBX_probe","known":true,"value":null}
        }]),
    );
    successful(&first).unwrap();
    // Explicit null is present in an unknown map, so reads return nil while
    // the map stays unavailable and a repeated null has no change event.
    exec(
        &mut runtime,
        r#"
        assert(p:GetAttribute("RBX_probe")==nil)
        assert(not pcall(function() p:GetAttribute("missing") end))
    "#,
    );
    let second = update(
        &mut runtime,
        9,
        json!([{
            "sequence":9,"kind":"attribute","id":"scope:root-1",
            "attribute_update":{"name":"RBX_probe","known":true,"value":null}
        }]),
    );
    successful(&second).unwrap();
    let third = update(
        &mut runtime,
        10,
        json!([{
            "sequence":10,"kind":"attribute","id":"scope:root-1",
            "attribute_update":{"name":"RBX_probe","known":false}
        }]),
    );
    successful(&third).unwrap();
    exec(
        &mut runtime,
        r#"
        assert(changes==2)
        assert(not pcall(function() return p:GetAttribute("RBX_probe") end))
    "#,
    );

    let fourth = update(
        &mut runtime,
        11,
        json!([{
            "sequence":11,"kind":"set","id":"scope:root-1","fields":{},
            "attribute_snapshot":{"attributes":{},"attributes_known":true}
        }]),
    );
    successful(&fourth).unwrap();
    exec(
        &mut runtime,
        r#"
        assert(changes==2)
        assert(p:GetAttribute("RBX_probe")==nil)
        assert(next(p:GetAttributes())==nil)
    "#,
    );
}

#[test]
fn snapshot_replacement_emits_the_sorted_union_of_changed_names() {
    let mut runtime = runtime_with_attributes(json!({"b":1,"keep":4}), true);
    exec(
        &mut runtime,
        r#"
        p=game:GetService("Players").LocalPlayer.Character.HumanoidRootPart
        order={}
        for _,name in {"a","b","keep","z"} do
            p:GetAttributeChangedSignal(name):Connect(function() table.insert(order,name) end)
        end
    "#,
    );
    let result = update(
        &mut runtime,
        8,
        json!([{
            "sequence":8,"kind":"set","id":"scope:root-1","fields":{},
            "attribute_snapshot":{"attributes":{"a":2,"keep":4,"z":9},"attributes_known":true}
        }]),
    );
    successful(&result).unwrap();
    exec(
        &mut runtime,
        r#"
        assert(#order==3)
        assert(order[1]=="a" and order[2]=="b" and order[3]=="z")
        assert(p:GetAttribute("a")==2 and p:GetAttribute("b")==nil)
    "#,
    );
}

#[test]
fn received_engine_names_are_accepted_but_local_reserved_names_are_rejected() {
    let mut runtime = runtime_with_attributes(json!({}), true);
    exec(
        &mut runtime,
        r#"
        p=game:GetService("Players").LocalPlayer.Character.HumanoidRootPart
        assert(not pcall(function() p:SetAttribute("RBX_probe", 1) end))
    "#,
    );
    let result = update(
        &mut runtime,
        8,
        json!([{
            "sequence":8,"kind":"attribute","id":"scope:root-1",
            "attribute_update":{"name":"RBX_probe","known":true,"value":7}
        }]),
    );
    successful(&result).unwrap();
    exec(&mut runtime, r#"assert(p:GetAttribute("RBX_probe")==7)"#);
}

#[test]
fn clone_attribute_instance_references_are_remapped_and_mutations_are_isolated() {
    let mut runtime = runtime_with_attributes(json!({}), true);
    exec(
        &mut runtime,
        r#"
        local source=Instance.new("Folder",workspace)
        source.Name="AttributeCloneSource"
        local target=Instance.new("Part",source)
        target.Name="Target"
        source:SetAttribute("TargetRef",target)
        local copy=source:Clone()
        assert(copy.Target==copy:GetAttribute("TargetRef"):Get())
        copy:SetAttribute("OnlyCopy",true)
        assert(source:GetAttribute("OnlyCopy")==nil)
        source:SetAttribute("OnlySource",true)
        assert(copy:GetAttribute("OnlySource")==nil)
    "#,
    );
}

#[test]
fn initial_large_maps_survive_but_oversized_received_replacements_are_rejected() {
    let mut attributes = serde_json::Map::new();
    for index in 0..1025 {
        attributes.insert(format!("key-{index}"), json!(index));
    }
    let mut runtime = runtime_with_attributes(Value::Object(attributes.clone()), true);
    exec(
        &mut runtime,
        r#"
        local p=game:GetService("Players").LocalPlayer.Character.HumanoidRootPart
        assert(p:GetAttribute("key-1024")==1024)
    "#,
    );

    attributes.insert("key-1024".into(), json!(999999));

    let result = update(
        &mut runtime,
        8,
        json!([{
            "sequence":8,"kind":"set","id":"scope:root-1","fields":{},
            "attribute_snapshot":{"attributes":attributes,"attributes_known":true}
        }]),
    );
    assert_eq!(
        result["ok"], false,
        "oversized received replacement should fail: {result}"
    );
    exec(
        &mut runtime,
        r#"
        local p=game:GetService("Players").LocalPlayer.Character.HumanoidRootPart
        assert(p:GetAttribute("key-1024")==1024)
    "#,
    );
}

#[test]
fn integral_and_float_attribute_numbers_compare_like_the_native_store() {
    let mut runtime = runtime_with_attributes(json!({"Count":1}), true);
    exec(
        &mut runtime,
        r#"
        p=game:GetService("Players").LocalPlayer.Character.HumanoidRootPart
        changes=0
        p:GetAttributeChangedSignal("Count"):Connect(function() changes+=1 end)
        p:SetAttribute("Count",1.0)
        assert(changes==0)
    "#,
    );
    let result = update(
        &mut runtime,
        8,
        json!([{
            "sequence":8,"kind":"attribute","id":"scope:root-1",
            "attribute_update":{"name":"Count","known":true,"value":1.0}
        }]),
    );
    successful(&result).unwrap();
    exec(
        &mut runtime,
        r#"
        assert(changes==0)
        p:SetAttribute("Count",2)
    "#,
    );
    // Instance signals run after the submitting script yields or returns.
    exec(&mut runtime, "assert(changes==1)");
}

fn actor_attribute_world() -> Value {
    let mut world = snapshot();
    let root = world["nodes"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|node| node["id"] == "scope:root-1")
        .unwrap();
    root["attributes"] = json!({"RootSeed":73});
    root["attributes_known"] = json!(true);
    world["nodes"].as_array_mut().unwrap().push(json!({
        "id":"attribute-unknown","class_name":"Folder","name":"AttributeUnknownProbe",
        "parent":"scope:workspace","properties":{},"attributes":{"NullMarker":null},
        "attributes_known":false
    }));
    world["nodes"].as_array_mut().unwrap().extend([
        json!({"id":"scope:attribute-actor","class_name":"Actor","name":"AttributeActor",
            "parent":"scope:workspace","properties":{}}),
        json!({"id":"scope:attribute-reader","class_name":"LocalScript","name":"AttributeReader",
            "parent":"scope:attribute-actor","properties":{"Disabled":false,"Enabled":true},
            "attributes":{},"attributes_known":true}),
    ]);
    world["class_parents"]["Actor"] = json!("Instance");
    world["class_parents"]["LocalScript"] = json!("LuaSourceContainer");
    world["class_parents"]["LuaSourceContainer"] = json!("Instance");
    world["script_sources"] = json!({"scope:attribute-reader":{"source":r#"
        local root=workspace.FixtureCharacter.HumanoidRootPart
        script:SetAttribute("initialValue",root:GetAttribute("RootSeed"))
        local probe=workspace:FindFirstChild("AttributeUnknownProbe")
        local nullOk,nullValue=pcall(function() return probe:GetAttribute("NullMarker") end)
        local missingOk=pcall(function() return probe:GetAttribute("Missing") end)
        script:SetAttribute("nullRead",nullOk and nullValue==nil)
        script:SetAttribute("unknownMissingRejected",not missingOk)
        while script:GetAttribute("Advance")~=true do task.wait() end
        script:SetAttribute("afterReplay",root:GetAttribute("RootSeed"))
        task.wait(20)
    "#}});
    world
}

#[test]
fn actor_forks_root_attribute_stores_and_observes_later_root_mutations() {
    use std::time::{Duration, Instant};

    let mut runtime = Runtime::new(actor_attribute_world(), 32).unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut initial = Value::Null;
    let mut last_response = Value::Null;
    while Instant::now() < deadline {
        let response = exec(
            &mut runtime,
            "return workspace.AttributeActor.AttributeReader:GetAttribute('initialValue'), workspace.AttributeActor.AttributeReader:GetAttribute('nullRead'), workspace.AttributeActor.AttributeReader:GetAttribute('unknownMissingRejected')",
        );
        initial = response["values"].clone();
        last_response = response;
        if initial == json!([73.0, true, true]) {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        initial,
        json!([73.0, true, true]),
        "last response: {last_response}"
    );

    exec(
        &mut runtime,
        "local root=workspace.FixtureCharacter.HumanoidRootPart; root:SetAttribute('RootSeed',99); workspace.AttributeActor.AttributeReader:SetAttribute('Advance',true)",
    );
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut after_replay = Value::Null;
    while Instant::now() < deadline {
        let response = exec(
            &mut runtime,
            "return workspace.FixtureCharacter.HumanoidRootPart:GetAttribute('RootSeed'), workspace.AttributeActor.AttributeReader:GetAttribute('afterReplay')",
        );
        after_replay = response["values"].clone();
        if after_replay == json!([99.0, 99.0]) {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(after_replay, json!([99.0, 99.0]));
}
