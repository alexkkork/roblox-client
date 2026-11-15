use super::*;

fn rig(lua: &Lua, root: &WorldRef, actor: &WorldRef) -> String {
    lua.load(
        r#"
        local p=Instance.new("Part",workspace);p.Name="Rig";p.CFrame=CFrame.new(10,0,0)
        local a=Instance.new("Bone",p);a.Name="Point";a.CFrame=CFrame.new(1,2,3)
    "#,
    )
    .exec()
    .unwrap();
    actor.borrow_mut().sync_actor_fork(&root.borrow());
    root.borrow_mut().effects.clear();
    actor.borrow_mut().effects.clear();
    root.borrow()
        .nodes
        .iter()
        .find(|(_, n)| n.name == "Point")
        .unwrap()
        .0
        .clone()
}
fn merge(lua: &Lua, root: &WorldRef, actor: &WorldRef) -> MergeReport {
    merge_actor_journal(lua, root, actor, "worker", &actor_journal(actor)).unwrap()
}

#[test]
fn actor_attachment_local_world_and_bone_writes_replay_in_order() {
    let (lua, root, actor_lua, actor) = setup_worlds();
    rig(&lua, &root, &actor);
    actor_lua
        .load(
            r#"
        local a=workspace.Rig.Point
        a.WorldPosition=Vector3.new(20,4,6);a.Orientation=Vector3.new(0,90,0)
        a.WorldAxis=Vector3.new(1,1,0);a.Transform=CFrame.new(0,3,0)
    "#,
        )
        .exec()
        .unwrap();
    let journal = actor_journal(&actor);
    assert_eq!(journal.len(), 4);
    assert!(journal.iter().all(|r| r["kind"] == "attachment_pose"));
    let expected: Vec<f64> = actor_lua
        .load("return {workspace.Rig.Point.WorldCFrame:GetComponents()}")
        .eval()
        .unwrap();
    let report = merge(&lua, &root, &actor);
    assert_eq!(report.applied, 4);
    assert!(report.conflicts.is_empty(), "{:?}", report.conflicts);
    let actual: Vec<f64> = lua
        .load("return {workspace.Rig.Point.WorldCFrame:GetComponents()}")
        .eval()
        .unwrap();
    assert_eq!(actual, expected);
    lua.load("assert(workspace.Rig.Point.Transform.Y==3)")
        .exec()
        .unwrap();
}

#[test]
fn actor_create_clone_and_world_setters_replay_from_original_state() {
    let (lua, root, actor_lua, actor) = setup_worlds();
    actor_lua
        .load(
            r#"
        local p=Instance.new("Part",workspace);p.Name="NewRig";p.CFrame=CFrame.new(10,0,0)
        local a=Instance.new("Attachment",p);a.Name="Point";a.Position=Vector3.new(1,2,3)
        a.WorldPosition=Vector3.new(20,30,40)
        local copy=p:Clone();copy.Name="Copy";copy.Parent=workspace
        copy.Point.WorldPosition=Vector3.new(50,60,70)
    "#,
        )
        .exec()
        .unwrap();
    let report = merge(&lua, &root, &actor);
    assert!(report.conflicts.is_empty(), "{:?}", report.conflicts);
    lua.load("assert(workspace.NewRig.Point.WorldPosition==Vector3.new(20,30,40));assert(workspace.Copy.Point.WorldPosition==Vector3.new(50,60,70))").exec().unwrap();
}

#[test]
fn actor_world_setter_rejects_changed_ancestor_including_physics_feedback() {
    for physics in [false, true] {
        let (lua, root, actor_lua, actor) = setup_worlds();
        rig(&lua, &root, &actor);
        actor_lua
            .load("workspace.Rig.Point.WorldPosition=Vector3.new(50,60,70)")
            .exec()
            .unwrap();
        if physics {
            let parent = root
                .borrow()
                .nodes
                .iter()
                .find(|(_, n)| n.name == "Rig")
                .unwrap()
                .0
                .clone();
            world::set_engine_property(
                &root,
                &parent,
                "CFrame",
                json!({"$cframe":{"position":[90,0,0],"rotation":[1,0,0,0,1,0,0,0,1]}}),
                None,
            )
            .unwrap();
        } else {
            lua.load("workspace.Rig.CFrame=CFrame.new(90,0,0)")
                .exec()
                .unwrap();
        }
        let report = merge(&lua, &root, &actor);
        assert_eq!(report.applied, 0);
        assert_eq!(report.conflicts.len(), 1);
        lua.load("assert(workspace.Rig.Point.Position==Vector3.new(1,2,3));assert(workspace.Rig.Point.WorldPosition==Vector3.new(91,2,3))").exec().unwrap();
    }
}

#[test]
fn actor_local_setter_remains_valid_when_only_ancestor_moves() {
    let (lua, root, actor_lua, actor) = setup_worlds();
    rig(&lua, &root, &actor);
    actor_lua
        .load("workspace.Rig.Point.Position=Vector3.new(4,5,6)")
        .exec()
        .unwrap();
    lua.load("workspace.Rig.CFrame=CFrame.new(90,0,0)")
        .exec()
        .unwrap();
    let report = merge(&lua, &root, &actor);
    assert_eq!(report.applied, 1);
    assert!(report.conflicts.is_empty());
    lua.load("assert(workspace.Rig.Point.WorldPosition==Vector3.new(94,5,6))")
        .exec()
        .unwrap();
}

#[test]
fn actor_stale_target_rejects_complete_pose_and_restores_fork() {
    let (lua, root, actor_lua, actor) = setup_worlds();
    rig(&lua, &root, &actor);
    actor_lua
        .load("workspace.Rig.Point.Position=Vector3.new(4,5,6)")
        .exec()
        .unwrap();
    lua.load("workspace.Rig.Point.CFrame=CFrame.new(8,9,10)")
        .exec()
        .unwrap();
    let report = merge(&lua, &root, &actor);
    assert_eq!(report.applied, 0);
    assert_eq!(report.conflicts.len(), 1);
    for vm in [&lua, &actor_lua] {
        vm.load("assert(workspace.Rig.Point.CFrame==CFrame.new(8,9,10))")
            .exec()
            .unwrap();
    }
}

#[test]
fn mutation_budget_failure_changes_no_attachment_fields() {
    let (lua, root, _, actor) = setup_worlds();
    rig(&lua, &root, &actor);
    root.borrow_mut().effects.insert(
        "mutations".into(),
        json!(vec![json!({"kind":"test-padding"}); 16_384]),
    );
    lua.load(r#"
        local a=workspace.Rig.Point;local before=a.CFrame
        assert(not pcall(function()a.WorldPosition=Vector3.new(80,90,100)end));assert(a.CFrame==before)
    "#).exec().unwrap();
}

#[test]
fn engine_feedback_and_peer_sync_emit_derived_attachment_events() {
    let (lua, root, peer_lua, peer) = setup_worlds();
    rig(&lua, &root, &peer);
    peer_lua.load(r#"getgenv().count=0;workspace.Rig.Point:GetPropertyChangedSignal("WorldPosition"):Connect(function()count+=1 end)"#).exec().unwrap();
    lua.load("workspace.Rig.CFrame=CFrame.new(30,0,0)")
        .exec()
        .unwrap();
    let journal = actor_journal(&root);
    world::sync_actor_fork_with_lua(&peer_lua, &peer, &root, &journal).unwrap();
    scheduler::step(
        &peer_lua,
        &peer_lua
            .app_data_ref::<scheduler::SchedulerRef>()
            .unwrap()
            .clone(),
        Instant::now(),
        Duration::from_millis(30),
    );
    peer_lua
        .load("assert(count==1);assert(workspace.Rig.Point.WorldPosition.X==31)")
        .exec()
        .unwrap();
    let parent = peer
        .borrow()
        .nodes
        .iter()
        .find(|(_, n)| n.name == "Rig")
        .unwrap()
        .0
        .clone();
    world::set_engine_property(
        &peer,
        &parent,
        "CFrame",
        json!({"$cframe":{"position":[40,0,0],"rotation":[1,0,0,0,1,0,0,0,1]}}),
        None,
    )
    .unwrap();
    super::super::super::movement::refresh_attachments(&peer_lua).unwrap();
    scheduler::step(
        &peer_lua,
        &peer_lua
            .app_data_ref::<scheduler::SchedulerRef>()
            .unwrap()
            .clone(),
        Instant::now(),
        Duration::from_millis(30),
    );
    peer_lua
        .load("assert(count==2);assert(workspace.Rig.Point.WorldPosition.X==41)")
        .exec()
        .unwrap();
}
