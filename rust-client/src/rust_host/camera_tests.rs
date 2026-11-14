use super::*;
use crate::rust_host::{actor_shadow, scheduler};
use std::{
    cell::RefCell,
    rc::Rc,
    time::{Duration, Instant},
};

struct Vm {
    lua: Lua,
    world: WorldRef,
    scheduler: scheduler::SchedulerRef,
}
impl Vm {
    fn new(world: world::World) -> Self {
        let lua = Lua::new();
        let scheduler = Rc::new(RefCell::new(scheduler::Scheduler::new()));
        scheduler::install(&lua, &scheduler).unwrap();
        let world = Rc::new(RefCell::new(world));
        crate::rust_host::install_bindings(&lua, &world).unwrap();
        Self {
            lua,
            world,
            scheduler,
        }
    }
    fn run(&self, source: &str) {
        self.lua.load(source).exec().unwrap();
    }
    fn drain(&self) {
        scheduler::step(
            &self.lua,
            &self.scheduler,
            Instant::now(),
            Duration::from_millis(30),
        );
        assert!(scheduler::drain_errors(&self.scheduler).is_empty());
    }
    fn journal(&self) -> Vec<Json> {
        self.world
            .borrow()
            .effects
            .get("mutations")
            .and_then(Json::as_array)
            .cloned()
            .unwrap_or_default()
    }
    fn sync(&self, root: &Vm, journal: &[Json]) {
        world::sync_actor_fork_with_lua(&self.lua, &self.world, &root.world, journal).unwrap();
        self.drain();
    }
    fn resize(&self, sequence: u64) {
        world::apply_delta_with_lua(&self.lua,&self.world,&json!({"version":1,"delta":true,"sequence":sequence,"display":{"size":[1000,500]},"actions":[]}),None).unwrap();
        self.drain();
    }
}
fn setup() -> (Vm, Vm) {
    let root=Vm::new(world::World::from_snapshot(&json!({"version":1,"sequence":1,"display":{"size":[800,600]},"services":{"Workspace":"ws"},"nodes":[{"id":"ws","class_name":"Workspace","name":"Workspace","parent":null,"properties":{}}]}),None).unwrap());
    root.run("c=Instance.new('Camera',workspace);c.Name='TestCamera';c.CFrame=CFrame.new();c.FieldOfView=90;workspace.CurrentCamera=c;other=Instance.new('Camera',workspace);other.Name='Other'");
    let actor = Vm::new(root.world.borrow().fork_for_actor("worker"));
    actor.run("c=workspace.CurrentCamera");
    root.world.borrow_mut().effects.clear();
    actor.world.borrow_mut().effects.clear();
    (root, actor)
}

#[test]
fn actor_camera_lens_and_pose_use_atomic_revision_fenced_batches() {
    let (root, actor) = setup();
    actor.run("c.CoordinateFrame=CFrame.new(3,4,5);c.DiagonalFieldOfView=90;c.FieldOfViewMode=Enum.FieldOfViewMode.Diagonal");
    let journal = actor.journal();
    assert_eq!(journal.len(), 3);
    assert!(journal.iter().all(|row| row["kind"] == "pose_batch"));
    let report =
        actor_shadow::merge_actor_journal(&root.lua, &root.world, &actor.world, "worker", &journal)
            .unwrap();
    assert_eq!(report.applied, 3);
    assert!(report.conflicts.is_empty());
    root.run("assert(c.CFrame==CFrame.new(3,4,5) and c.CoordinateFrame==c.CFrame);assert(math.abs(c.DiagonalFieldOfView-90)<.0001);assert(c.FieldOfViewMode==Enum.FieldOfViewMode.Diagonal)");
}

#[test]
fn stale_actor_lens_batch_cannot_partially_overwrite_root_angles() {
    let (root, actor) = setup();
    actor.run("c.DiagonalFieldOfView=90");
    root.run("c.FieldOfView=75;diagonal=c.DiagonalFieldOfView;maximum=c.MaxAxisFieldOfView");
    let report = actor_shadow::merge_actor_journal(
        &root.lua,
        &root.world,
        &actor.world,
        "worker",
        &actor.journal(),
    )
    .unwrap();
    assert_eq!(report.applied, 0);
    assert_eq!(report.conflicts.len(), 1);
    root.run("assert(c.FieldOfView==75 and c.DiagonalFieldOfView==diagonal and c.MaxAxisFieldOfView==maximum)");
}

#[test]
fn actor_receives_viewport_and_derived_lens_events_once() {
    let (root, actor) = setup();
    actor.run("seen=0;diagonal=0;c:GetPropertyChangedSignal('ViewportSize'):Connect(function() seen+=1;assert(c.ViewportSize==Vector2.new(1000,500)) end);c:GetPropertyChangedSignal('DiagonalFieldOfView'):Connect(function() diagonal+=1 end)");
    root.resize(2);
    actor.sync(&root, &[]);
    actor.run("assert(seen==1 and diagonal==1)");
    actor.sync(&root, &[]);
    actor.run("assert(seen==1 and diagonal==1)");
}

#[test]
fn actor_local_camera_refresh_does_not_duplicate_resize_events() {
    let (root, actor) = setup();
    actor.run("c=Instance.new('Camera',workspace);c.CFrame=CFrame.new();c.FieldOfView=90;workspace.CurrentCamera=c;seen=0;diagonal=0;c:GetPropertyChangedSignal('ViewportSize'):Connect(function() seen+=1 end);c:GetPropertyChangedSignal('DiagonalFieldOfView'):Connect(function() diagonal+=1 end)");
    root.resize(2);
    actor.sync(&root, &[]);
    actor.run("assert(workspace.CurrentCamera==c);assert(c.ViewportSize==Vector2.new(1000,500));assert(seen==1 and diagonal==1)");
}

#[test]
fn switching_root_camera_notifies_a_peer_that_watched_the_inactive_camera() {
    let (root, actor) = setup();
    actor.run("other=workspace.Other;seen=0;other:GetPropertyChangedSignal('ViewportSize'):Connect(function() seen+=1;assert(other.ViewportSize==Vector2.new(800,600)) end)");
    root.run("workspace.CurrentCamera=other");
    actor.sync(&root, &root.journal());
    actor.run("assert(workspace.CurrentCamera==other and seen==1)");
}

#[test]
fn destroyed_cameras_are_removed_from_retained_surfaces() {
    let (root, _) = setup();
    assert_eq!(root.world.borrow().camera_state.surfaces.len(), 1);
    root.run("c:Destroy()");
    refresh(&root.lua, &root.world).unwrap();
    assert!(root.world.borrow().camera_state.surfaces.is_empty());
}

#[test]
fn surface_sync_prefers_adopted_root_state_and_keeps_only_its_own_local_namespace() {
    let old = Surface {
        size: [800., 600.],
        ..Surface::default()
    };
    let next = Surface {
        size: [1000., 500.],
        ..Surface::default()
    };
    let mut actor = CameraState {
        surfaces: HashMap::from([
            ("local:actor:77:1".into(), old),
            ("local:actor:77:2".into(), old),
            ("local:actor:7730:1".into(), old),
        ]),
    };
    let root = CameraState {
        surfaces: HashMap::from([("local:actor:77:1".into(), next)]),
    };
    actor.sync_from_root(&root, "local:actor:77");
    assert_eq!(actor.surfaces["local:actor:77:1"], next);
    assert_eq!(actor.surfaces["local:actor:77:2"], old);
    assert!(!actor.surfaces.contains_key("local:actor:7730:1"));
}
