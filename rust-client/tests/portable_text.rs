//! Real font files through the official Luau VM; also runs on native Windows CI.
#![cfg(feature = "rust-host")]

use headless_luau::{engine_services::local_text::LocalTextBackend, runtime_owner::Runtime};
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_owned()
}

fn snapshot() -> Value {
    let mut snapshot: Value =
        serde_json::from_str(include_str!("../../tests/fixtures/world_snapshot.json")).unwrap();
    snapshot["display"] =
        json!({"size":[800,600],"device_insets":[0,0,0,0],"core_insets":[0,0,0,0]});
    snapshot
}

fn runtime() -> Runtime {
    Runtime::with_text_backend(
        snapshot(),
        32,
        Some(Arc::new(LocalTextBackend::new(
            root().join("tests/fixtures"),
        ))),
    )
    .unwrap()
}

fn execute(runtime: &mut Runtime, source: &str) -> Value {
    let first = runtime
        .call(json!({"op":"execute","source":source,"timeout_ms":1000}))
        .unwrap();
    let id = first["job_id"]
        .as_u64()
        .unwrap_or_else(|| panic!("{first}"));
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let status = runtime.status(id).unwrap();
        assert_eq!(status["ok"], true, "{status}");
        if status["completed"] == true {
            return status;
        }
        assert!(
            Instant::now() < deadline,
            "text request did not finish: {status}"
        );
        std::thread::sleep(Duration::from_millis(1));
        runtime.poll().unwrap();
    }
}

#[test]
fn sync_and_async_services_use_the_same_real_font_provider() {
    execute(
        &mut runtime(),
        r#"
        local service = game:GetService("TextService")
        local family = "rbxasset://fonts/families/NotoSans.json"
        local info = service:GetFamilyInfoAsync(family)
        assert(info.Name == "Noto Sans" and #info.Faces == 1)
        local params = Instance.new("GetTextBoundsParams")
        params.Font = Font.new(family)
        params.Text = "Hello world"
        params.Size = 20
        params.Width = 55
        local async = service:GetTextBoundsAsync(params)
        local sync = service:GetTextSize(params.Text, 20, Enum.Font.SourceSans, Vector2.new(55, 1000))
        assert(async == sync, tostring(async).." ~= "..tostring(sync))
        assert(async.Y == 40 and async.X <= 55)
        params.RichText = true
        params.Text = '<font size="40">Hi</font>!'
        params.Width = 0
        assert(service:GetTextBoundsAsync(params).Y == 40)
        params.RichText = false
        params.Font = Font.new("rbxasset://fonts/families/NotoNaskhArabic.json")
        params.Text = "سلام 123 سلام"
        local arabic = service:GetTextBoundsAsync(params)
        assert(arabic.X > 0 and arabic.Y == 20)
        return true
    "#,
    );
}

#[test]
fn gui_bounds_wrapping_padding_scaling_and_automatic_size_use_real_measurements() {
    execute(
        &mut runtime(),
        r#"
        local screen = Instance.new("ScreenGui", game:GetService("CoreGui"))
        local text = Instance.new("TextLabel", screen)
        text.Position = UDim2.new()
        text.FontFace = Font.new("rbxasset://fonts/families/NotoSans.json")
        text.TextSize = 20
        text.Text = "Hello world"
        text.Size = UDim2.fromOffset(300, 80)
        local service = game:GetService("TextService")
        local expected = service:GetTextSize(text.Text, 20, Enum.Font.SourceSans, Vector2.new(0, 0))
        assert(text.TextBounds == expected, tostring(text.TextBounds).." ~= "..tostring(expected))
        assert(text.TextFits)
        assert(not pcall(function() text.TextBounds = Vector2.zero end))
        assert(not pcall(function() text.TextFits = false end))
        text.Size = UDim2.fromOffset(65, 30)
        text.TextWrapped = true
        local padding = Instance.new("UIPadding", text)
        padding.PaddingLeft = UDim.new(0, 5)
        padding.PaddingRight = UDim.new(0, 5)
        assert(text.TextBounds.Y == 40 and not text.TextFits)
        text.Size = UDim2.fromOffset(65, 50)
        assert(text.TextFits)
        padding:Destroy()
        text.TextWrapped = false
        text.Size = UDim2.fromOffset(0, 0)
        text.AutomaticSize = Enum.AutomaticSize.XY
        assert(text.AbsoluteSize == expected, tostring(text.AbsoluteSize).." ~= "..tostring(expected))
        text.AutomaticSize = Enum.AutomaticSize.None
        text.Size = UDim2.fromOffset(300, 80)
        local scale = Instance.new("UIScale", screen)
        scale.Scale = 2
        -- Scale on the text object changes both its box and its text metrics.
        scale.Parent = text
        assert(text.TextBounds == expected * 2)
        scale:Destroy()
        text.TextScaled = true
        local constraint = Instance.new("UITextSizeConstraint", text)
        constraint.MinTextSize = 10
        constraint.MaxTextSize = 25
        assert(text.TextBounds.Y == 25 and text.TextFits)
        text.RichText = true
        text.Text = '<font size="40">Hi</font>'
        assert(text.TextBounds.Y == 50 and text.TextFits)
        text.TextScaled = false
        constraint.MaxTextSize = 10
        assert(text.TextBounds.Y == 20 and text.TextFits)
        text.TextScaled = true
        text.Size = UDim2.fromOffset(1, 1)
        assert(not text.TextFits)
        return true
    "#,
    );
}

#[test]
fn disabled_provider_and_missing_glyphs_return_errors_without_poisoning_other_runtimes() {
    let source = r#"return game:GetService("TextService"):GetTextSize("Hello",20,Enum.Font.SourceSans,Vector2.zero).X"#;
    let mut good = runtime();
    let width = execute(&mut good, source)["values"][0].clone();
    let mut disabled = Runtime::with_text_backend(snapshot(), 32, None).unwrap();
    execute(
        &mut disabled,
        r#"
        local service = game:GetService("TextService")
        local ok, message = pcall(function() return service:GetTextSize("Hello",20,Enum.Font.SourceSans,Vector2.zero) end)
        assert(not ok and string.find(tostring(message), "not configured", 1, true))
        return true
    "#,
    );
    assert_eq!(execute(&mut good, source)["values"][0], width);
    execute(
        &mut good,
        r#"
        local service = game:GetService("TextService")
        local ok, message = pcall(function() return service:GetTextSize(utf8.char(0x10ffff),20,Enum.Font.SourceSans,Vector2.zero) end)
        assert(not ok and string.find(tostring(message), "required glyph", 1, true))
        return true
    "#,
    );
}

#[test]
fn cold_synchronous_font_work_after_a_yield_keeps_the_authored_task_budget() {
    struct ColdFont(headless_luau::engine_services::local_text::LocalTextBackend);
    impl headless_luau::engine_services::TextServiceBackend for ColdFont {
        fn handle(
            &self,
            request: &headless_luau::engine_services::TextRequest,
        ) -> Result<Value, String> {
            // Reproduce a cold disk/font parse taking longer than a 5 ms host
            // poll. The measurement still uses the real pinned font file.
            std::thread::sleep(Duration::from_millis(15));
            self.0.handle(request)
        }
    }
    let mut runtime = Runtime::with_text_backend(
        snapshot(),
        32,
        Some(Arc::new(ColdFont(LocalTextBackend::new(
            root().join("tests/fixtures"),
        )))),
    )
    .unwrap();
    execute(
        &mut runtime,
        r#"
        task.wait()
        local bounds = game:GetService("TextService"):GetTextSize("Hello",20,Enum.Font.SourceSans,Vector2.zero)
        local after = 0
        for i = 1,100000 do after += i end
        assert(bounds.X == 49 and bounds.Y == 20 and after == 5000050000)
        return true
    "#,
    );
}

#[test]
fn executable_content_root_applies_to_synchronous_text_service() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_rove"))
        .args(["run", "--snapshot"])
        .arg(root().join("tests/fixtures/world_snapshot.json"))
        .arg("--content-root")
        .arg(root().join("tests/fixtures"))
        .arg("--script")
        .arg(root().join("examples/text-service-smoke.luau"))
        .arg("--json")
        .env(
            "ROBLOX_CONTENT_DIR",
            root().join("deliberately-missing-font-directory"),
        )
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .rev()
        .find_map(|line| serde_json::from_str(line).ok())
        .unwrap();
    assert_eq!(value["ok"], true, "{value}");
    assert_eq!(value["values"][0]["passed"], 5.0, "{value}");
}
