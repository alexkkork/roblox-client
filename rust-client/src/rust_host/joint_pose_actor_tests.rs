use super::*;
use crate::rust_host::movement;

fn rig(lua: &Lua, root: &WorldRef, actor: &WorldRef) {
    lua.load(r#"
        local a=Instance.new("Part",workspace);a.Name="A";a.RootPriority=100
        local b=Instance.new("Part",workspace);b.Name="B";b.CFrame=CFrame.new(4,0,0)
        local c=Instance.new("Part",workspace);c.Name="C";c.CFrame=CFrame.new(8,0,0)
        local ab=Instance.new("Motor6D",a);ab.Name="AB";ab.Part0=a;ab.Part1=b;ab.C0=CFrame.new(4,0,0)
        local bc=Instance.new("Motor6D",b);bc.Name="BC";bc.Part0=b;bc.Part1=c;bc.C0=CFrame.new(4,0,0)
    "#).exec().unwrap();
    movement::flush_joint_poses(lua, root).unwrap();
    actor.borrow_mut().sync_actor_fork(&root.borrow());
    root.borrow_mut().effects.clear();
    actor.borrow_mut().effects.clear();
}
fn merge(lua: &Lua, root: &WorldRef, actor: &WorldRef) -> MergeReport {
    merge_actor_journal(lua, root, actor, "worker", &actor_journal(actor)).unwrap()
}

#[test]
fn actor_joint_offsets_and_rebases_replay_in_order() {
    let (lua, root, alua, actor) = setup_worlds();
    rig(&lua, &root, &actor);
    alua.load(
        r#"
        workspace.A.AB.C0=CFrame.new(6,0,0)
        workspace.B.Position=Vector3.new(7,2,0)
        workspace.B.BC.C1=CFrame.new(1,0,0)
    "#,
    )
    .exec()
    .unwrap();
    let report = merge(&lua, &root, &actor);
    assert_eq!(report.applied, 3, "{report:?}");
    assert!(report.conflicts.is_empty(), "{report:?}");
    for vm in [&lua, &alua] {
        vm.load("assert(workspace.A.Position.X==0 and workspace.B.Position.X==7 and workspace.C.Position.X==9)").exec().unwrap();
    }
}

#[test]
fn actor_motor_checkpoint_uses_new_root_pose_and_restores_actor_view() {
    let (lua, root, alua, actor) = setup_worlds();
    rig(&lua, &root, &actor);
    alua.load("workspace.A.AB.Transform=CFrame.new(0,2,0)")
        .exec()
        .unwrap();
    movement::flush_joint_poses(&alua, &actor).unwrap();
    lua.load("workspace.A.CFrame=CFrame.new(20,0,0)")
        .exec()
        .unwrap();
    let report = merge(&lua, &root, &actor);
    assert_eq!(report.applied, 2, "{report:?}");
    assert!(report.conflicts.is_empty(), "{report:?}");
    for vm in [&lua, &alua] {
        vm.load("assert(workspace.A.Position.X==20 and workspace.B.Position.X==24 and workspace.B.Position.Y==2 and workspace.C.Position.X==28 and workspace.C.Position.Y==2)").exec().unwrap();
    }
    movement::flush_joint_poses(&lua, &root).unwrap();
    lua.load("assert(workspace.C.Position.X==28 and workspace.C.Position.Y==2)")
        .exec()
        .unwrap();
}

#[test]
fn actor_joint_offset_conflicts_restore_entire_speculative_branch() {
    for conflict in ["pose", "priority", "offset", "sequence"] {
        let (lua, root, alua, actor) = setup_worlds();
        rig(&lua, &root, &actor);
        alua.load("workspace.A.AB.C0=CFrame.new(20,0,0)")
            .exec()
            .unwrap();
        match conflict {
            "pose" => lua
                .load("workspace.A.CFrame=CFrame.new(2,0,0)")
                .exec()
                .unwrap(),
            "priority" => lua.load("workspace.C.RootPriority=127").exec().unwrap(),
            "offset" => lua
                .load("workspace.B.BC.C0=CFrame.new(8,0,0)")
                .exec()
                .unwrap(),
            _ => root.borrow_mut().sequence += 1,
        }
        let report = merge(&lua, &root, &actor);
        assert_eq!(report.applied, 0, "{conflict}: {report:?}");
        assert_eq!(report.conflicts, vec!["joint_offset:stale_input"]);
        for name in ["A", "B", "C"] {
            let source = format!("return workspace.{name}.CFrame.X");
            let expected: f64 = lua.load(&source).eval().unwrap();
            assert_eq!(
                alua.load(&source).eval::<f64>().unwrap(),
                expected,
                "{conflict}: {name}"
            );
        }
        alua.load("assert(workspace.A.AB.C0.X==4)").exec().unwrap();
    }
}

#[test]
fn actor_rewired_motor_rejects_target_and_checkpoint() {
    let (lua, root, alua, actor) = setup_worlds();
    rig(&lua, &root, &actor);
    alua.load("workspace.A.AB.Transform=CFrame.new(0,5,0)")
        .exec()
        .unwrap();
    movement::flush_joint_poses(&alua, &actor).unwrap();
    lua.load("workspace.A.AB.Part1=workspace.C").exec().unwrap();
    let report = merge(&lua, &root, &actor);
    assert_eq!(report.applied, 0, "{report:?}");
    assert_eq!(report.conflicts.len(), 2);
    for vm in [&lua, &alua] {
        vm.load("assert(workspace.B.Position.Y==0 and workspace.C.Position.Y==0 and workspace.A.AB.Transform.Y==0)").exec().unwrap();
    }
}

#[test]
fn joint_budget_failures_do_not_change_offsets_parts_or_pending_targets() {
    for actor_mode in [false, true] {
        let (lua, root, alua, actor) = setup_worlds();
        rig(&lua, &root, &actor);
        let (vm, world, limit) = if actor_mode {
            (&alua, &actor, 4096)
        } else {
            (&lua, &root, 16384)
        };
        world.borrow_mut().effects.insert(
            "mutations".into(),
            json!(vec![json!({"kind":"test-padding"}); limit]),
        );
        vm.load(
            r#"
            assert(not pcall(function()workspace.A.AB.C0=CFrame.new(20,0,0)end))
            assert(not pcall(function()workspace.A.AB.Transform=CFrame.new(0,5,0)end))
            assert(workspace.A.AB.C0.X==4 and workspace.A.AB.Transform.Y==0)
            assert(workspace.B.Position.X==4 and workspace.C.Position.X==8)
        "#,
        )
        .exec()
        .unwrap();
        world.borrow_mut().effects.clear();
        movement::flush_joint_poses(vm, world).unwrap();
        vm.load("assert(workspace.B.Position.Y==0)").exec().unwrap();
    }
}

#[test]
fn failed_deferred_transaction_keeps_last_applied_pose_and_can_retry() {
    let (lua, root, _, actor) = setup_worlds();
    rig(&lua, &root, &actor);
    lua.load("workspace.A.AB.Transform=CFrame.new(0,5,0)")
        .exec()
        .unwrap();
    root.borrow_mut().effects.insert(
        "mutations".into(),
        json!(vec![json!({"kind":"test-padding"}); 16384]),
    );
    movement::flush_joint_poses(&lua, &root).unwrap();
    assert_eq!(
        root.borrow().effects["joint_pose_errors"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    lua.load("assert(workspace.A.AB.Transform.Y==5 and workspace.B.Position.Y==0)")
        .exec()
        .unwrap();
    root.borrow_mut().effects.clear();
    lua.load("workspace.A.AB.C0=CFrame.new(6,0,0);assert(workspace.B.Position.X==6 and workspace.B.Position.Y==0);workspace.A.AB.Transform=workspace.A.AB.Transform").exec().unwrap();
    movement::flush_joint_poses(&lua, &root).unwrap();
    lua.load("assert(workspace.B.Position.X==6 and workspace.B.Position.Y==5 and workspace.C.Position.X==10 and workspace.C.Position.Y==5)").exec().unwrap();
}

#[test]
fn unrelated_receipt_does_not_claim_pending_local_registration_or_motor_work() {
    let (lua, world, _, _) = setup_worlds();
    lua.load(
        r#"
        local a=Instance.new("Part",workspace);a.Name="A";a.RootPriority=100
        local b=Instance.new("Part",workspace);b.Name="B";b.CFrame=CFrame.new(100,0,0)
        local j=Instance.new("Motor6D",a);j.C0=CFrame.new(7,0,0);j.Transform=CFrame.new(0,2,0)
        j.Part0=a;j.Part1=b
    "#,
    )
    .exec()
    .unwrap();
    world.borrow_mut().effects.clear();
    world::apply_delta_with_lua(&lua,&world,&json!({"version":1,"delta":true,"sequence":1,"actions":[
        {"sequence":1,"kind":"create","id":"unrelated","node":{"id":"unrelated","class_name":"Folder","name":"Unrelated","parent":"workspace","properties":{}}}
    ]}),None).unwrap();
    lua.load("assert(workspace.B.Position.X==100 and workspace.B.Position.Y==0)")
        .exec()
        .unwrap();
    movement::flush_joint_poses(&lua, &world).unwrap();
    lua.load("assert(workspace.B.Position.X==7 and workspace.B.Position.Y==2);assert(client.propertyOrigin(workspace.B,'CFrame')=='local')").exec().unwrap();
    assert!(!actor_journal(&world).is_empty());
}
