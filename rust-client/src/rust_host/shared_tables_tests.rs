use super::*;
use serde_json::json;
use std::{cell::RefCell, rc::Rc};

fn lua_and_world() -> (Lua, super::super::world::WorldRef) {
    let world=super::super::world::World::from_snapshot(&json!({"version":1,"nodes":[{"id":"registry","class_name":"SharedTableRegistry","name":"SharedTableRegistry"}],"services":{"SharedTableRegistry":"registry"}}),None).unwrap();
    (Lua::new(), Rc::new(RefCell::new(world)))
}

#[test]
fn cycles_clone_identity_and_freeze_follow_shared_table_semantics() {
    let (lua, world) = lua_and_world();
    let registry = SharedTableRegistry::default();
    super::super::world::install(&lua, &world).unwrap();
    super::super::numeric::install(&lua).unwrap();
    super::super::cframe::install(&lua).unwrap();
    install(&lua, &world, registry).unwrap();
    lua.load(
        r#"
        local inner = {}; inner.self = inner
        local t = SharedTable.new({n = 4, child = inner, v=Vector3.new(1,2,3), cf=CFrame.new(4,5,6)})
        assert(typeof(t) == "SharedTable" and t.n == 4 and t.child == t.child.self)
        assert(typeof(t.v)=="Vector3" and t.v.X==1 and typeof(t.cf)=="CFrame" and t.cf.X==4)
        local shallow=t:clone(); assert(shallow ~= t and shallow.child == t.child)
        local deep=t:clone(true); assert(deep.child ~= t.child and deep.child == deep.child.self)
        local frozen=t:cloneAndFreeze(true)
        assert(frozen:isFrozen() and frozen.child:isFrozen() and frozen:size()==4)
        assert(not pcall(function() frozen.n=8 end))
        assert(not pcall(function() frozen.child.x=1 end))
        t:clear(); assert(t:size()==0)
    "#,
    )
    .exec()
    .unwrap();
}

#[test]
fn update_callback_runs_without_a_table_lock_and_increment_returns_old_value() {
    let (lua, world) = lua_and_world();
    let registry = SharedTableRegistry::default();
    super::super::world::install(&lua, &world).unwrap();
    super::super::numeric::install(&lua).unwrap();
    super::super::cframe::install(&lua).unwrap();
    install(&lua, &world, registry).unwrap();
    lua.load(
        r#"
        local t=SharedTable.new({count=2})
        assert(t:increment("count",3)==2 and t.count==5)
        local first=true
        t:update("count",function(old)
            if first then first=false; t.side_effect="ran outside lock" end
            return old+1
        end)
        assert(t.count==6 and t.side_effect=="ran outside lock")
    "#,
    )
    .exec()
    .unwrap();
}

#[test]
fn registry_shares_tables_and_codec_resolves_ids_in_another_vm() {
    let registry = SharedTableRegistry::default();
    let (lua_a, world) = lua_and_world();
    super::super::world::install(&lua_a, &world).unwrap();
    super::super::numeric::install(&lua_a).unwrap();
    super::super::cframe::install(&lua_a).unwrap();
    install(&lua_a, &world, registry.clone()).unwrap();
    let service = super::super::world::instance(&lua_a, &world, "registry").unwrap();
    lua_a.globals().set("registryService", service).unwrap();
    lua_a.load("local t=SharedTable.new({value=41}); registryService:SetSharedTable('shared',t); assert(registryService:GetSharedTable('shared')==t)").exec().unwrap();
    let (lua_b, world_b) = lua_and_world();
    super::super::world::install(&lua_b, &world_b).unwrap();
    super::super::numeric::install(&lua_b).unwrap();
    super::super::cframe::install(&lua_b).unwrap();
    install(&lua_b, &world_b, registry.clone()).unwrap();
    let service_b = super::super::world::instance(&lua_b, &world_b, "registry").unwrap();
    lua_b.globals().set("registryService", service_b).unwrap();
    lua_b
        .load("local t=registryService:GetSharedTable('shared'); assert(t.value==41); t.value=42")
        .exec()
        .unwrap();
    lua_a
        .load("assert(registryService:GetSharedTable('shared').value==42)")
        .exec()
        .unwrap();
    let stored = registry.get_registered("shared").unwrap();
    let wrapped = wrapper_value(&lua_a, &registry, stored).unwrap();
    let encoded = super::super::values::to_json(&lua_a, &wrapped).unwrap();
    assert_eq!(
        encoded,
        json!({"$shared_table_ref":registry.get_registered("shared").unwrap().id})
    );
    let decoded = super::super::values::from_json(&lua_b, &encoded).unwrap();
    assert!(matches!(decoded, Value::UserData(_)));
}

#[test]
fn invalid_values_keys_foreign_pool_and_cross_vm_identity_are_rejected() {
    let (lua, world) = lua_and_world();
    let pool = SharedTableRegistry::default();
    super::super::world::install(&lua, &world).unwrap();
    super::super::numeric::install(&lua).unwrap();
    super::super::cframe::install(&lua).unwrap();
    install(&lua, &world, pool.clone()).unwrap();
    lua.load(
        r#"
        local t=SharedTable.new()
        assert(not pcall(function() t[-1]=1 end))
        assert(not pcall(function() t[1.5]=1 end))
        assert(not pcall(function() t.fn=function() end end))
        assert(not pcall(function() t.child={} end))
        assert(not pcall(function() SharedTable.new({}, {}) end))
    "#,
    )
    .exec()
    .unwrap();
    let data = pool.create().unwrap();
    let other = SharedTableRegistry::default();
    assert!(!other.contains(&data));
}
