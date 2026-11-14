use super::*;
use serde_json::json;
use std::{cell::RefCell, rc::Rc};

fn snapshot() -> WorldRef {
    let data = json!({"version":1,"sequence":7,"local_player":"player","display":{"size":[800,600],"device_insets":[0,24,0,0],"core_insets":[0,24,0,0]},
    "nodes":[
        {"id":"game","class_name":"DataModel","name":"game","parent":null,"properties":{}},
        {"id":"player","class_name":"Player","name":"tester","parent":"game","properties":{"DisplayName":"Tester"}},
        {"id":"pg","class_name":"PlayerGui","name":"PlayerGui","parent":"player","properties":{}},
        {"id":"screen","class_name":"ScreenGui","name":"ScreenGui","parent":"pg","properties":{}},
        {"id":"frame","class_name":"Frame","name":"Panel","parent":"screen","properties":{
            "Position":{"$udim2":[{"$udim":[0.5,0]},{"$udim":[0.5,0]}]},
            "Size":{"$udim2":[{"$udim":[0,200]},{"$udim":[0,100]}]},
            "AnchorPoint":{"$vector2":[0.5,0.5]},"Visible":true,"BackgroundTransparency":0.25}}
    ]});
    Rc::new(RefCell::new(
        world::World::from_snapshot(&data, None).unwrap(),
    ))
}

fn size_frame_for_layout(ctx: &WorldRef, lua: &mlua::Lua) {
    super::super::signals::install(lua, ctx).unwrap();
    world::set_property(
        lua,
        ctx,
        "frame",
        "Size",
        json!({"$udim2":[{"$udim":[0,100]},{"$udim":[0,100]}]}),
    )
    .unwrap();
    // Keep the authored top-left at x=300 while the centered frame shrinks
    // from 200 px to 100 px; its y center stays fixed in the safe area.
    world::set_property(
        lua,
        ctx,
        "frame",
        "Position",
        json!({"$udim2":[{"$udim":[0,350]},{"$udim":[0.5,0]}]}),
    )
    .unwrap();
}

#[test]
fn screen_gui_uses_snapshot_viewport_and_insets() {
    let ctx = snapshot();
    let d = display(&ctx);
    let mut cache = HashMap::new();
    let b = box_for(&ctx, "screen", &d, &mut cache, &mut HashSet::new(), 0).unwrap();
    assert_eq!([b.x, b.y, b.w, b.h], [0., 0., 800., 576.]);
}

#[test]
fn udim2_anchor_and_scale_project_to_expected_box() {
    let ctx = snapshot();
    let d = display(&ctx);
    let mut cache = HashMap::new();
    let b = box_for(&ctx, "frame", &d, &mut cache, &mut HashSet::new(), 0).unwrap();
    assert_eq!([b.x, b.y, b.w, b.h], [300., 238., 200., 100.]);
}

#[test]
fn render_keeps_v2_schema_and_projects_gui_player() {
    let ctx = snapshot();
    let frame = render(&ctx).unwrap();
    assert_eq!(frame["version"], 2);
    assert_eq!(frame["sequence"], 7);
    assert_eq!(frame["viewport"], json!([800, 600]));
    assert_eq!(frame["players"][0]["name"], "tester");
    assert_eq!(frame["gui"][0]["box"], json!([300., 238., 200., 100., 0.]));
}

#[test]
fn automatic_sizing_keeps_authored_minimum_for_empty_content() {
    let ctx = snapshot();
    let lua = mlua::Lua::new();
    super::super::signals::install(&lua, &ctx).unwrap();
    world::set_property(
        &lua,
        &ctx,
        "frame",
        "AutomaticSize",
        json!({"$enum":{"type":"AutomaticSize","value":2}}),
    )
    .unwrap();
    let d = display(&ctx);
    let mut cache = HashMap::new();
    let b = box_for(&ctx, "frame", &d, &mut cache, &mut HashSet::new(), 0).unwrap();
    assert_eq!([b.x, b.y, b.w, b.h], [300., 238., 200., 100.]);
}

#[test]
fn rich_text_decodes_entities_and_keeps_nested_unicode_runs() {
    let (text, runs) = rich_text("a&amp;<b>猫 e\u{301}</b><br/>z").unwrap();
    assert_eq!(text, "a&猫 e\u{301}\nz");
    assert_eq!(runs[1]["text"], "猫 e\u{301}");
    assert_eq!(runs[1]["style"]["weight"], 700);
}

#[test]
fn malformed_rich_text_is_not_interpreted_as_markup() {
    assert!(rich_text("<b>open").is_none());
    assert!(rich_text("<font color=red>x</font>").is_none());
}

#[test]
fn ui_list_layout_places_children_in_stable_name_order() {
    let ctx = snapshot();
    let lua = mlua::Lua::new();
    size_frame_for_layout(&ctx, &lua);
    let make_size = json!({"$udim2":[{"$udim":[0,20]},{"$udim":[0,10]}]});
    let delta = json!({"version":1,"delta":true,"sequence":10,"actions":[
        {"kind":"create","sequence":8,"node":{"id":"list","class_name":"UIListLayout","name":"List","parent":"frame","properties":{"FillDirection":{"$enum":{"type":"FillDirection","value":1}},"SortOrder":{"$enum":{"type":"SortOrder","value":0}},"HorizontalAlignment":{"$enum":{"type":"HorizontalAlignment","value":1}},"VerticalAlignment":{"$enum":{"type":"VerticalAlignment","value":1}},"Padding":{"$udim":[0,0]},"Wraps":false}}},
        {"kind":"create","sequence":9,"node":{"id":"a","class_name":"Frame","name":"A","parent":"frame","properties":{"Size":make_size,"Position":{"$udim2":[{"$udim":[0,0]},{"$udim":[0,0]}]},"AnchorPoint":{"$vector2":[0,0]},"Visible":true}}},
        {"kind":"create","sequence":10,"node":{"id":"b","class_name":"Frame","name":"B","parent":"frame","properties":{"Size":make_size,"Position":{"$udim2":[{"$udim":[0,0]},{"$udim":[0,0]}]},"AnchorPoint":{"$vector2":[0,0]},"Visible":true}}}
    ]});
    ctx.borrow_mut().apply_delta(&delta, None).unwrap();
    assert_eq!(layout_bounds(&ctx, "a").unwrap()[..2], [300., 238.]);
    assert_eq!(layout_bounds(&ctx, "b").unwrap()[..2], [300., 248.]);
}

#[test]
fn ui_grid_layout_uses_cell_stride_and_alignment() {
    let ctx = snapshot();
    let lua = mlua::Lua::new();
    size_frame_for_layout(&ctx, &lua);
    let cell = json!({"$udim2":[{"$udim":[0,20]},{"$udim":[0,10]}]});
    let padding = json!({"$udim2":[{"$udim":[0,5]},{"$udim":[0,5]}]});
    let child_size = json!({"$udim2":[{"$udim":[0,5]},{"$udim":[0,5]}]});
    let delta = json!({"version":1,"delta":true,"sequence":10,"actions":[
        {"kind":"create","sequence":8,"node":{"id":"grid","class_name":"UIGridLayout","name":"Grid","parent":"frame","properties":{"CellSize":cell,"CellPadding":padding,"FillDirection":{"$enum":{"type":"FillDirection","value":0}},"SortOrder":{"$enum":{"type":"SortOrder","value":0}},"HorizontalAlignment":{"$enum":{"type":"HorizontalAlignment","value":1}},"VerticalAlignment":{"$enum":{"type":"VerticalAlignment","value":1}},"StartCorner":{"$enum":{"type":"StartCorner","value":0}},"FillDirectionMaxCells":0}}},
        {"kind":"create","sequence":9,"node":{"id":"a","class_name":"Frame","name":"A","parent":"frame","properties":{"Size":child_size,"Position":{"$udim2":[{"$udim":[0,0]},{"$udim":[0,0]}]},"AnchorPoint":{"$vector2":[0,0]},"Visible":true}}},
        {"kind":"create","sequence":10,"node":{"id":"b","class_name":"Frame","name":"B","parent":"frame","properties":{"Size":child_size,"Position":{"$udim2":[{"$udim":[0,0]},{"$udim":[0,0]}]},"AnchorPoint":{"$vector2":[0,0]},"Visible":true}}}
    ]});
    ctx.borrow_mut().apply_delta(&delta, None).unwrap();
    assert_eq!(
        layout_bounds(&ctx, "a").unwrap(),
        [300., 238., 20., 10., 0.]
    );
    assert_eq!(
        layout_bounds(&ctx, "b").unwrap(),
        [325., 238., 20., 10., 0.]
    );
}

#[test]
fn ui_list_fill_flex_distributes_main_axis_space() {
    let ctx = snapshot();
    let lua = mlua::Lua::new();
    size_frame_for_layout(&ctx, &lua);
    let size = json!({"$udim2":[{"$udim":[0,20]},{"$udim":[0,10]}]});
    let delta = json!({"version":1,"delta":true,"sequence":10,"actions":[
        {"kind":"create","sequence":8,"node":{"id":"list","class_name":"UIListLayout","name":"List","parent":"frame","properties":{"FillDirection":{"$enum":{"type":"FillDirection","value":1}},"VerticalFlex":{"$enum":{"type":"UIFlexAlignment","value":1}},"SortOrder":{"$enum":{"type":"SortOrder","value":0}},"HorizontalAlignment":{"$enum":{"type":"HorizontalAlignment","value":1}},"VerticalAlignment":{"$enum":{"type":"VerticalAlignment","value":1}},"Padding":{"$udim":[0,0]},"Wraps":false}}},
        {"kind":"create","sequence":9,"node":{"id":"a","class_name":"Frame","name":"A","parent":"frame","properties":{"Size":size,"Position":{"$udim2":[{"$udim":[0,0]},{"$udim":[0,0]}]},"AnchorPoint":{"$vector2":[0,0]},"Visible":true}}},
        {"kind":"create","sequence":10,"node":{"id":"b","class_name":"Frame","name":"B","parent":"frame","properties":{"Size":size,"Position":{"$udim2":[{"$udim":[0,0]},{"$udim":[0,0]}]},"AnchorPoint":{"$vector2":[0,0]},"Visible":true}}}
    ]});
    ctx.borrow_mut().apply_delta(&delta, None).unwrap();
    assert_eq!(
        layout_bounds(&ctx, "frame").unwrap(),
        [300., 238., 100., 100., 0.]
    );
    assert_eq!(
        enum_value(&prop(&ctx, "list", "VerticalFlex", Json::Null)),
        Some(1)
    );
    assert_eq!(
        enum_value(&prop(&ctx, "list", "FillDirection", Json::Null)),
        Some(1)
    );
    assert!(component(&ctx, "a", "UIFlexItem").is_none());
    assert!(component(&ctx, "b", "UIFlexItem").is_none());
    let a = layout_bounds(&ctx, "a").unwrap();
    let b = layout_bounds(&ctx, "b").unwrap();
    assert_eq!(a[3], 50.);
    assert_eq!(b[3], 50.);
    assert_eq!(b[1], 288.);
}

#[test]
fn scrolling_frame_applies_pixel_delta_and_clamps_enabled_axis() {
    let ctx = snapshot();
    let delta = json!({"version":1,"delta":true,"sequence":8,"actions":[
        {"kind":"create","sequence":8,"node":{"id":"scroll","class_name":"ScrollingFrame","name":"Scroll","parent":"screen","properties":{
            "Position":{"$udim2":[{"$udim":[0,0]},{"$udim":[0,0]}]},
            "Size":{"$udim2":[{"$udim":[0,100]},{"$udim":[0,100]}]},
            "CanvasSize":{"$udim2":[{"$udim":[0,300]},{"$udim":[0,200]}]},
            "ScrollingDirection":{"$enum":{"type":"ScrollingDirection","name":"X","value":1}},
            "CanvasPosition":{"$vector2":[0,0]}
        }}}
    ]});
    ctx.borrow_mut().apply_delta(&delta, None).unwrap();

    assert_eq!(scroll_by(&ctx, "scroll", [500., 60.]).unwrap(), [200., 0.]);
    assert_eq!(
        vector2(&prop(
            &ctx,
            "scroll",
            "CanvasPosition",
            json!({"$vector2":[0,0]})
        )),
        Some([200., 0.])
    );
    assert_eq!(scroll_by(&ctx, "scroll", [50., 0.]).unwrap(), [0., 0.]);
}
