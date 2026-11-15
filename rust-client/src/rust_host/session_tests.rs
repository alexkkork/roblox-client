use super::*;

fn command(session: &mut Session, request: Json) -> Json {
    serde_json::from_slice(
        &session
            .request(&serde_json::to_vec(&request).unwrap())
            .unwrap(),
    )
    .unwrap()
}

#[test]
fn drawings_persist_across_response_drain_and_repeated_render() {
    let mut session = Session::new();
    let initialized = command(
        &mut session,
        json!({"op":"init","snapshot":empty_snapshot("drawings-fixture"),"memory_mb":16}),
    );
    assert_eq!(initialized["ok"], true, "{initialized}");
    let executed = command(
        &mut session,
        json!({"op":"execute","source":"mark = Drawing.new('Circle'); mark.Visible = true; mark.Position = Vector2.new(20,30); mark.Radius = 8"}),
    );
    assert_eq!(executed["ok"], true, "{executed}");
    for _ in 0..2 {
        let rendered = command(&mut session, json!({"op":"render"}));
        assert_eq!(rendered["ok"], true, "{rendered}");
        assert_eq!(
            rendered["frame"]["drawings"].as_array().unwrap().len(),
            1,
            "{rendered}"
        );
        let polled = command(&mut session, json!({"op":"poll"}));
        assert!(
            polled.get("drawing_scene").is_none(),
            "render data leaked into effects: {polled}"
        );
    }
    let removed = command(
        &mut session,
        json!({"op":"execute","source":"mark:Remove()"}),
    );
    assert_eq!(removed["ok"], true, "{removed}");
    let rendered = command(&mut session, json!({"op":"render"}));
    assert!(
        rendered["frame"]["drawings"].as_array().unwrap().is_empty(),
        "{rendered}"
    );
}

#[test]
fn text_service_reply_reaches_the_waiting_persistent_job() {
    let mut snapshot = empty_snapshot("text-fixture");
    snapshot["services"]["TextService"] = json!("text");
    snapshot["nodes"].as_array_mut().unwrap().push(json!({"id":"text","class_name":"TextService","name":"TextService","parent":"game","properties":{}}));
    let mut session = Session::new();
    let initialized = command(
        &mut session,
        json!({"op":"init","snapshot":snapshot,"memory_mb":16}),
    );
    assert_eq!(initialized["ok"], true, "{initialized}");
    let executed = command(
        &mut session,
        json!({"op":"execute","source":"local family = game:GetService('TextService'):GetFamilyInfoAsync('rbxasset://fonts/families/Test.json'); return family.Name, #family.Faces"}),
    );
    assert_eq!(executed["completed"], false, "{executed}");
    let request = &executed["text_service_requests"][0];
    let id = request["request_id"].as_u64().unwrap();
    assert_eq!(
        executed["active_text_service_requests"],
        json!([id]),
        "{executed}"
    );
    let completed = command(
        &mut session,
        json!({"op":"poll","text_service_responses":[{"request_id":id,"result":{"Name":"Fixture font","Faces":[{"Name":"Regular","Weight":400,"Style":0}]}}]}),
    );
    let jobs = completed["completed_jobs"].as_array().unwrap();
    assert_eq!(jobs.len(), 1, "{completed}");
    assert_eq!(jobs[0]["job_id"], executed["job_id"], "{completed}");
    assert_eq!(
        jobs[0]["values"],
        json!(["Fixture font", 1.0]),
        "{completed}"
    );
    assert_eq!(
        completed["active_text_service_requests"],
        json!([]),
        "{completed}"
    );
}

#[test]
fn interaction_effects_require_received_signature_and_keep_local_signals() {
    let mut snapshot = empty_snapshot("interaction-fixture");
    snapshot["class_event_signatures"] =
        json!({"ProximityPrompt":{"TriggeredActionReplicated":[28]}});
    snapshot["nodes"].as_array_mut().unwrap().extend([
        json!({"id":"prompt","class_name":"ProximityPrompt","name":"Prompt","parent":"workspace","properties":{},"server_owned":true}),
        json!({"id":"a","class_name":"Part","name":"A","parent":"workspace","properties":{},"server_owned":true}),
        json!({"id":"b","class_name":"Part","name":"B","parent":"workspace","properties":{},"server_owned":true}),
    ]);
    let mut session = Session::new();
    let initialized = command(
        &mut session,
        json!({"op":"init","snapshot":snapshot,"memory_mb":16}),
    );
    assert_eq!(initialized["ok"], true, "{initialized}");
    let fired = command(
        &mut session,
        json!({"op":"execute","source":"touches=0; workspace.A.Touched:Connect(function(other) assert(other==workspace.B); touches+=1 end); fireproximityprompt(workspace.Prompt); firetouchinterest(workspace.A,workspace.B,true)"}),
    );
    assert_eq!(fired["ok"], true, "{fired}");
    assert_eq!(
        fired["remote_server_events"],
        json!([
            {"node":"prompt","event":"TriggeredActionReplicated","argument_types":[28],"arguments":[{"$ref":null}]},
            {"touch":{"first":"a","second":"b","began":true}}
        ])
    );
    let checked = command(
        &mut session,
        json!({"op":"execute","source":"assert(touches==1); assert(not pcall(firetouchinterest,workspace.A,workspace.B,2)); local localPrompt=Instance.new('ProximityPrompt'); fireproximityprompt(localPrompt); return true"}),
    );
    assert_eq!(checked["ok"], true, "{checked}");
    assert_eq!(checked["remote_server_events"], json!([]));
    session.vm.as_mut().unwrap().world.borrow_mut().snapshot["class_event_signatures"] = json!({});
    let rejected = command(
        &mut session,
        json!({"op":"execute","source":"return pcall(fireproximityprompt,workspace.Prompt)"}),
    );
    assert_eq!(rejected["values"][0], false, "{rejected}");
    assert_eq!(rejected["remote_server_events"], json!([]));
}

#[test]
fn compiler_owned_program_uses_bounded_vm_and_current_context() {
    let bytes = include_bytes!("../../../tests/fixtures/luau-v9/calculation.luac");
    let result =
        run_program_bytes(bytes, &[2., 5.], "00000000-0000-4000-8000-000000000001").unwrap();
    assert_eq!(result["ok"], true, "{result}");
    assert_eq!(result["values"], json!([8 ^ 0x1234, "owned-v9"]));
    assert_eq!(result["runtime_protected_integrity"]["failures"], 0);
    assert!(
        result["runtime_protected_integrity"]["constructions"]
            .as_u64()
            .unwrap()
            > 0
    );
    let context = include_bytes!("../../../tests/fixtures/luau-session-context/source.luac");
    for suffix in [1, 2] {
        let job = format!("00000000-0000-4000-8000-{suffix:012}");
        let result = run_program_bytes(context, &[7., 11.], &job).unwrap();
        assert_eq!(result["values"], json!([job, false, 7, 11]), "{result}");
    }
}

#[test]
fn program_context_has_no_authored_io_capabilities() {
    let bytes=mlua::chunk::Compiler::new().compile(b"return request==nil, readfile==nil, loadstring==nil, debug==nil, setclipboard==nil, game:FindService('HttpService')==nil").unwrap();
    let result =
        run_program_bytes(&bytes, &[1., 2.], "00000000-0000-4000-8000-000000000001").unwrap();
    assert_eq!(
        result["values"],
        json!([true, true, true, true, true, true]),
        "{result}"
    );
    assert!(run_program_bytes(&bytes, &[1., 2.], "unissued-session").is_err());
    assert!(run_program_bytes(
        &bytes,
        &[f64::NAN, 2.],
        "00000000-0000-4000-8000-000000000001"
    )
    .is_err());
}

#[test]
fn request_requires_init_and_retains_a_single_vm() {
    let mut session = Session::new();
    assert!(session.request(br#"{"op":"poll"}"#).is_err());
    let init = serde_json::to_vec(
        &json!({"op":"init","snapshot":empty_snapshot("offline-fixture-session"),"memory_mb":16}),
    )
    .unwrap();
    let result: Json = serde_json::from_slice(&session.request(&init).unwrap()).unwrap();
    assert_eq!(result["ok"], true, "{result}");
    assert!(session.request(&init).is_err());
    let result: Json = serde_json::from_slice(
        &session
            .request(br#"{"op":"execute","source":"error('deadline is just text')"}"#)
            .unwrap(),
    )
    .unwrap();
    assert_eq!(result["ok"], false, "{result}");
    assert_eq!(result["timed_out"], false, "{result}");
    let result: Json = serde_json::from_slice(
        &session
            .request(br#"{"op":"execute","source":"return 6 * 7"}"#)
            .unwrap(),
    )
    .unwrap();
    assert_eq!(result["ok"], true, "{result}");
    assert_eq!(result["values"][0].as_f64(), Some(42.));
}
