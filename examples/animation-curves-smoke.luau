-- build a curve, edit it, and make sure sampling survives the real host.
local passed = 0
local function check(ok, message)
    assert(ok, message)
    passed += 1
end
local function near(a, b)
    return math.abs(a - b) < 1e-6
end
local key = FloatCurveKey.new(0, 0)
check(typeof(key) == "FloatCurveKey" and key.LeftTangent == nil, "float key")
key.RightTangent = 3
key.Interpolation = Enum.KeyInterpolationMode.Linear
check(key.RightTangent == nil, "clear unused tangent")

local curve = Instance.new("FloatCurve")
check(curve.Length == 0 and curve:GetValueAtTime(0) == nil, "empty scalar")
check(curve:SetKeys({FloatCurveKey.new(2, 2), key, FloatCurveKey.new(0, 99)}) == 2, "stable sorted keys")
key.Value = 100
check(curve:GetValueAtTime(0) == 0, "input key is copied")
local copy = curve:GetKeys()
copy[1].Value = 50
check(curve:GetKeyAtIndex(1).Value == 0, "output key is copied")
local pair = curve:GetKeyIndicesAtTime(1)
check(pair[1] == 1 and pair[2] == 2, "bracket indices")
local result = curve:InsertKey(FloatCurveKey.new(2, 4))
check(result[1] == false and result[2] == 2, "replace same time")
check(near(curve:GetValueAtTime(1), 2), "linear sample")
check(curve:RemoveKeyAtIndex(2, 3) == 1 and curve.Length == 1, "remove bounded range")
local a, b = FloatCurveKey.new(0, 0), FloatCurveKey.new(2, 2)
a.RightTangent, b.LeftTangent = 1, 1
curve:SetKeys({a, b})
check(near(curve:GetValueAtTime(.5), .5), "time scaled Hermite tangents")
a.Interpolation = Enum.KeyInterpolationMode.Constant
curve:InsertKey(a)
check(curve:GetValueAtTime(1) == 0 and curve:GetValueAtTime(2) == 2, "constant endpoint")
check(not pcall(function() curve:InsertKey(FloatCurveKey.new(0/0, 0)) end) and curve.Length == 2, "invalid edit is atomic")

local rotation = Instance.new("RotationCurve")
check(rotation:GetValueAtTime(0) == CFrame.identity, "empty rotation")
rotation:SetKeys({
    RotationCurveKey.new(0, CFrame.new(3,4,5) * CFrame.Angles(0, math.rad(170), 0), Enum.KeyInterpolationMode.Linear),
    RotationCurveKey.new(1, CFrame.Angles(0, math.rad(-170), 0)),
})
check(near(rotation:GetValueAtTime(.5).LookVector.Z, 1), "shortest rotation arc")
check(rotation:GetKeyAtIndex(1).Value.Position == Vector3.zero, "rotation ignores translation")
rotation:SetKeys({RotationCurveKey.new(0, CFrame.identity), RotationCurveKey.new(1, CFrame.Angles(0, math.pi/2, 0))})
check(rotation:GetValueAtTime(.25):FuzzyEq(CFrame.Angles(0, math.pi/2*.15625, 0), 1e-6), "cubic rotation")

local vector = Instance.new("Vector3Curve")
vector:GetValueAtTime(0)
check(#vector:GetChildren() == 0, "sampling does not create axes")
local x = vector:X()
check(x == vector:X() and x.Name == "X" and x.Parent == vector, "axis identity")
x:InsertKey(FloatCurveKey.new(0, 7))
vector:Z():InsertKey(FloatCurveKey.new(0, 9))
local v = vector:GetValueAtTime(1)
check(v[1] == 7 and v[2] == nil and v[3] == 9, "missing axis stays nil")
local cloned = vector:Clone()
cloned:X():InsertKey(FloatCurveKey.new(0, 12))
check(vector:X():GetValueAtTime(0) == 7 and cloned:X():GetValueAtTime(0) == 12, "clone independence")

local euler = Instance.new("EulerRotationCurve")
check(euler.RotationOrder == Enum.RotationOrder.XYZ and euler:GetRotationAtTime(0) == CFrame.identity, "Euler defaults")
euler:X():InsertKey(FloatCurveKey.new(0, .2))
euler:Y():InsertKey(FloatCurveKey.new(0, .4))
euler:Z():InsertKey(FloatCurveKey.new(0, .7))
for _, order in Enum.RotationOrder:GetEnumItems() do
    euler.RotationOrder = order
    assert(euler:GetRotationAtTime(0):FuzzyEq(CFrame.fromEulerAngles(.2, .4, .7, order), 1e-6))
end
check(true, "all six rotation orders")
local angles = euler:GetAnglesAtTime(0)
check(angles[1] == .2 and angles[2] == .4 and angles[3] == .7, "Euler angle samples")

local changes = 0
local connection = curve:GetPropertyChangedSignal("Length"):Connect(function()
    changes += 1
    assert(curve.Length == 3)
end)
curve:InsertKey(FloatCurveKey.new(3, 3))
task.wait()
check(changes == 1, "length event sees committed keys")
connection:Disconnect()
for _, instance in {curve, rotation, vector, cloned, euler} do instance:Destroy() end
check(not pcall(function() curve:GetKeys() end), "destroyed curve")
return {feature = "editable animation curves", passed = passed, failed = 0}
