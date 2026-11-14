-- run with --content-root tests/fixtures to use the bundled open font fixtures.
local text = game:GetService("TextService")
local font = Enum.Font.SourceSans
local function measure(s, size, width)
    return text:GetTextSize(s, size or 20, font, Vector2.new(width or 0, 0))
end
assert(measure("WWWW").X > measure("iiii").X)
assert(measure("é") == measure("e\u{301}"))
assert(measure("Hello world", 20, 55).Y == 40)
assert(measure("A\nB").Y == 40)
assert(measure("") == Vector2.zero)
return {passed = 5, backend = "rust font shaping", live_game_session = false}
