use super::*;
use mlua::{Lua, Value};

fn runtime() -> Lua {
    let lua = Lua::new();
    install(&lua, "12345678-1234-4234-9234-123456789abc").unwrap();
    lua
}

#[test]
fn only_runservice_is_enabled_and_has_live_client_mode() {
    let lua = runtime();
    let good: bool = lua
        .load(
            r#"
        local run=game:GetService("RunService")
        assert(game.JobId=="12345678-1234-4234-9234-123456789abc")
        assert(run.Name=="RunService" and run.ClassName=="RunService")
        assert(run.Parent==game and run:IsA("Instance"))
        assert(run:IsStudio()==false and run:IsRunning()==true)
        assert(game:FindService("RunService")==run)
        assert(game:FindFirstChild("RunService")==run)
        assert(#game:GetChildren()==1 and game:GetChildren()[1]==run)
        assert(game:FindService("Workspace")==nil)
        assert(game:FindFirstChild("Workspace")==nil)
        return true
    "#,
        )
        .eval()
        .unwrap();
    assert!(good);
    let unavailable = lua
        .load("return game:GetService('HttpService')")
        .eval::<Value>();
    assert!(unavailable
        .unwrap_err()
        .to_string()
        .contains("service 'HttpService' is not enabled"));
}

#[test]
fn global_settings_fixtures_keep_native_errors_and_names() {
    let lua = runtime();
    let good: bool = lua
        .load(
            r#"
        local user=UserSettings()
        local global=settings()
        assert(user.Name=="GlobalBasicSettings" and user.ClassName=="UserSettings")
        assert(global.Name=="GlobalSettings" and global:IsA("GenericSettings"))
        return true
    "#,
        )
        .eval()
        .unwrap();
    assert!(good);
    assert!(lua
        .load("UserSettings(true)")
        .exec()
        .unwrap_err()
        .to_string()
        .contains("UserSettings takes no arguments"));
    assert!(lua
        .load("settings('x')")
        .exec()
        .unwrap_err()
        .to_string()
        .contains("settings takes no arguments"));
    assert!(lua
        .load("settings():GetFFlag('Anything')")
        .exec()
        .unwrap_err()
        .to_string()
        .contains("engine flag values are unavailable in this local runtime"));
    assert!(lua
        .load("game.JobId='forged'")
        .exec()
        .unwrap_err()
        .to_string()
        .contains("local Instance properties are read-only"));
}
