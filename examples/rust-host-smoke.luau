-- quick local check with tests/fixtures/world_snapshot.json
local passed = {}
local function check(name, condition)
    assert(condition, name)
    table.insert(passed, name)
end

local workspace = game:GetService("Workspace")
local player = game:GetService("Players").LocalPlayer
check("received humanoid", player.Character.Humanoid.Health == 100)

local part = Instance.new("Part")
part.Name = "RustHostProbe"
part.Parent = workspace
check("part defaults", part.Shape == Enum.PartType.Block and part.Material == Enum.Material.Plastic)
part.Size = Vector3.new(2, 3, 4)
check("local mass", math.abs(part:GetMass() - 16.8) < 0.0001)
part.Material = Enum.Material.Metal
check("mass after material change", math.abs(part:GetMass() - 188.4) < 0.0001)
part:ResetPropertyToDefault("Material")
check("property reset", not part:IsPropertyModified("Material") and part.Material == Enum.Material.Plastic)
check("instance selector", #workspace:QueryDescendants("Part#RustHostProbe") == 1)

local handle = InstanceHandle.new(part)
check("instance handle", handle:Get() == part and handle:Wait(0.01) == part)
check("settings singleton", settings() == settings() and settings():IsA("GenericSettings"))
check("stats singleton", Stats() == stats() and stats() == game:GetService("Stats"))

local received
local event = Instance.new("BindableEvent")
local connection = event.Event:Connect(function(...)
    received = table.pack(...)
end)
check("engine types", typeof(part) == "Instance" and typeof(handle) == "InstanceHandle"
    and typeof(event.Event) == "RBXScriptSignal" and typeof(connection) == "RBXScriptConnection")
firesignal(event.Event, 42, nil, "rust")
task.wait()
check("signal tuple", received.n == 3 and received[1] == 42 and received[2] == nil and received[3] == "rust")

local dock = DockWidgetPluginGuiInfo.new(Enum.InitialDockState.Float, true, false, 100, 90, 40, 30)
check("typed values", dock.InitialDockState == Enum.InitialDockState.Float and typeof(CFrame.new()) == "CFrame")

part:Destroy()
event:Destroy()
return { passed = #passed, checks = passed }
