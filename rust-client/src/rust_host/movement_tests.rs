use super::{apply_engine_owned_parts, apply_engine_poses, install};
use crate::rust_host::{
    cframe, enums, numeric, signals,
    world::{self, World, WorldRef},
};
use mlua::Lua;
use serde_json::json;
use std::{cell::RefCell, rc::Rc};

fn fixture() -> WorldRef {
    let snapshot = json!({"version":1,"sequence":10,"local_player":"player",
    "services":{"Workspace":"ws","Players":"players"},"nodes":[
        {"id":"game","class_name":"DataModel","name":"game","parent":null,"properties":{}},
        {"id":"ws","class_name":"Workspace","name":"Workspace","parent":"game","properties":{"Gravity":196.2}},
        {"id":"players","class_name":"Players","name":"Players","parent":"game","properties":{}},
        {"id":"player","class_name":"Player","name":"LocalPlayer","parent":"players","properties":{"Character":{"$ref":"char"}}},
        {"id":"char","class_name":"Model","name":"Character","parent":"ws","properties":{"PrimaryPart":{"$ref":"root"}}},
        {"id":"hum","class_name":"Humanoid","name":"Humanoid","parent":"char","properties":{"Health":100,"MaxHealth":100}},
        {"id":"root","class_name":"Part","name":"HumanoidRootPart","parent":"char","property_revisions":{"CFrame":10,"Anchored":10,"NetworkOwnerV3":10},"properties":{"Anchored":false,"NetworkOwnerV3":7,"CFrame":{"$cframe":{"position":[0,3,0],"rotation":[1,0,0,0,1,0,0,0,1]}},"Position":{"$vector3":[0,3,0]},"AssemblyLinearVelocity":{"$vector3":[0,0,0]},"AssemblyAngularVelocity":{"$vector3":[0,0,0]}}},
        {"id":"part","class_name":"Part","name":"OtherPart","parent":"char","properties":{"Anchored":false,"CFrame":{"$cframe":{"position":[2,3,0],"rotation":[1,0,0,0,1,0,0,0,1]}},"Position":{"$vector3":[2,3,0]}}},
        {"id":"loose","class_name":"Part","name":"Loose","parent":"ws","property_revisions":{"CFrame":11,"Anchored":10,"NetworkOwnerV3":10},"properties":{"Anchored":false,"NetworkOwnerV3":7,"CFrame":{"$cframe":{"position":[30,0,0],"rotation":[1,0,0,0,1,0,0,0,1]}},"Position":{"$vector3":[30,0,0]}}}
    ]});
    Rc::new(RefCell::new(World::from_snapshot(&snapshot, None).unwrap()))
}

fn runtime() -> (Lua, WorldRef) {
    let lua = Lua::new();
    let world = fixture();
    signals::install(&lua, &world).unwrap();
    world::install(&lua, &world).unwrap();
    numeric::install(&lua).unwrap();
    enums::install(&lua).unwrap();
    cframe::install(&lua).unwrap();
    install(&lua, &world).unwrap();
    (lua, world)
}

#[test]
fn humanoid_move_and_move_to_preserve_local_intent_shapes() {
    let (lua, world) = runtime();
    let mutation_cursor = world.borrow().effects["mutations"]
        .as_array()
        .unwrap()
        .len();
    lua.load("local p=game:GetService('Players'):FindFirstChild('LocalPlayer'); local h=p.Character:FindFirstChildOfClass('Humanoid'); h:Move(Vector3.new(1,0,0)); h:MoveTo(Vector3.new(4,0,2))").exec().unwrap();
    let intents = world.borrow().effects["movement_intents"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(
        intents[0],
        json!({"kind":"Move","instance_id":"hum","vector":[1.,0.,0.],"relative_to_camera":false,"sequence":1,"mutation_cursor":mutation_cursor})
    );
    assert_eq!(
        intents[1],
        json!({"kind":"MoveTo","instance_id":"hum","vector":[4.,0.,2.],"sequence":2,"mutation_cursor":mutation_cursor})
    );
}

#[test]
fn humanoid_state_methods_use_feedback_and_queue_local_state_intents() {
    let (lua, world) = runtime();
    world::set_engine_property(&world, "hum", "__HumanoidStateFeedback", json!(5), None).unwrap();
    lua.load("local h=game:GetService('Players'):FindFirstChild('LocalPlayer').Character:FindFirstChildOfClass('Humanoid'); local state=h:GetState(); assert(state.Name=='Freefall' and state.Value==5); assert(h:GetStateEnabled(Enum.HumanoidStateType.Jumping)); h:ChangeState(Enum.HumanoidStateType.Jumping); h:SetStateEnabled(Enum.HumanoidStateType.Swimming,false); assert(not h:GetStateEnabled(Enum.HumanoidStateType.Swimming))").exec().unwrap();
    let intents = world.borrow().effects["movement_intents"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(intents[0]["kind"], json!("ChangeState"));
    assert_eq!(intents[0]["state_value"], json!(3));
    assert_eq!(intents[1]["kind"], json!("SetStateEnabled"));
    assert_eq!(intents[1]["enabled"], json!(false));
}

#[test]
fn translate_by_and_pivot_to_write_local_descendant_transforms() {
    let (lua, world) = runtime();
    let bootstrap_mutations = world.borrow().effects["mutations"]
        .as_array()
        .unwrap()
        .len();
    lua.load("local m=game:GetService('Players'):FindFirstChild('LocalPlayer').Character; m:TranslateBy(Vector3.new(5,0,-1)); m:PivotTo(CFrame.new(20,0,0))").exec().unwrap();
    let root = world::get_property(&world, "root", "CFrame").unwrap();
    assert_eq!(root["$cframe"]["position"], json!([20., 0., 0.]));
    let part = world::get_property(&world, "part", "CFrame").unwrap();
    assert_eq!(part["$cframe"]["position"], json!([22., 0., 0.]));
    assert_eq!(
        world.borrow().effects["mutations"]
            .as_array()
            .unwrap()
            .len(),
        bootstrap_mutations + 16
    );
}

#[test]
fn gravity_and_anchored_are_local_property_mutations() {
    let (lua, world) = runtime();
    let bootstrap_mutations = world.borrow().effects["mutations"]
        .as_array()
        .unwrap()
        .len();
    lua.load("workspace.Gravity=80; game:GetService('Players'):FindFirstChild('LocalPlayer').Character:FindFirstChild('HumanoidRootPart').Anchored=true").exec().unwrap();
    assert_eq!(
        world::get_property(&world, "ws", "Gravity").unwrap(),
        json!(80.0)
    );
    assert_eq!(
        world::get_property(&world, "root", "Anchored").unwrap(),
        json!(true)
    );
    assert_eq!(
        world.borrow().effects["mutations"]
            .as_array()
            .unwrap()
            .len(),
        bootstrap_mutations + 2
    );
}

#[test]
fn engine_pose_requires_current_character_and_all_writes_validate_before_commit() {
    let world = fixture();
    let frame = json!({"$cframe":{"position":[8.,9.,10.],"rotation":[1.,0.,0.,0.,1.,0.,0.,0.,1.]}});
    let stale = json!({"character_id":"other","root_id":"root","frames":[{"id":"root","cframe":frame.clone()}]});
    assert_eq!(
        apply_engine_poses(&world, &stale).unwrap(),
        json!({"status":"stale_character","frames_applied":0})
    );
    let malformed = json!({"character_id":"char","root_id":"root","frames":[{"id":"root","cframe":frame.clone()},{"id":"missing","cframe":frame.clone()}]});
    assert!(apply_engine_poses(&world, &malformed).is_err());
    assert_eq!(
        world::get_property(&world, "root", "CFrame").unwrap()["$cframe"]["position"],
        json!([0, 3, 0])
    );
}

#[test]
fn owned_part_feedback_rejects_stale_property_revisions() {
    let world = fixture();
    let batch = json!({"scope":7,"source_revision":10,"frames":[{"id":"loose","cframe":{"$cframe":{"position":[8.,9.,10.],"rotation":[1.,0.,0.,0.,1.,0.,0.,0.,1.]}},"linear_velocity":[0.,0.,0.],"angular_velocity":[0.,0.,0.]}]});
    assert_eq!(
        apply_engine_owned_parts(&world, &batch, 7).unwrap(),
        json!({"status":"stale_authority","frames_applied":0})
    );
    assert_eq!(
        world::get_property(&world, "loose", "CFrame").unwrap()["$cframe"]["position"],
        json!([30, 0, 0])
    );
}
