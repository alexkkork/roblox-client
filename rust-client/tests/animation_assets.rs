#![cfg(feature = "rust-host")]
use headless_luau::{
    animation_assets::{AnimationAssetBackend, AnimationBackend, AssetError},
    runtime_owner::Runtime,
};
use serde_json::{json, Value};
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

fn snapshot() -> Value {
    json!({"version":1,"sequence":0,"services":{"Workspace":"workspace"},"nodes":[
        {"id":"game","class_name":"DataModel","name":"game","parent":null,"properties":{}},
        {"id":"workspace","class_name":"Workspace","name":"Workspace","parent":"game","properties":{}}
    ]})
}
fn backend() -> AnimationAssetBackend {
    AnimationAssetBackend::local(Some(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../tests/fixtures"),
    ))
}
fn check(reply: &Value) {
    assert_ne!(reply["ok"], false, "{}", reply["error"]);
    for key in ["task_errors", "animation_pose_errors", "joint_pose_errors"] {
        assert!(
            reply[key].as_array().is_none_or(Vec::is_empty),
            "{key}: {}",
            reply[key]
        );
    }
    if let Some(actors) = reply["actor_runtime"].as_array() {
        for actor in actors {
            check(actor);
            check(&actor["world_effects"]);
        }
    }
}
fn execute(runtime: &mut Runtime, source: &str) -> Value {
    let reply = runtime
        .call(json!({"op":"execute","source":source,"timeout_ms":2000}))
        .unwrap();
    check(&reply);
    reply
}
fn finish(runtime: &mut Runtime, reply: Value) -> Value {
    let job = reply["job_id"].as_u64().unwrap();
    let deadline = Instant::now() + Duration::from_secs(12);
    loop {
        let status = runtime.status(job).unwrap();
        check(&status);
        if status["completed"] == true {
            return status;
        }
        assert!(Instant::now() < deadline, "animation script did not finish");
        check(&runtime.poll().unwrap());
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn actual_asset_loading_drives_motor_and_bone_in_the_official_vm() {
    let mut runtime =
        Runtime::with_animation_backend(snapshot(), 32, Arc::new(backend()), None).unwrap();
    let reply = execute(
        &mut runtime,
        include_str!("../../examples/animation-assets-smoke.luau"),
    );
    let done = finish(&mut runtime, reply);
    assert_eq!(done["values"][0]["passed"].as_f64(), Some(10.));
}

#[test]
fn missing_asset_stops_waiting_playback_without_aborting_the_session() {
    let mut runtime =
        Runtime::with_animation_backend(snapshot(), 32, Arc::new(backend()), None).unwrap();
    execute(
        &mut runtime,
        r#"
        local rig=Instance.new("Model",workspace)
        local controller=Instance.new("AnimationController",rig)
        local animator=Instance.new("Animator",controller)
        local animation=Instance.new("Animation");animation.AnimationId="rbxasset://missing.rbxm"
        failedTrack=animator:LoadAnimation(animation);failedTrack:Play(0)
    "#,
    );
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut failure = Value::Null;
    while Instant::now() < deadline {
        let reply = runtime.poll().unwrap();
        check(&reply);
        if let Some(errors) = reply["animation_load_errors"].as_array() {
            if !errors.is_empty() {
                failure = errors[0].clone();
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(failure["error"], "animation_local_asset_unavailable");
    let reply=execute(&mut runtime,"assert(not failedTrack.IsPlaying); local ok=pcall(function() failedTrack:Play() end); assert(not ok); return 42");
    assert_eq!(reply["values"][0].as_f64(), Some(42.));
}

#[test]
fn actor_and_root_share_loading_but_receive_separate_vm_completions() {
    struct Counted {
        inner: AnimationAssetBackend,
        calls: AtomicUsize,
    }
    impl AnimationBackend for Counted {
        fn load(&self, reference: &str) -> Result<Value, AssetError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.inner.load(reference)
        }
    }
    let backend = Arc::new(Counted {
        inner: backend(),
        calls: AtomicUsize::new(0),
    });
    let mut world = snapshot();
    world["nodes"].as_array_mut().unwrap().extend([
        json!({"id":"actor","class_name":"Actor","name":"Worker","parent":"workspace","properties":{}}),
        json!({"id":"source","class_name":"LocalScript","name":"Source","parent":"actor","properties":{"Enabled":true,"Disabled":false}})
    ]);
    world["script_sources"] = json!({"source":{"source":r#"
        local rig=Instance.new("Model",workspace);rig.Name="ActorRig"
        local root=Instance.new("Part",rig);root.Name="Root";root.RootPriority=100
        local arm=Instance.new("Part",rig);arm.Name="Arm"
        local joint=Instance.new("Motor6D",root);joint.Part0=root;joint.Part1=arm
        local controller=Instance.new("AnimationController",rig)
        local animator=Instance.new("Animator",controller)
        local a=Instance.new("Animation");a.AnimationId="rbxasset://animations/two-keyframes.rbxmx"
        local track=animator:LoadAnimation(a);track:Play(0,1,0)
        local deadline=os.clock()+10
        while track.Length==0 and os.clock()<deadline do task.wait() end
        assert(track.Length==2,"actor did not get its asset")
        track.TimePosition=1;task.wait()
        script.Parent:SetAttribute("PoseX",joint.Transform.X)
        script.Parent:SetAttribute("Length",track.Length)
    "#}});
    let mut runtime = Runtime::with_animation_backend(world, 32, backend.clone(), None).unwrap();
    let reply = execute(
        &mut runtime,
        include_str!("../../examples/animation-assets-smoke.luau"),
    );
    finish(&mut runtime, reply);
    let reply = execute(
        &mut runtime,
        r#"
        local deadline=os.clock()+10
        while workspace.Worker:GetAttribute("Length")==nil and os.clock()<deadline do task.wait() end
        return workspace.Worker:GetAttribute("Length"),workspace.Worker:GetAttribute("PoseX")
    "#,
    );
    let done = finish(&mut runtime, reply);
    assert_eq!(done["values"], json!([2.0, 2.0]));
    assert_eq!(backend.calls.load(Ordering::SeqCst), 1);
}

#[test]
#[ignore = "requires an authorized account file and animation asset ID"]
fn authorized_asset_delivery_decodes_a_real_clip() {
    use headless_luau::account_auth::AccountSession;
    let path = PathBuf::from(
        std::env::var_os("ROVE_ANIMATION_COOKIE_FILE").expect("set ROVE_ANIMATION_COOKIE_FILE"),
    );
    let id = std::env::var("ROVE_ANIMATION_ASSET_ID").expect("set ROVE_ANIMATION_ASSET_ID");
    let mut account =
        AccountSession::from_private_file(&path).expect("account file could not be loaded");
    account.validate().expect("account validation failed");
    let (session, _) = account.into_parts();
    struct Observed {
        inner: AnimationAssetBackend,
        metadata: std::sync::Mutex<Value>,
    }
    impl AnimationBackend for Observed {
        fn load(&self, reference: &str) -> Result<Value, AssetError> {
            let clip = self.inner.load(reference)?;
            let frames = clip["keyframes"].as_array().unwrap();
            *self.metadata.lock().unwrap() = json!({"length":clip["length"],"keyframes":frames.len(),
                "pose_samples":frames.iter().map(|f|f["poses"].as_array().map_or(0,Vec::len)).sum::<usize>(),
                "source_sha256":clip["source_sha256"]});
            Ok(clip)
        }
    }
    let id = id.parse::<u64>().expect("asset ID must be numeric");
    let backend = Arc::new(Observed {
        inner: AnimationAssetBackend::authenticated(None, session),
        metadata: std::sync::Mutex::new(Value::Null),
    });
    let mut runtime =
        Runtime::with_animation_backend(snapshot(), 32, backend.clone(), None).unwrap();
    let source = format!(
        r#"
        local rig=Instance.new("Model",workspace)
        local root=Instance.new("Part",rig);root.Name="HumanoidRootPart";root.RootPriority=100
        local torso=Instance.new("Part",rig);torso.Name="LowerTorso"
        local motor=Instance.new("Motor6D",root);motor.Part0=root;motor.Part1=torso
        local controller=Instance.new("AnimationController",rig)
        local animator=Instance.new("Animator",controller)
        local animation=Instance.new("Animation");animation.AnimationId="rbxassetid://{id}"
        local track=animator:LoadAnimation(animation);track:Play(0,1,0)
        local deadline=os.clock()+10
        while track.Length==0 and track.IsPlaying and os.clock()<deadline do task.wait() end
        assert(track.Length>0,"real asset failed to load into the VM")
        track.TimePosition=track.Length/2;task.wait()
        assert(track.IsPlaying)
        local length=track.Length;track:Stop(0);return length
    "#
    );
    let reply = execute(&mut runtime, &source);
    let done = finish(&mut runtime, reply);
    assert!(done["values"][0].as_f64().unwrap() > 0.0);
    println!(
        "authorized animation loaded in VM: {}",
        backend.metadata.lock().unwrap()
    );
}
