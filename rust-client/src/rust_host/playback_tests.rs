use super::*;
use std::{
    cell::Cell,
    time::{Duration, Instant},
};
use std::{cell::RefCell, rc::Rc};

fn test_world() -> WorldRef {
    let snapshot = json!({"version":1,"sequence":0,"nodes":[{"id":"part","class_name":"Part","properties":{"Transparency":0.0}}]});
    Rc::new(RefCell::new(
        world::World::from_snapshot(&snapshot, None).unwrap(),
    ))
}

fn local_tween(ctx: &WorldRef, info: TweenInfo) -> Rc<RefCell<TweenState>> {
    Rc::new(RefCell::new(TweenState {
        id: "tween-test".into(),
        target: "part".into(),
        world: ctx.clone(),
        goals: vec![("Transparency".into(), json!(1.0))],
        starts: vec![("Transparency".into(), json!(0.0))],
        info,
        elapsed: 0.,
        state: 2,
        last: 0.,
        last_error: None,
    }))
}

fn install_test_signals(lua: &Lua, ctx: &WorldRef) {
    // Property writes emit Changed/property signals even when the test has no
    // listeners. Install the real signal hub so those engine notifications
    // don't make `step` cancel an otherwise valid tween.
    super::super::signals::install(lua, ctx).unwrap();
    // Tween Completed carries the engine PlaybackState enum item.
    super::super::enums::install(lua).unwrap();
}

fn assert_tween_state(state: &Rc<RefCell<TweenState>>, expected: i32) {
    let tween = state.borrow();
    assert_eq!(
        tween.state, expected,
        "tween state mismatch at elapsed {} (last error: {:?})",
        tween.elapsed, tween.last_error
    );
    assert!(
        tween.last_error.is_none(),
        "tween failed at elapsed {}: {:?}",
        tween.elapsed,
        tween.last_error
    );
}

#[test]
fn ease_modes_hit_endpoints_and_midpoints() {
    for style in 0..=10 {
        assert_eq!(ease(style, 0, 0.), 0.);
        assert_eq!(ease(style, 1, 1.), 1.);
        assert!(ease(style, 2, 0.5).is_finite());
    }
    assert_eq!(ease(0, 0, 0.25), 0.25);
    assert_eq!(ease(3, 0, 0.5), 0.25);
    assert_eq!(ease(3, 1, 0.5), 0.75);
}

#[test]
fn deterministic_step_interpolates_then_completes() {
    let lua = Lua::new();
    let ctx = test_world();
    install_test_signals(&lua, &ctx);
    let state = local_tween(
        &ctx,
        TweenInfo {
            time: 2.,
            style: 0,
            direction: 0,
            repeats: 0,
            reverses: false,
            delay: 0.,
        },
    );
    ensure_store(&lua).tweens.push(state.clone());
    step(&lua, &ctx, 0.5).unwrap();
    assert_eq!(
        world::get_property(&ctx, "part", "Transparency").unwrap(),
        json!(0.25)
    );
    step(&lua, &ctx, 1.5).unwrap();
    assert_eq!(
        world::get_property(&ctx, "part", "Transparency").unwrap(),
        json!(1.0)
    );
    assert_tween_state(&state, 4);
}

#[test]
fn delay_and_pause_do_not_consume_tween_time() {
    let lua = Lua::new();
    let ctx = test_world();
    install_test_signals(&lua, &ctx);
    let state = local_tween(
        &ctx,
        TweenInfo {
            time: 1.,
            style: 0,
            direction: 0,
            repeats: 0,
            reverses: false,
            delay: 0.5,
        },
    );
    state.borrow_mut().state = 1;
    ensure_store(&lua).tweens.push(state.clone());
    step(&lua, &ctx, 0.25).unwrap();
    assert_eq!(
        world::get_property(&ctx, "part", "Transparency").unwrap(),
        json!(0.0)
    );
    step(&lua, &ctx, 0.5).unwrap();
    assert_eq!(
        world::get_property(&ctx, "part", "Transparency").unwrap(),
        json!(0.25)
    );
    state.borrow_mut().state = 3;
    step(&lua, &ctx, 1.).unwrap();
    assert_eq!(
        world::get_property(&ctx, "part", "Transparency").unwrap(),
        json!(0.25)
    );
}

#[test]
fn reverse_repeat_returns_to_start_on_final_cycle() {
    let lua = Lua::new();
    let ctx = test_world();
    install_test_signals(&lua, &ctx);
    let state = local_tween(
        &ctx,
        TweenInfo {
            time: 1.,
            style: 0,
            direction: 0,
            repeats: 1,
            reverses: true,
            delay: 0.,
        },
    );
    ensure_store(&lua).tweens.push(state.clone());
    step(&lua, &ctx, 1.).unwrap();
    assert_eq!(
        world::get_property(&ctx, "part", "Transparency").unwrap(),
        json!(1.)
    );
    step(&lua, &ctx, 1.).unwrap();
    assert_eq!(
        world::get_property(&ctx, "part", "Transparency").unwrap(),
        json!(0.)
    );
    assert_tween_state(&state, 2);
    step(&lua, &ctx, 2.).unwrap();
    assert_eq!(
        world::get_property(&ctx, "part", "Transparency").unwrap(),
        json!(0.)
    );
    assert_tween_state(&state, 4);
}

#[test]
fn invalid_elapsed_is_rejected() {
    let lua = Lua::new();
    let ctx = test_world();
    assert!(step(&lua, &ctx, f64::NAN).is_err());
    assert!(step(&lua, &ctx, -0.1).is_err());
}

#[test]
fn workspace_wave_metadata_yields_deterministic_local_duration() {
    let data_bytes = 16_000u32;
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + data_bytes).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&8_000u32.to_le_bytes());
    bytes.extend_from_slice(&16_000u32.to_le_bytes());
    bytes.extend_from_slice(&2u16.to_le_bytes());
    bytes.extend_from_slice(&16u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data_bytes.to_le_bytes());
    bytes.resize(bytes.len() + data_bytes as usize, 0);
    assert_eq!(audio_duration(&bytes), Some(1.0));
    assert_eq!(audio_duration(b"RIFFbad"), None);
    let result = queue_audio_decode(bytes)
        .unwrap()
        .recv_timeout(Duration::from_secs(2))
        .unwrap();
    assert_eq!(result, Some(1.0));
}

#[test]
fn animation_response_drives_keyframe_and_marker_signals() {
    let lua = Lua::new();
    let ctx = test_world();
    let signals = Rc::new(RefCell::new(super::super::signals::SignalHub::new()));
    lua.set_app_data(signals);
    let scheduler = Rc::new(RefCell::new(super::super::scheduler::Scheduler::new()));
    super::super::scheduler::install(&lua, &scheduler).unwrap();
    let reached = Rc::new(Cell::new(false));
    let marker = reached.clone();
    let callback = lua
        .create_function(move |_, value: String| {
            if value == "left" {
                marker.set(true)
            }
            Ok(())
        })
        .unwrap();
    super::super::signals::connect_owned(
        &lua,
        &ctx,
        "track-test",
        "marker:step",
        callback,
        "test",
        false,
    )
    .unwrap();
    let reference = "rbxassetid://test";
    ensure_store(&lua)
        .animation_requested
        .insert(reference.into());
    let clip = json!({"version":1,"kind":"KeyframeSequence","length":2.0,"looped":false,"priority":null,"keyframes":[
        {"time":0.0,"name":"Keyframe","markers":[]},
        {"time":1.0,"name":"Footstep","markers":[{"name":"step","value":"left"}]}
    ]});
    complete_animation_requests(&lua, &json!([{"reference":reference,"clip":clip}])).unwrap();
    let track = Rc::new(RefCell::new(TrackState {
        id: "track-test".into(),
        world: ctx.clone(),
        animator: "humanoid".into(),
        animation: "animation".into(),
        reference: reference.into(),
        position: 0.,
        length: Some(2.),
        playing: true,
        active: true,
        looped: false,
        speed: 1.,
        weight: 1.,
        fade_from: 1.,
        target_weight: 1.,
        fade_elapsed: 0.,
        fade_duration: 0.,
        start_pending: true,
        priority: 1000,
        priority_override: false,
        loop_override: false,
    }));
    ensure_store(&lua).tracks.push(track);
    step(&lua, &ctx, 1.25).unwrap();
    super::super::scheduler::step(&lua, &scheduler, Instant::now(), Duration::from_millis(50));
    assert!(reached.get());
    assert_eq!(track_position(&lua), 1.25);
}

fn track_position(lua: &Lua) -> f64 {
    lua.app_data_ref::<Store>().unwrap().tracks[0]
        .borrow()
        .position
}
