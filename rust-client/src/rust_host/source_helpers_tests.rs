use super::*;
use crate::rust_host::world::World;
use serde_json::json;
use std::{cell::RefCell, rc::Rc};

fn test_lua() -> (Lua, WorldRef) {
    let lua = Lua::new();
    let world = Rc::new(RefCell::new(
        World::from_snapshot(&json!({"version":1,"sequence":0,"nodes": []}), None).unwrap(),
    ));
    super::super::integrity::install(&lua).unwrap();
    install(&lua, &world).unwrap();
    (lua, world)
}

#[test]
fn session_environments_keep_identity_and_are_isolated() {
    let (lua, _) = test_lua();
    lua.load("shared.answer = 42; getgenv().local_value = 9")
        .exec()
        .unwrap();
    assert_eq!(lua.load("shared.answer").eval::<i64>().unwrap(), 42);
    assert_eq!(lua.load("getgenv().local_value").eval::<i64>().unwrap(), 9);
}

#[test]
fn loadstring_compiles_and_returns_lua_error_pair_on_bad_source() {
    let (lua, _) = test_lua();
    assert_eq!(
        lua.load("local f = assert(loadstring('return 19')); return f()")
            .eval::<i64>()
            .unwrap(),
        19
    );
    let (is_nil, has_error): (bool, bool) = lua
        .load("local f,e=loadstring('local ='); return f==nil, type(e)=='string'")
        .eval()
        .unwrap();
    assert!(is_nil && has_error);
}

#[test]
fn workspace_files_are_vm_local_and_reject_escape_paths() {
    let (lua, _) = test_lua();
    lua.load("makefolder('a/b'); writefile('a/b/test.lua','return 7')")
        .exec()
        .unwrap();
    assert_eq!(
        lua.load("assert(isfile('a/b/test.lua')); return dofile('a/b/test.lua')")
            .eval::<i64>()
            .unwrap(),
        7
    );
    assert!(lua.load("writefile('../outside','x')").exec().is_err());
    assert!(lua
        .load("return isfile('../outside')")
        .eval::<bool>()
        .is_err());
}

#[test]
fn local_print_and_clipboard_are_captured_without_os_side_effects() {
    let (lua, _) = test_lua();
    lua.load("print('hello', 5); warn('careful'); setclipboard('local only')")
        .exec()
        .unwrap();
    let clipboard: String = lua.load("return getclipboard()").eval().unwrap();
    assert_eq!(clipboard, "local only");
    let messages: Table = lua.load("return takeconsole()").eval().unwrap();
    assert_eq!(
        messages
            .raw_get::<Table>(1)
            .unwrap()
            .get::<String>("text")
            .unwrap(),
        "hello\t5"
    );
    assert_eq!(
        messages
            .raw_get::<Table>(2)
            .unwrap()
            .get::<String>("level")
            .unwrap(),
        "warn"
    );
}

#[test]
fn json_helpers_use_the_registered_world_value_codec() {
    let (lua, _) = test_lua();
    let result: String = lua
        .load("return jsonencode({x=3, ok=true})")
        .eval()
        .unwrap();
    assert!(result.contains("\"x\":3"));
    assert_eq!(
        lua.load("local t=jsondecode('{\"x\":3}'); return t.x")
            .eval::<i64>()
            .unwrap(),
        3
    );
}

#[test]
fn base64_and_sha256_use_bounded_rust_codecs() {
    let (lua, _) = test_lua();
    assert_eq!(
        lua.load("return base64decode(base64encode('hi'))")
            .eval::<String>()
            .unwrap(),
        "hi"
    );
    assert_eq!(
        lua.load("return crypt.hash('abc','sha256')")
            .eval::<String>()
            .unwrap(),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert_eq!(
        lua.load("return crypt.hash('abc','blake3')")
            .eval::<String>()
            .unwrap(),
        "6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85"
    );
}

#[test]
fn custom_asset_resolution_is_workspace_scoped() {
    let (lua, _) = test_lua();
    lua.load("writefile('folder/a b.png','asset')")
        .exec()
        .unwrap();
    let uri: String = lua
        .load("return getcustomasset('folder/a b.png')")
        .eval()
        .unwrap();
    assert_eq!(resolve_workspace_asset(&lua, &uri).unwrap(), b"asset");
    assert!(resolve_workspace_asset(&lua, "rbxasset://workspace/../../secret").is_none());
}

#[test]
fn lz4_raw_codec_roundtrips_and_checks_declared_size() {
    let (lua, _) = test_lua();
    assert_eq!(
        lua.load("return lz4decompress(lz4compress('hello'))")
            .eval::<String>()
            .unwrap(),
        "hello"
    );
    assert!(lua
        .load("return lz4decompress(lz4compress('hello'), 4)")
        .eval::<String>()
        .is_err());
    assert!(lua
        .load("return lz4decompress('\\0\\0\\0')")
        .eval::<String>()
        .is_err());
}
