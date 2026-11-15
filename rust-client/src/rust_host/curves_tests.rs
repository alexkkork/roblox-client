use super::*;
use crate::rust_host::{self, actor_shadow, scheduler};
use std::{
    cell::RefCell,
    rc::Rc,
    time::{Duration, Instant},
};

fn setup() -> (Lua, WorldRef, scheduler::SchedulerRef) {
    let world=Rc::new(RefCell::new(world::World::from_snapshot(&json!({"version":1,"sequence":0,"services":{"Workspace":"workspace"},"nodes":[
        {"id":"game","class_name":"DataModel","name":"game","parent":null,"properties":{}},
        {"id":"workspace","class_name":"Workspace","name":"Workspace","parent":"game","properties":{}},
        {"id":"worker","class_name":"Actor","name":"worker","parent":"workspace","properties":{}}
    ]}),None).unwrap()));
    let (lua, queue) = vm(&world);
    (lua, world, queue)
}
fn vm(world: &WorldRef) -> (Lua, scheduler::SchedulerRef) {
    let lua = Lua::new();
    let queue = Rc::new(RefCell::new(scheduler::Scheduler::new()));
    scheduler::install(&lua, &queue).unwrap();
    rust_host::install_bindings(&lua, world).unwrap();
    (lua, queue)
}
fn rows(world: &WorldRef) -> Vec<Json> {
    world
        .borrow()
        .effects
        .get("mutations")
        .and_then(Json::as_array)
        .cloned()
        .unwrap_or_default()
}
fn step(lua: &Lua, queue: &scheduler::SchedulerRef) {
    scheduler::step(lua, queue, Instant::now(), Duration::from_secs(1));
    assert!(scheduler::drain_errors(queue).is_empty());
}

#[test]
fn float_curve_key_mutation_and_transport_roundtrip() {
    let (lua, _, _) = setup();
    let value: Value = lua
        .load(
            r#"
        local k=FloatCurveKey.new(-2,5)
        assert(typeof(k)=='FloatCurveKey' and k.Interpolation==Enum.KeyInterpolationMode.Cubic)
        assert(k.LeftTangent==nil and k.RightTangent==nil)
        k.Time=3;k.Value=6;k.LeftTangent=-2;k.RightTangent=4
        k.Interpolation=Enum.KeyInterpolationMode.Linear
        assert(k.RightTangent==nil and k.LeftTangent==-2)
        assert(not pcall(function()k.RightTangent=2 end))
        assert(not pcall(function()k.Interpolation=Enum.EasingStyle.Linear end))
        assert(not pcall(function()k.Other=1 end))
        return k
    "#,
        )
        .eval()
        .unwrap();
    let wire = values::to_json(&lua, &value).unwrap();
    assert_eq!(wire, json!({"$float_curve_key":[3.,6.,1,-2.,null]}));
    let copy = values::from_json(&lua, &wire).unwrap();
    assert_eq!(values::to_json(&lua, &copy).unwrap(), wire);
    assert!(values::from_json(&lua, &json!({"$float_curve_key":[0,1,1,null,2]})).is_err());
}

#[test]
fn ordered_keys_are_independent_and_duplicate_times_are_stable() {
    let (lua, _, _) = setup();
    lua.load(r#"
        local c=Instance.new('FloatCurve')
        assert(c.Length==0 and c:GetValueAtTime(0)==nil)
        local empty=c:GetKeyIndicesAtTime(0);assert(empty[1]==0 and empty[2]==0)
        local k=FloatCurveKey.new(2,20,Enum.KeyInterpolationMode.Linear)
        assert(c:SetKeys({k,FloatCurveKey.new(-1,-10),FloatCurveKey.new(2,99),FloatCurveKey.new(0,0)})==3)
        k.Value=500
        assert(c.Length==3 and c:GetKeyAtIndex(3).Value==20)
        local copy=c:GetKeys();copy[1].Value=300;copy[2].Time=100
        assert(c:GetKeyAtIndex(1).Value==-10 and c:GetKeyAtIndex(2).Time==0)
        for _,case in {{-2,1,1},{-1,1,1},{-.5,1,2},{0,2,2},{1,2,3},{5,3,3}} do
            local pair=c:GetKeyIndicesAtTime(case[1]);assert(pair[1]==case[2] and pair[2]==case[3])
        end
        local added=c:InsertKey(FloatCurveKey.new(1,10));assert(added[1] and added[2]==3)
        local replaced=c:InsertKey(FloatCurveKey.new(1,11));assert(not replaced[1] and replaced[2]==3)
        assert(c:RemoveKeyAtIndex(2,2)==2 and c.Length==2)
        assert(c:RemoveKeyAtIndex(2,100)==1 and c.Length==1)
        assert(c:RemoveKeyAtIndex(1)==1 and c.Length==0)
    "#).exec().unwrap();
}

#[test]
fn float_sampling_uses_time_scaled_tangents_and_exact_boundaries() {
    let (lua, _, _) = setup();
    lua.load(
        r#"
        local c=Instance.new('FloatCurve')
        local a=FloatCurveKey.new(0,0);local b=FloatCurveKey.new(2,2)
        a.RightTangent=1;b.LeftTangent=1;c:SetKeys({a,b})
        for _,time in {0,.1,.5,1,1.75,2} do assert(math.abs(c:GetValueAtTime(time)-time)<1e-12) end
        assert(c:GetValueAtTime(-10)==0 and c:GetValueAtTime(10)==2)
        a.Interpolation=Enum.KeyInterpolationMode.Constant;c:InsertKey(a)
        assert(c:GetValueAtTime(1.999)==0 and c:GetValueAtTime(2)==2)
        a.Interpolation=Enum.KeyInterpolationMode.Linear;c:InsertKey(a)
        assert(c:GetValueAtTime(.5)==.5)
        a=FloatCurveKey.new(0,0);b=FloatCurveKey.new(1,1)
        a.RightTangent=10;b.LeftTangent=-10;c:SetKeys({a,b})
        assert(math.abs(c:GetValueAtTime(.5)-3)<1e-12)
        c:SetKeys({FloatCurveKey.new(0,0),FloatCurveKey.new(1,1)})
        assert(math.abs(c:GetValueAtTime(.25)-.15625)<1e-12)
    "#,
    )
    .exec()
    .unwrap();
}

#[test]
fn rotation_curves_ignore_translation_and_take_the_short_arc() {
    let (lua, _, _) = setup();
    lua.load(r#"
        local c=Instance.new('RotationCurve');assert(c:GetValueAtTime(3)==CFrame.identity)
        local a=RotationCurveKey.new(0,CFrame.new(10,20,30)*CFrame.Angles(0,math.rad(170),0),Enum.KeyInterpolationMode.Linear)
        local b=RotationCurveKey.new(1,CFrame.Angles(0,math.rad(-170),0))
        c:SetKeys({b,a});assert(c:GetKeyAtIndex(1).Value.Position==Vector3.zero)
        local middle=c:GetValueAtTime(.5)
        assert(middle.Position==Vector3.zero and math.abs(middle.LookVector.Z-1)<1e-6)
        a=RotationCurveKey.new(0,CFrame.identity);b=RotationCurveKey.new(1,CFrame.Angles(0,math.pi/2,0))
        c:SetKeys({a,b})
        assert(c:GetValueAtTime(.25):FuzzyEq(CFrame.Angles(0,math.pi/2*.15625,0),1e-6))
        a.RightTangent=1;b.LeftTangent=1;c:SetKeys({a,b})
        assert(c:GetValueAtTime(.25):FuzzyEq(CFrame.Angles(0,math.pi/8,0),1e-6))
        a.Interpolation=Enum.KeyInterpolationMode.Constant;c:InsertKey(a)
        assert(c:GetValueAtTime(.9)==CFrame.identity and c:GetValueAtTime(1):FuzzyEq(b.Value,1e-6))
        local copy=c:GetKeys();copy[1].Value=CFrame.Angles(1,0,0)
        assert(c:GetValueAtTime(0)==CFrame.identity)
    "#).exec().unwrap();
}

#[test]
fn axis_curves_are_lazy_and_euler_orders_compose_in_radians() {
    let (lua, _, _) = setup();
    lua.load(r#"
        local c=Instance.new('Vector3Curve')
        local values=c:GetValueAtTime(0);assert(values[1]==nil and values[2]==nil and values[3]==nil)
        assert(#c:GetChildren()==0)
        local wrong=Instance.new('Folder',c);wrong.Name='X'
        local first=c:X();assert(first.ClassName=='FloatCurve' and first.Name=='X' and first.Parent==c)
        assert(c:X()==first)
        local second=Instance.new('FloatCurve',c);second.Name='X';second:InsertKey(FloatCurveKey.new(0,9))
        first:InsertKey(FloatCurveKey.new(0,2));c:Z():InsertKey(FloatCurveKey.new(0,4))
        local v=c:GetValueAtTime(5);assert(v[1]==2 and v[2]==nil and v[3]==4)
        first:Destroy();assert(c:X()==second)
        local e=Instance.new('EulerRotationCurve');assert(e.RotationOrder==Enum.RotationOrder.XYZ)
        assert(e:GetRotationAtTime(0)==CFrame.identity and #e:GetChildren()==0)
        e:X():InsertKey(FloatCurveKey.new(0,.2));e:Y():InsertKey(FloatCurveKey.new(0,.4));e:Z():InsertKey(FloatCurveKey.new(0,.7))
        for _,order in Enum.RotationOrder:GetEnumItems() do
            e.RotationOrder=order
            assert(e:GetRotationAtTime(10):FuzzyEq(CFrame.fromEulerAngles(.2,.4,.7,order),1e-6))
        end
        assert(not pcall(function()e.RotationOrder=Enum.EasingStyle.Linear end))
        local angles=e:GetAnglesAtTime(0);assert(angles[1]==.2 and angles[2]==.4 and angles[3]==.7)
        local clone=e:Clone();clone:X():InsertKey(FloatCurveKey.new(0,3))
        assert(e:X():GetValueAtTime(0)==.2 and clone:X():GetValueAtTime(0)==3)
    "#).exec().unwrap();
}

#[test]
fn invalid_input_and_budget_failures_are_atomic() {
    let (lua, ctx, _) = setup();
    lua.load("c=Instance.new('FloatCurve');c:SetKeys({FloatCurveKey.new(0,5)})")
        .exec()
        .unwrap();
    ctx.borrow_mut().effects.clear();
    for source in [
        "c:SetKeys({FloatCurveKey.new(0,3),RotationCurveKey.new(1,CFrame.identity)})",
        "c:SetKeys({[1]=FloatCurveKey.new(0,3),[3]=FloatCurveKey.new(1,4)})",
        "c:SetKeys({extra=FloatCurveKey.new(0,3)})",
        "c:InsertKey(FloatCurveKey.new(0/0,3))",
        "c:InsertKey(FloatCurveKey.new(0,math.huge))",
        "c:RemoveKeyAtIndex(0)",
        "c:RemoveKeyAtIndex(1,-1)",
        "c:GetKeyAtIndex(2)",
        "c:GetValueAtTime(0/0)",
        "c.Length=3",
    ] {
        assert!(lua.load(source).exec().is_err(), "{source}");
        lua.load("assert(c.Length==1 and c:GetValueAtTime(0)==5)")
            .exec()
            .unwrap();
        assert!(rows(&ctx).is_empty());
    }
    ctx.borrow_mut().effects.insert(
        "mutations".into(),
        json!(vec![json!({"kind":"padding"}); 4096]),
    );
    assert!(lua.load("c:SetKeys({})").exec().is_err());
    lua.load("assert(c.Length==1)").exec().unwrap();
}

#[test]
fn received_curves_remain_unknown_until_explicitly_replaced() {
    let (lua, ctx, _) = setup();
    world::apply_delta_with_lua(&lua,&ctx,&json!({"version":1,"delta":true,"sequence":1,"actions":[{"kind":"create","sequence":1,"node":{"id":"received","class_name":"FloatCurve","name":"Received","parent":"workspace","properties":{"Archivable":true},"attributes":{},"attributes_known":true,"collection_tags":[],"collection_tags_known":true}}]}),None).unwrap();
    lua.load(
        r#"
        local c=workspace.Received
        assert(not pcall(function()c:GetKeys()end))
        local clone=c:Clone();assert(not pcall(function()clone:GetKeys()end))
        c:SetKeys({FloatCurveKey.new(0,7)})
        assert(c:GetValueAtTime(0)==7)
        assert(not pcall(function()clone:GetKeys()end))
    "#,
    )
    .exec()
    .unwrap();
    world::apply_delta_with_lua(&lua,&ctx,&json!({"version":1,"delta":true,"sequence":2,"actions":[{"kind":"set","sequence":2,"id":"received","property":"ValuesAndTimes","value":{"$bytes_hex":"02"}}]}),None).unwrap();
    lua.load("assert(not pcall(function()workspace.Received:GetKeys()end))")
        .exec()
        .unwrap();
}

#[test]
fn actors_replay_key_edits_clones_and_axis_creation_in_order() {
    let (lua, root, _) = setup();
    let actor = Rc::new(RefCell::new(root.borrow().fork_for_actor("worker")));
    let (alua, _) = vm(&actor);
    alua.load(
        r#"
        local v=Instance.new('Vector3Curve',workspace);v.Name='Vector'
        v:X():InsertKey(FloatCurveKey.new(0,5));v:X():InsertKey(FloatCurveKey.new(1,10))
        local copy=v:Clone();copy.Name='Copy';copy.Parent=workspace
        v:X():InsertKey(FloatCurveKey.new(0,8))
        copy:X():RemoveKeyAtIndex(2)
    "#,
    )
    .exec()
    .unwrap();
    let report =
        actor_shadow::merge_actor_journal(&lua, &root, &actor, "worker", &rows(&actor)).unwrap();
    assert!(report.conflicts.is_empty(), "{:?}", report.conflicts);
    lua.load("assert(workspace.Vector:X().Length==2 and workspace.Vector:X():GetValueAtTime(0)==8);assert(workspace.Copy:X().Length==1 and workspace.Copy:X():GetValueAtTime(0)==5)").exec().unwrap();
}

#[test]
fn stale_actor_edits_are_rejected_and_peer_events_see_committed_keys() {
    let (lua, root, queue) = setup();
    lua.load(
        "c=Instance.new('FloatCurve',workspace);c.Name='Curve';c:SetKeys({FloatCurveKey.new(0,1)})",
    )
    .exec()
    .unwrap();
    root.borrow_mut().effects.clear();
    let actor = Rc::new(RefCell::new(root.borrow().fork_for_actor("worker")));
    let (alua, aq) = vm(&actor);
    alua.load("workspace.Curve:SetKeys({FloatCurveKey.new(0,2)})")
        .exec()
        .unwrap();
    lua.load("c:SetKeys({FloatCurveKey.new(0,3)})")
        .exec()
        .unwrap();
    let report =
        actor_shadow::merge_actor_journal(&lua, &root, &actor, "worker", &rows(&actor)).unwrap();
    assert_eq!(report.applied, 0);
    assert_eq!(report.conflicts, vec!["curve_keys:stale_input"]);
    alua.load("assert(workspace.Curve:GetValueAtTime(0)==3);getgenv().events=0;workspace.Curve:GetPropertyChangedSignal('Length'):Connect(function()events+=1;assert(workspace.Curve.Length==2)end)").exec().unwrap();
    root.borrow_mut().effects.clear();
    lua.load("c:InsertKey(FloatCurveKey.new(1,4))")
        .exec()
        .unwrap();
    world::sync_actor_fork_with_lua(&alua, &actor, &root, &rows(&root)).unwrap();
    step(&alua, &aq);
    step(&lua, &queue);
    alua.load("assert(events==1 and workspace.Curve:GetValueAtTime(1)==4)")
        .exec()
        .unwrap();
}

#[test]
fn curve_size_and_clone_journal_limits_leave_originals_intact() {
    let (lua, ctx, _) = setup();
    lua.load("c=Instance.new('FloatCurve');c:SetKeys({FloatCurveKey.new(0,7)})")
        .exec()
        .unwrap();
    let id = world::instance_id(&lua.globals().get::<Value>("c").unwrap()).unwrap();
    let oversized = Data::Float(
        (0..=MAX_KEYS)
            .map(|i| FloatCurveKey {
                time: i as f64,
                value: 0.,
                interpolation: 2,
                left: None,
                right: None,
            })
            .collect(),
    );
    assert!(commit(&lua, &ctx, &id, oversized).is_err());
    lua.load("assert(c.Length==1 and c:GetValueAtTime(0)==7)")
        .exec()
        .unwrap();
    let actor = Rc::new(RefCell::new(ctx.borrow().fork_for_actor("worker")));
    let (alua, _) = vm(&actor);
    alua.globals()
        .set("c", world::instance(&alua, &actor, &id).unwrap())
        .unwrap();
    actor.borrow_mut().effects.insert(
        "mutations".into(),
        json!(vec![json!({"kind":"padding"}); 4096]),
    );
    let before = actor.borrow().nodes.len();
    assert!(alua.load("return c:Clone()").eval::<Value>().is_err());
    assert_eq!(actor.borrow().nodes.len(), before);
    alua.load("assert(c:GetValueAtTime(0)==7)").exec().unwrap();
}

#[test]
fn curve_edits_invalidate_a_stale_actor_destroy() {
    let (lua, root, _) = setup();
    lua.load(
        "c=Instance.new('FloatCurve',workspace);c.Name='Curve';c:InsertKey(FloatCurveKey.new(0,1))",
    )
    .exec()
    .unwrap();
    let actor = Rc::new(RefCell::new(root.borrow().fork_for_actor("worker")));
    let (alua, _) = vm(&actor);
    alua.load("workspace.Curve:Destroy()").exec().unwrap();
    lua.load("c:InsertKey(FloatCurveKey.new(1,2))")
        .exec()
        .unwrap();
    let report =
        actor_shadow::merge_actor_journal(&lua, &root, &actor, "worker", &rows(&actor)).unwrap();
    assert_eq!(report.applied, 0);
    assert_eq!(report.conflicts.len(), 1);
    assert!(report.conflicts[0].starts_with("stale Actor destroy mutation"));
    lua.load("assert(workspace.Curve.Length==2)")
        .exec()
        .unwrap();
}
