use super::*;

fn rig(lua: &Lua, root: &WorldRef, actor: &WorldRef) {
    lua.load(
        r#"
        local a=Instance.new("Part",workspace);a.Name="A";a.CFrame=CFrame.new(1,2,3)
        local b=Instance.new("Part",workspace);b.Name="B";b.CFrame=CFrame.new(5,2,3)
        local w=Instance.new("WeldConstraint",a);w.Name="Joint";w.Part0=a;w.Part1=b
        local c=Instance.new("Part",workspace);c.Name="C";c.CFrame=CFrame.new(20,0,0)
    "#,
    )
    .exec()
    .unwrap();
    actor.borrow_mut().sync_actor_fork(&root.borrow());
    root.borrow_mut().effects.clear();
    actor.borrow_mut().effects.clear();
}

fn merge(lua: &Lua, root: &WorldRef, actor: &WorldRef) -> MergeReport {
    merge_actor_journal(lua, root, actor, "worker", &actor_journal(actor)).unwrap()
}

#[test]
fn bulk_budget_failure_preserves_every_component_and_last_pose() {
    let (lua, root, _, actor) = setup_worlds();
    rig(&lua, &root, &actor);
    lua.load(
        "workspace:BulkMoveTo({workspace.A,workspace.C},{CFrame.new(10,0,0),CFrame.new(30,0,0)})",
    )
    .exec()
    .unwrap();
    root.borrow_mut().effects.insert(
        "mutations".into(),
        json!(vec![json!({"kind":"test-padding"}); 16_384]),
    );
    lua.load(r#"
        assert(not pcall(function()workspace:BulkMoveTo({workspace.A,workspace.C},{CFrame.new(100,0,0),CFrame.new(300,0,0)})end))
        assert(workspace.A.Position.X==10 and workspace.B.Position.X==14 and workspace.C.Position.X==30)
    "#).exec().unwrap();
}

#[test]
fn actor_bulk_and_individual_cframe_moves_replay_in_order() {
    let (lua, root, alua, actor) = setup_worlds();
    rig(&lua, &root, &actor);
    alua.load(
        r#"
        workspace:BulkMoveTo({workspace.A,workspace.C},{CFrame.new(10,0,0),CFrame.new(30,0,0)})
        workspace.B.CFrame=CFrame.new(50,0,0)
    "#,
    )
    .exec()
    .unwrap();
    let rows = actor_journal(&actor);
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|r| r["kind"] == "bulk_move"));
    // Unrelated root edits must not spoil later fingerprints in this batch.
    lua.load("workspace.Part.Name='Unrelated'").exec().unwrap();
    let report = merge(&lua, &root, &actor);
    assert_eq!(report.applied, 2, "{:?}", report.conflicts);
    assert!(report.conflicts.is_empty());
    lua.load("assert(workspace.A.Position.X==46 and workspace.B.Position.X==50 and workspace.C.Position.X==30)").exec().unwrap();
}

#[test]
fn actor_bulk_rejects_changed_pose_topology_and_received_sequence_atomically() {
    for kind in ["pose", "topology", "sequence"] {
        let (lua, root, alua, actor) = setup_worlds();
        rig(&lua, &root, &actor);
        alua.load("workspace:BulkMoveTo({workspace.C,workspace.A},{CFrame.new(100,0,0),CFrame.new(200,0,0)})").exec().unwrap();
        match kind {
            "pose" => {
                let id = root
                    .borrow()
                    .nodes
                    .iter()
                    .find(|(_, n)| n.name == "B")
                    .unwrap()
                    .0
                    .clone();
                world::set_engine_property(
                    &root,
                    &id,
                    "CFrame",
                    json!({"$cframe":{"position":[77,2,3],"rotation":[1,0,0,0,1,0,0,0,1]}}),
                    None,
                )
                .unwrap();
            }
            "topology" => lua.load("workspace.A.Joint.Enabled=false").exec().unwrap(),
            _ => root.borrow_mut().sequence += 1,
        }
        let report = merge(&lua, &root, &actor);
        assert_eq!(report.applied, 0, "{kind}");
        assert_eq!(report.conflicts, vec!["bulk_move:stale_input"]);
        for vm in [&lua, &alua] {
            vm.load("assert(workspace.A.CFrame.X==1 and workspace.C.CFrame.X==20)")
                .exec()
                .unwrap();
        }
    }
}

#[test]
fn actor_bulk_create_and_clone_replay_keep_joint_endpoints_local() {
    let (lua, root, alua, actor) = setup_worlds();
    alua.load(
        r#"
        local m=Instance.new("Model",workspace);m.Name="Rig"
        local a=Instance.new("Part",m);a.Name="A";a.CFrame=CFrame.new(2,0,0)
        local b=Instance.new("Part",m);b.Name="B";b.CFrame=CFrame.new(6,0,0)
        local w=Instance.new("Weld",a);w.Part0=a;w.Part1=b
        workspace:BulkMoveTo({a},{CFrame.new(10,0,0)})
        local c=m:Clone();c.Name="Copy";c.Parent=workspace
        workspace:BulkMoveTo({c.B},{CFrame.new(50,0,0)})
    "#,
    )
    .exec()
    .unwrap();
    let report = merge(&lua, &root, &actor);
    assert!(report.conflicts.is_empty(), "{:?}", report.conflicts);
    lua.load(
        r#"
        assert(workspace.Rig.A.CFrame.X==10 and workspace.Rig.B.CFrame.X==14)
        assert(workspace.Copy.A.CFrame.X==46 and workspace.Copy.B.CFrame.X==50)
        assert(workspace.Copy.A.Weld.Part1==workspace.Copy.B)
    "#,
    )
    .exec()
    .unwrap();
}

#[test]
fn actor_bulk_budget_and_foreign_userdata_change_no_parts() {
    let (lua, root, alua, actor) = setup_worlds();
    rig(&lua, &root, &actor);
    let id = root
        .borrow()
        .nodes
        .iter()
        .find(|(_, n)| n.name == "A")
        .unwrap()
        .0
        .clone();
    alua.globals()
        .set("foreign", actor_instance(&alua, &root, &id))
        .unwrap();
    alua.load(r#"
        assert(not pcall(function()workspace:BulkMoveTo({workspace.C,foreign},{CFrame.new(300,0,0),CFrame.new(100,0,0)})end))
        assert(workspace.C.CFrame.X==20)
    "#).exec().unwrap();
    actor.borrow_mut().effects.insert(
        "mutations".into(),
        json!(vec![json!({"kind":"test-padding"}); 4096]),
    );
    alua.load(r#"
        assert(not pcall(function()workspace:BulkMoveTo({workspace.A,workspace.C},{CFrame.new(100,0,0),CFrame.new(300,0,0)})end))
        assert(workspace.A.CFrame.X==1 and workspace.B.CFrame.X==5 and workspace.C.CFrame.X==20)
    "#).exec().unwrap();
}

#[test]
fn peer_sync_preserves_bulk_event_mode_and_joint_notifications() {
    let (lua, root, peer_lua, peer) = setup_worlds();
    rig(&lua, &root, &peer);
    peer_lua
        .load(
            r#"
        getgenv().cf=0;getgenv().pos=0;getgenv().active=0
        workspace.A:GetPropertyChangedSignal("CFrame"):Connect(function()cf+=1 end)
        workspace.A:GetPropertyChangedSignal("Position"):Connect(function()pos+=1 end)
        workspace.A.Joint:GetPropertyChangedSignal("Active"):Connect(function()active+=1 end)
    "#,
        )
        .exec()
        .unwrap();
    lua.load(
        r#"
        workspace:BulkMoveTo({workspace.A},{CFrame.new(50,0,0)},Enum.BulkMoveMode.FireCFrameChanged)
        workspace.A.Joint.Enabled=false
    "#,
    )
    .exec()
    .unwrap();
    world::sync_actor_fork_with_lua(&peer_lua, &peer, &root, &actor_journal(&root)).unwrap();
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
        .load("assert(cf==1 and pos==0 and active==1);assert(workspace.B.Position.X==54)")
        .exec()
        .unwrap();
}
