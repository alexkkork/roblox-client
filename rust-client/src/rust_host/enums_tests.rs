use super::{enum_item_matches, install};
use crate::rust_host::values;
use mlua::{Lua, Value};
use serde_json::json;

fn runtime() -> Lua {
    let lua = Lua::new();
    install(&lua).expect("Enum install");
    lua
}

#[test]
fn enum_items_are_canonical_read_only_and_have_aliases() {
    let lua = runtime();
    let (same, val, name, type_name, alias, count): (bool, i32, String, String, bool, usize) = lua
        .load(
            r#"
            local key = Enum.KeyCode.Unknown
            local items = Enum.KeyCode:GetEnumItems()
            return rawequal(key, Enum.KeyCode.None), key.Value, key.Name,
                tostring(key.EnumType),
                Enum.RaycastFilterType.Blacklist == Enum.RaycastFilterType.Exclude,
                #Enum:GetEnums()
        "#,
        )
        .eval()
        .expect("enum behavior");
    assert!(same);
    assert_eq!((val, name, type_name), (0, "None".into(), "KeyCode".into()));
    assert!(alias);
    assert!(count >= 80);
    let write_ok: bool = lua
        .load("return not pcall(function() Enum.KeyCode.None.Value = 7 end) and not pcall(function() Enum.Other = 1 end)")
        .eval()
        .expect("enum write probe");
    assert!(write_ok);
}

#[test]
fn enum_unknown_member_rejected_and_tag_round_trip_preserves_identity() {
    let lua = runtime();
    let unknown: bool = lua
        .load("return not pcall(function() return Enum.KeyCode.NotARealKey end)")
        .eval()
        .expect("unknown enum probe");
    assert!(unknown);
    let tag = json!({"$enum":{"type":"EasingStyle","name":"Bounce","value":6}});
    let decoded = values::from_json(&lua, &tag).expect("decode enum");
    let encoded = values::to_json(&lua, &decoded).expect("encode enum");
    assert_eq!(encoded, tag);
    let value_only = values::from_json(
        &lua,
        &json!({"$enum":{"type":"PathWaypointAction","value":1}}),
    )
    .expect("decode compact enum");
    let Value::UserData(item) = value_only else {
        panic!("expected EnumItem")
    };
    assert!(enum_item_matches(&item, "PathWaypointAction").expect("item type"));
}
