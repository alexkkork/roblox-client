use super::*;
use std::{cell::RefCell, rc::Rc};

fn runtime() -> (Lua, WorldRef) {
    let lua = Lua::new();
    crate::rust_host::numeric::install(&lua).unwrap();
    crate::rust_host::enums::install(&lua).unwrap();
    crate::rust_host::cframe::install(&lua).unwrap();
    let world = Rc::new(RefCell::new(
        World::from_snapshot(&json!({"version":1,"sequence":0,"nodes":[]}), None).unwrap(),
    ));
    install(&lua, &world).unwrap();
    (lua, world)
}

#[test]
fn region3_and_ray_match_tagged_json_and_geometry() {
    let (lua, _) = runtime();
    lua.load(
        r#"
        local r=Region3.new(Vector3.new(-1,2,3),Vector3.new(4,6,8))
        assert(r.Size == Vector3.new(5,4,5))
        assert(r:ExpandToGrid(2).Size == Vector3.new(6,4,6))
        local ray=Ray.new(Vector3.new(0,0,0),Vector3.new(0,1,0))
        assert(ray:ClosestPoint(Vector3.new(4,3,1)) == Vector3.new(0,3,0))
        assert(ray:Distance(Vector3.new(4,3,1)) == math.sqrt(17))
        region_value=r; ray_value=ray
    "#,
    )
    .exec()
    .unwrap();
    let region: AnyUserData = lua.globals().get("region_value").unwrap();
    assert_eq!(
        enc_region(&region).unwrap().unwrap()["$region3"][0],
        json!([-1.0, 2.0, 3.0])
    );
    let ray: AnyUserData = lua.globals().get("ray_value").unwrap();
    assert_eq!(
        enc_ray(&ray).unwrap().unwrap()["$ray"][1],
        json!([0.0, 1.0, 0.0])
    );
}

#[test]
fn mutable_catalog_and_query_params_keep_defaults_and_validate_writes() {
    let (lua, _) = runtime();
    let ok:bool=lua.load(r#"
        local c=CatalogSearchParams.new()
        assert(c.Limit==30 and c.IncludeOffSale==false and c.SearchKeyword=="")
        c.Limit=120; c.SearchKeyword="hats"; c.IncludeOffSale=true
        local p=RaycastParams.new()
        assert(typeof(p)=="RaycastParams")
        p.Margin=2; p.IgnoreWater=true; p.FilterDescendantsInstances={}
        local o=OverlapParams.new(); o.MaxParts=7; o.Tolerance=0.2
        assert(typeof(o)=="OverlapParams")
        return c.Limit==120 and c.SearchKeyword=="hats" and c.IncludeOffSale and p.Margin==2 and p.IgnoreWater and o.MaxParts==7 and o.Tolerance==0.05
    "#).eval().unwrap();
    assert!(ok);
}

#[test]
fn immutable_values_reject_writes_and_cell_id_roundtrips() {
    let (lua, _) = runtime();
    let ok: bool = lua
        .load(
            r#"
        local c=CellId.new(true,1,2,3)
        assert(c.IsNil and c.Location==Vector3.new(1,2,3) and c.TerrainPart==nil)
        local f=Font.new("BuilderSans",Enum.FontWeight.Bold,Enum.FontStyle.Normal)
        assert(f.Bold and f.Family=="BuilderSans")
        local ok=pcall(function() Region3.new(Vector3.zero,Vector3.one).Size=Vector3.zero end)
        return not ok
    "#,
        )
        .eval()
        .unwrap();
    assert!(ok);
    let cell: AnyUserData = lua.load("return CellId.new(true,1,2,3)").eval().unwrap();
    assert_eq!(
        enc_cell(&cell).unwrap().unwrap()["$cell_id"]["location"],
        json!([1.0, 2.0, 3.0])
    );
}

#[test]
fn plugin_content_directions_and_dock_info_keep_typed_values() {
    let (lua, _) = runtime();
    let ok: bool = lua
        .load(
            r#"
        local d=PluginDrag.new("sender","text/plain","payload","mouse","drag",Vector2.new(2,3))
        assert(d.Sender=="sender" and d.HotSpot.X==2 and d.HotSpot.Y==3)
        local axes=Axes.new(Enum.Axis.X,Enum.NormalId.Top)
        assert(typeof(axes)=="Axes")
        assert(axes.X and axes.Y and not axes.Z)
        local faces=Faces.new(Enum.NormalId.Front,Enum.NormalId.Left)
        assert(typeof(faces)=="Faces")
        assert(faces.Front and faces.Left and not faces.Right)
        local c=Content.fromUri("rbxasset://hello")
        assert(c.Uri=="rbxasset://hello" and c.Object==nil)
        local dock=DockWidgetPluginGuiInfo.new(Enum.InitialDockState.Float,true,false,100,90,40,30)
        assert(dock.InitialDockState==Enum.InitialDockState.Float)
        assert(not pcall(function() DockWidgetPluginGuiInfo.new(Enum.Axis.X,true,false,100,90,40,30) end))
        assert(not pcall(function() DockWidgetPluginGuiInfo.new(Enum.InitialDockState.Float,1,false,100,90,40,30) end))
        assert(not pcall(function() DockWidgetPluginGuiInfo.new(Enum.InitialDockState.Float,true,nil,100,90,40,30) end))
        dock_fixture=dock
        plugin_value=d; content_value=c; direction_value=axes
        return true
    "#,
        )
        .eval()
        .unwrap();
    assert!(ok);
    let plugin: AnyUserData = lua.globals().get("plugin_value").unwrap();
    assert_eq!(
        enc_plugin_drag(&plugin).unwrap().unwrap()["$plugin_drag"]["hot_spot"],
        json!([2.0, 3.0])
    );
    let content: AnyUserData = lua.globals().get("content_value").unwrap();
    assert_eq!(
        enc_content(&content).unwrap().unwrap()["$content"]["source_type"],
        json!(1)
    );
    let dock_json = json!({"$dock_widget_plugin_gui_info":{"enabled":true,"state":4,"override_enabled":false,"width":100.0,"height":90.0,"min_width":40.0,"min_height":30.0}});
    let dock: AnyUserData = lua.globals().get("dock_fixture").unwrap();
    let ok:bool=lua.load("return dock_fixture.InitialEnabled and dock_fixture.FloatingXSize==100 and dock_fixture.MinHeight==30").eval().unwrap();
    assert!(ok);
    assert_eq!(enc_dock(&dock).unwrap().unwrap(), dock_json);
}

#[test]
fn object_and_spatial_filters_reject_cross_world_handles_even_with_same_id() {
    let (lua, own) = runtime();
    own.borrow_mut().nodes.insert(
        "same".into(),
        world::Node {
            id: "same".into(),
            class_name: "Part".into(),
            name: "Part".into(),
            parent: None,
            destroyed: false,
            server_owned: true,
            properties: serde_json::Map::new(),
            local_overrides: serde_json::Map::new(),
            attributes: serde_json::Map::new(),
            attributes_known: false,
            revisions: serde_json::Map::new(),
            local_revision: 0,
            collection_tags: Vec::new(),
            collection_tags_known: false,
            projected: None,
        },
    );
    let foreign = Rc::new(RefCell::new(
        World::from_snapshot(
            &json!({"version":1,"sequence":0,"nodes":[{"id":"same","class_name":"Part"}]}),
            None,
        )
        .unwrap(),
    ));
    let handle = world::instance(&lua, &foreign, "same").unwrap();
    lua.globals().set("foreign_instance", handle).unwrap();
    let rejected:bool=lua.load(r#"
        local content_ok=pcall(function() Content.fromObject(foreign_instance) end)
        local filter_ok=pcall(function() local p=RaycastParams.new(); p.FilterDescendantsInstances={foreign_instance} end)
        return not content_ok and not filter_ok
    "#).eval().unwrap();
    assert!(rejected);
}
