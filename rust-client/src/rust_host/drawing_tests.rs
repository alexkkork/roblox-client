use super::*;
use serde_json::json;

fn setup() -> Result<(Lua, WorldRef)> {
    let data = json!({"version":1,"sequence":1,"nodes":[]});
    let world = Rc::new(RefCell::new(
        super::super::world::World::from_snapshot(&data, None).map_err(error)?,
    ));
    let lua = Lua::new();
    numeric::install(&lua)?;
    install(&lua, &world)?;
    Ok((lua, world))
}

#[test]
fn drawing_objects_validate_properties_and_remove_from_scene() -> Result<()> {
    let (lua, world) = setup()?;
    lua.load(
        r#"
        local object = Drawing.new("Square")
        assert(object.Visible == false)
        assert(object.__OBJECT_EXISTS == true)
        object.Visible = true
        object.Position = Vector2.new(12, 24)
        object.Size = Vector2.new(20, 30)
        object.Color = Color3.new(0.25, 0.5, 1)
        assert(isrenderobj(object))
        assert(getrenderproperty(object, "Visible") == true)
        assert(not pcall(function() object.Size = 3 end))
        assert(not pcall(function() object.Missing = true end))
        local svg = Drawing.ToSVG(100, 100)
        assert(string.find(svg, "<rect", 1, true))
        object:Remove()
        assert(object.__OBJECT_EXISTS == false)
        assert(not isrenderobj(object))
        assert(Drawing.ToSVG(100, 100):find("<rect", 1, true) == nil)
        "#,
    )
    .exec()?;
    let scene = world.borrow().effects.get(SCENE_KEY).cloned().unwrap();
    assert_eq!(scene.as_array().unwrap().len(), 0);
    Ok(())
}

#[test]
fn every_drawing_kind_has_its_authored_property_shape() -> Result<()> {
    let (lua, _) = setup()?;
    lua.load(
        r#"
        local expected = {
            Square = {"Position", "Size", "Thickness", "Filled"},
            Line = {"From", "To", "Thickness"},
            Circle = {"Position", "Radius", "Thickness", "NumSides", "Filled"},
            Text = {"Position", "Text", "Size", "Font", "Center", "Outline", "OutlineColor", "OutlineOpacity", "TextBounds"},
            Image = {"Position", "Size", "Data", "Rounding"},
            Triangle = {"PointA", "PointB", "PointC", "Thickness", "Filled"},
            Quad = {"PointA", "PointB", "PointC", "PointD", "Thickness", "Filled"},
        }
        for kind, properties in expected do
            local object = Drawing.new(kind)
            for _, property in properties do
                assert(object[property] ~= nil, kind .. "." .. property .. " missing")
            end
        end
        assert(Drawing.Fonts.UI == 0 and Drawing.Fonts.Monospace == 3)
        assert(not pcall(function() Drawing.new("Unknown") end))
        "#,
    )
    .exec()?;
    Ok(())
}

#[test]
fn svg_is_sorted_escaped_and_projected_with_render_frame() -> Result<()> {
    let (lua, world) = setup()?;
    lua.load(
        r#"
        local back = Drawing.new("Text")
        back.Visible = true
        back.ZIndex = 1
        back.Position = Vector2.new(8, 9)
        back.Text = "<hello & world>"
        back.Transparency = 0
        local front = Drawing.new("Circle")
        front.Visible = true
        front.ZIndex = 2
        front.Position = Vector2.new(30, 40)
        front.Radius = 5
        front.NumSides = 0
        front.Transparency = 0
        local svg = Drawing.ToSVG(120, 80)
        assert(svg:find("&lt;hello &amp; world&gt;", 1, true))
        assert(svg:find("<polygon", 1, true))
        assert(svg:find("width=\"120\"", 1, true))
        assert(not pcall(function() Drawing.ToSVG(0, 10) end))
        "#,
    )
    .exec()?;
    publish(&lua)?;
    let frame = super::super::gui::render(&world)?;
    let objects = frame["drawings"]
        .as_array()
        .ok_or_else(|| error("missing drawings frame field"))?;
    assert_eq!(objects.len(), 2);
    assert_eq!(objects[0]["kind"], "Text");
    assert_eq!(objects[1]["kind"], "Circle");
    assert_eq!(frame["gui"].as_array().unwrap().len(), 1);
    assert_eq!(frame["gui"][0]["id"], "drawing:2");
    assert_eq!(frame["gui"][0]["cornerRadius"], 5.0);
    Ok(())
}
