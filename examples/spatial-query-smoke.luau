-- self-contained collision queries; no game server or asset downloads needed.
local world = Instance.new("WorldModel", workspace)
local source = Instance.new("Part", world)
source.Anchored = true
source.Size = Vector3.new(2,2,2)
source.CFrame = CFrame.new()
local target = Instance.new("Part", world)
target.Anchored = true
target.Size = Vector3.new(2,2,2)
target.CFrame = CFrame.new(10,0,0)
local checks = 0
local function check(ok, message)
    assert(ok, message)
    checks += 1
end
local function near(a,b) return math.abs(a-b) < .0001 end

local hit = world:Shapecast(source, Vector3.new(20,0,0))
check(typeof(hit) == "RaycastResult" and hit.Instance == target, "sweep target")
check(near(hit.Distance,8) and near(hit.Position.X,9), "first contact")
check(near(hit.Normal.X,-1), "contact normal")
check(world:Shapecast(source,Vector3.new(-20,0,0)) == nil, "miss")
local params = RaycastParams.new()
params.Margin = .5
params.SizeGradient = 1
check(near(world:Shapecast(source,Vector3.new(20,0,0),params).Distance,7.5/1.05), "growing margin")
hit = world:Shapecast(source,Vector3.new(20,0,0),nil,CFrame.new(3,0,0),Vector3.new(4,2,2))
check(near(hit.Distance,4) and source.CFrame.Position == Vector3.zero, "hypothetical pose")
target.CFrame = CFrame.new()
check(table.find(world:GetPartsInPart(source),target) ~= nil, "occupied volume")
check(world:Shapecast(source,Vector3.new(20,0,0)) == nil, "initial overlap")
source.Shape = Enum.PartType.Ball
source.Size = Vector3.new(4,4,4)
target.Size = Vector3.new(.2,.2,.2)
target.CFrame = CFrame.new(1.8,1.8,1.8)
check(#world:GetPartsInPart(source) == 0, "empty sphere corner")
target.CFrame = CFrame.new()
local overlaps = OverlapParams.new()
overlaps.ExcludeInstances = {target}
check(#world:GetPartsInPart(source,overlaps) == 0, "exclusion")
check(not pcall(function() workspace:GetPartsInPart(source) end), "world isolation")
check(not pcall(function() world:Shapecast(source,Vector3.new(1025,0,0)) end), "bounded sweep")
world:Destroy()
return {passed=checks, backend="rust occupied-volume queries", live_game_session=false}
