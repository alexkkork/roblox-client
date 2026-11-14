use super::errors;
use mlua::{Lua, MultiValue, Value};

#[test]
fn protected_calls_convert_native_callback_errors_to_strings() {
    let lua = Lua::new();
    lua.globals()
        .set(
            "native_failure",
            lua.create_function(|_, ()| -> mlua::Result<()> {
                Err(mlua::Error::RuntimeError("native callback failed".into()))
            })
            .unwrap(),
        )
        .unwrap();
    let raw: MultiValue = lua.load("return pcall(native_failure)").eval().unwrap();
    assert_eq!(raw.front(), Some(&Value::Boolean(false)));
    let Some(Value::Error(error)) = raw.get(1) else {
        panic!("Rust callback failure was not returned as mlua::Value::Error: {raw:?}");
    };
    assert!(error.to_string().contains("native callback failed"));
    errors::install(&lua).unwrap();
    lua.load(
        r#"
        local ok, message = pcall(native_failure)
        assert(not ok and type(message) == "string",
            "native pcall error type=" .. type(message) .. "; value=" .. tostring(message))
        assert(string.find(message, "native callback failed", 1, true))

        local observed
        local xok, xmessage = xpcall(native_failure, function(err)
            observed = err
            return err
        end)
        assert(not xok and type(observed) == "string" and type(xmessage) == "string",
            "native xpcall error types=" .. type(observed) .. "/" .. type(xmessage) ..
            "; values=" .. tostring(observed) .. "/" .. tostring(xmessage))
        assert(string.find(observed, "native callback failed", 1, true))
        "#,
    )
    .exec()
    .unwrap();
}

#[test]
fn protected_calls_preserve_user_error_values_and_return_tuples() {
    let lua = Lua::new();
    errors::install(&lua).unwrap();
    lua.load(
        r#"
        local payload = {marker = "authored"}
        local ok, err = pcall(function() error(payload) end)
        assert(not ok and rawequal(err, payload))

        local observed
        local xok, xerr = xpcall(function() error(payload) end, function(err)
            observed = err
            return err
        end)
        assert(not xok and rawequal(observed, payload) and rawequal(xerr, payload))

        local result = table.pack(pcall(function() return "first", nil, "third" end))
        assert(result.n == 4 and result[1] and result[2] == "first")
        assert(result[3] == nil and result[4] == "third")
        "#,
    )
    .exec()
    .unwrap();
}
