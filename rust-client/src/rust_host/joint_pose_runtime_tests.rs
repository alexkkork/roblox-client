use super::*;

#[test]
fn actor_joint_commands_publish_resulting_part_events_to_other_running_vms() {
    let (root, world, pool) = setup();
    root.load(
        r#"
        local a=Instance.new("Part",workspace);a.Name="A";a.RootPriority=100
        local b=Instance.new("Part",workspace);b.Name="B";b.CFrame=CFrame.new(4,0,0)
        local j=Instance.new("Motor6D",a);j.Name="Joint";j.C0=CFrame.new(4,0,0);j.Part0=a;j.Part1=b
    "#,
    )
    .exec()
    .unwrap();
    crate::rust_host::movement::flush_joint_poses(&root, &world).unwrap();
    start_source(
        &root,
        &world,
        &pool,
        "other",
        "=reader",
        r#"
        local count=0
        workspace.B:GetPropertyChangedSignal("CFrame"):Connect(function()
            count+=1;workspace.other:SetAttribute("Count",count)
            workspace.other:SetAttribute("X",workspace.B.Position.X)
            workspace.other:SetAttribute("Y",workspace.B.Position.Y)
        end)
    "#,
        Duration::from_secs(1),
    )
    .unwrap();
    start_source(
        &root,
        &world,
        &pool,
        "worker",
        "=writer",
        r#"
        workspace.worker:BindToMessage("move",function()
            workspace.A.Joint.C0=CFrame.new(10,0,0)
            workspace.A.Joint.Transform=CFrame.new(0,2,0)
        end)
    "#,
        Duration::from_secs(1),
    )
    .unwrap();
    advance(&root, &world, &pool);
    // Session::pump drains effects once per frame; mirror that lifecycle here.
    world.borrow_mut().effects.clear();
    root.load("workspace.worker:SendMessage('move')")
        .exec()
        .unwrap();
    for _ in 0..6 {
        crate::rust_host::movement::flush_joint_poses(&root, &world).unwrap();
        let status = advance(&root, &world, &pool);
        for row in status.as_array().unwrap() {
            assert!(
                row["task_errors"].as_array().is_none_or(Vec::is_empty),
                "{row}"
            );
            assert!(
                row.pointer("/world_effects/joint_pose_errors")
                    .and_then(Json::as_array)
                    .is_none_or(Vec::is_empty),
                "{row}"
            );
            assert!(row.get("shadow_merge_error").is_none(), "{row}");
        }
        world.borrow_mut().effects.clear();
    }
    root.load(r#"
        assert(workspace.B.Position.X==10 and workspace.B.Position.Y==2)
        assert(workspace.other:GetAttribute("Count")==2,tostring(workspace.other:GetAttribute("Count")))
        assert(workspace.other:GetAttribute("X")==10 and workspace.other:GetAttribute("Y")==2)
    "#).exec().unwrap();
}
