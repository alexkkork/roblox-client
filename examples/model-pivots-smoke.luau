-- local model transforms and bounds, using the same rust host on every os.
local m=Instance.new("Model",workspace)
local a=Instance.new("Part",m);a.Anchored=true;a.Size=Vector3.new(2,2,2);a.CFrame=CFrame.new(-2,0,0)
local b=Instance.new("Part",m);b.Anchored=true;b.Size=a.Size;b.CFrame=CFrame.new(2,0,0)
local passed=0
local function check(ok,message) assert(ok,message);passed+=1 end
local function near(a,b) return math.abs(a-b)<.0001 end
local cf,size=m:GetBoundingBox()
check(cf.Position==Vector3.zero and size==Vector3.new(6,2,2),"model bounds")
check(m:GetExtentsSize()==size,"extents size")
check(m:GetPivot().Position==Vector3.zero,"automatic pivot")
m:PivotTo(CFrame.new(10,20,30))
check(a.Position==Vector3.new(8,20,30) and b.Position==Vector3.new(12,20,30),"whole model move")
check(m:GetPivot().Position==Vector3.new(10,20,30),"saved pivot")
b.CFrame=CFrame.new(50,0,0)
check(m:GetPivot().Position==Vector3.new(10,20,30),"stable world pivot")
a.PivotOffset=CFrame.new(1,2,3);m.PrimaryPart=a
check(a:GetPivot()==a.CFrame*a.PivotOffset and m:GetPivot()==a:GetPivot(),"primary offset")
m:PivotTo(CFrame.new(5,6,7))
check(near(a.Position.X,4) and near(a.Position.Y,4) and near(a.Position.Z,4),"offset-aware movement")
check(m:GetPrimaryPartCFrame()==a.CFrame,"legacy frame query")
m.PrimaryPart=nil;m.WorldPivot=CFrame.new()
local clone=m:Clone();clone.Name="PivotClone"
check(clone:GetPivot()==CFrame.new(),"explicit identity clone")
clone:Destroy()
local empty=Instance.new("Model",m)
empty:PivotTo(CFrame.new(1,2,3))
check(empty:GetExtentsSize()==Vector3.zero and empty:GetPivot().Position==Vector3.new(1,2,3),"empty model")
check(not pcall(function() m:PivotTo(CFrame.new(0/0,0,0)) end),"finite transforms")
m:Destroy()
return {passed=passed,backend="rust model pivots",live_game_session=false}
