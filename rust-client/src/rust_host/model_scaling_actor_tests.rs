use super::*;

#[test]
fn actor_scaling_replays_absolute_commands_in_order() {
    let (lua, root, actor_lua, actor) = setup_worlds();
    prepare_pivot_model(&lua, &root, &actor);
    actor_lua
        .load("workspace.Mover:ScaleTo(2);workspace.Mover:ScaleTo(0.5);workspace.Mover:ScaleTo(1)")
        .exec()
        .unwrap();
    let journal = actor_journal(&actor);
    assert_eq!(journal.len(), 3);
    assert!(journal.iter().all(|r| r["kind"] == "model_scale"));
    let report = merge_actor_journal(&lua, &root, &actor, "worker", &journal).unwrap();
    assert!(report.conflicts.is_empty(), "{:?}", report.conflicts);
    assert_eq!(report.applied, 3);
    lua.load("assert(workspace.Mover:GetScale()==1 and workspace.Mover.A.Position.X==-2 and workspace.Mover.B.Position.X==2)").exec().unwrap();
    actor_lua
        .load("assert(workspace.Mover:GetScale()==1 and workspace.Mover.A.Position.X==-2)")
        .exec()
        .unwrap();
    assert!(root.borrow().effects["mutations"]
        .as_array()
        .unwrap()
        .iter()
        .all(|r| r["kind"] == "set" && r["actor_origin"] == "worker"));
}
#[test]
fn stale_actor_scale_does_not_overwrite_any_descendant() {
    let (lua, root, actor_lua, actor) = setup_worlds();
    prepare_pivot_model(&lua, &root, &actor);
    actor_lua.load("workspace.Mover:ScaleTo(2)").exec().unwrap();
    lua.load("workspace.Mover.A.Size=Vector3.new(9,9,9)")
        .exec()
        .unwrap();
    let report =
        merge_actor_journal(&lua, &root, &actor, "worker", &actor_journal(&actor)).unwrap();
    assert_eq!(report.applied, 0);
    assert_eq!(report.conflicts.len(), 1);
    for vm in [&lua, &actor_lua] {
        vm.load("local m=workspace.Mover;assert(m:GetScale()==1 and m.A.Size.X==9 and m.A.Position.X==-2 and m.B.Position.X==2)").exec().unwrap();
    }
}
#[test]
fn new_descendant_or_received_sequence_invalidates_scale_read_set() {
    for received in [false, true] {
        let (lua, root, actor_lua, actor) = setup_worlds();
        prepare_pivot_model(&lua, &root, &actor);
        actor_lua.load("workspace.Mover:ScaleTo(2)").exec().unwrap();
        if received {
            root.borrow_mut().sequence += 1;
        } else {
            lua.load("Instance.new('Part',workspace.Mover)")
                .exec()
                .unwrap();
        }
        let report =
            merge_actor_journal(&lua, &root, &actor, "worker", &actor_journal(&actor)).unwrap();
        assert_eq!(report.applied, 0);
        assert_eq!(report.conflicts.len(), 1);
        lua.load("assert(workspace.Mover:GetScale()==1 and workspace.Mover.A.Position.X==-2)")
            .exec()
            .unwrap();
    }
}
#[test]
fn actor_clone_transfers_original_baseline_before_later_root_scaling() {
    let (lua, root, actor_lua, actor) = setup_worlds();
    prepare_pivot_model(&lua, &root, &actor);
    lua.load("local m=workspace.Mover;m.A.CFrame=CFrame.new(.137,.281,.319);original=m.A.CFrame;m:ScaleTo(3.79)").exec().unwrap();
    actor.borrow_mut().sync_actor_fork(&root.borrow());
    root.borrow_mut().effects.clear();
    actor.borrow_mut().effects.clear();
    actor_lua
        .load("local c=workspace.Mover:Clone();c.Name='Copy';c.Parent=workspace")
        .exec()
        .unwrap();
    let report =
        merge_actor_journal(&lua, &root, &actor, "worker", &actor_journal(&actor)).unwrap();
    assert!(report.conflicts.is_empty(), "{:?}", report.conflicts);
    lua.load("local c=workspace.Copy;assert(c:GetScale()==3.79);c:ScaleTo(1);assert(c.A.CFrame==original)").exec().unwrap();
}
#[test]
fn actor_create_scale_clone_and_scale_are_replayed_from_initial_state() {
    let (lua, root, actor_lua, actor) = setup_worlds();
    actor_lua.load(r#"
        local m=Instance.new("Model",workspace);m.Name="Scaled";m.WorldPivot=CFrame.new()
        local p=Instance.new("Part",m);p.Name="P";p.Size=Vector3.new(2,3,4);p.CFrame=CFrame.new(.137,.281,.319)
        m:ScaleTo(3.79);local copy=m:Clone();copy.Name="Copy";copy.Parent=workspace;copy:ScaleTo(1)
    "#).exec().unwrap();
    let report =
        merge_actor_journal(&lua, &root, &actor, "worker", &actor_journal(&actor)).unwrap();
    assert!(report.conflicts.is_empty(), "{:?}", report.conflicts);
    lua.load("assert(workspace.Scaled:GetScale()==3.79 and workspace.Copy:GetScale()==1);assert(workspace.Copy.P.CFrame==CFrame.new(.137,.281,.319));assert(workspace.Copy.P.Size==Vector3.new(2,3,4))").exec().unwrap();
}
#[test]
fn exhausted_mutation_budget_leaves_scale_geometry_and_cache_untouched() {
    let (lua, root, _, actor) = setup_worlds();
    prepare_pivot_model(&lua, &root, &actor);
    root.borrow_mut().effects.insert(
        "mutations".into(),
        Json::Array(vec![json!({"kind":"test"}); 16384]),
    );
    lua.load("local m=workspace.Mover;assert(not pcall(function()m:ScaleTo(2)end));assert(m:GetScale()==1 and m.A.Position.X==-2 and m.B.Position.X==2)").exec().unwrap();
    root.borrow_mut().effects.clear();
    lua.load("local m=workspace.Mover;m:ScaleTo(2);m:ScaleTo(1);assert(m.A.Position.X==-2 and m.B.Position.X==2)").exec().unwrap();
}

#[test]
fn engine_feedback_without_a_local_revision_still_rejects_stale_scaling() {
    let (lua, root, actor_lua, actor) = setup_worlds();
    prepare_pivot_model(&lua, &root, &actor);
    actor_lua.load("workspace.Mover:ScaleTo(2)").exec().unwrap();
    let id = root
        .borrow()
        .nodes
        .values()
        .find(|n| n.name == "A")
        .unwrap()
        .id
        .clone();
    world::set_engine_property(
        &root,
        &id,
        "CFrame",
        json!({"$cframe":{"position":[99,0,0],"rotation":[1,0,0,0,1,0,0,0,1]}}),
        None,
    )
    .unwrap();
    let report =
        merge_actor_journal(&lua, &root, &actor, "worker", &actor_journal(&actor)).unwrap();
    assert_eq!(report.applied, 0);
    assert_eq!(report.conflicts.len(), 1);
    lua.load("local m=workspace.Mover;assert(m:GetScale()==1 and m.A.CFrame.X==99 and m.B.Position.X==2)").exec().unwrap();
}

#[test]
fn actor_clone_checks_journal_budget_before_inserting_nodes_or_caches() {
    let (lua, root, actor_lua, actor) = setup_worlds();
    prepare_pivot_model(&lua, &root, &actor);
    lua.load("workspace.Mover:ScaleTo(3.79)").exec().unwrap();
    actor.borrow_mut().sync_actor_fork(&root.borrow());
    actor.borrow_mut().effects.insert(
        "mutations".into(),
        Json::Array(vec![json!({"kind":"test"}); 4095]),
    );
    let count = actor.borrow().nodes.len();
    actor_lua.load("assert(not pcall(function()workspace.Mover:Clone()end));assert(workspace.Mover:GetScale()==3.79)").exec().unwrap();
    assert_eq!(actor.borrow().nodes.len(), count);
}
