-- local rig math, through the same vm used by the packaged client
local passed=0
local function check(ok,name) assert(ok,name);passed+=1 end
local function near(a,b) return math.abs(a-b)<.0001 end
local function vec(a,b) return (a-b).Magnitude<.0001 end
local function frame(a,b)
    local av,bv={a:GetComponents()},{b:GetComponents()}
    for i=1,12 do if not near(av[i],bv[i]) then return false end end
    return true
end
local m=Instance.new("Model",workspace);m.WorldPivot=CFrame.new()
local p=Instance.new("Part",m);p.Name="P";p.CFrame=CFrame.new(10,0,0)
p.PivotOffset=CFrame.new(500,0,0)
local a=Instance.new("Attachment",p);a.Name="A"
check(a.CFrame==CFrame.new() and a.Axis==Vector3.xAxis,"default attachment")
a.Position=Vector3.new(1,2,3);a.Orientation=Vector3.new(20,30,40)
check(frame(a.CFrame,CFrame.new(1,2,3)*CFrame.fromOrientation(math.rad(20),math.rad(30),math.rad(40))),"orientation order")
check(vec(a.WorldPosition,Vector3.new(11,2,3)),"part frame ignores pivot offset")
a.WorldPosition=Vector3.new(14,5,6)
check(vec(a.Position,Vector3.new(4,5,6)),"world position setter")
local target=CFrame.new(20,30,40)*CFrame.Angles(.2,.4,.6)
a.WorldCFrame=target
check(frame(a.WorldCFrame,target),"world frame setter")
a.Axis=Vector3.new(1,2,3)
check(near(a.Axis.Magnitude,1),"axis normalization")
a.SecondaryAxis=Vector3.new(-3,2,1)
check(near(a.Axis:Dot(a.SecondaryAxis),0),"axis projection")
a:SetAxis(Vector3.xAxis);a:SetSecondaryAxis(Vector3.yAxis)
check(vec(a:GetAxis(),Vector3.xAxis) and vec(a:GetSecondaryAxis(),Vector3.yAxis),"axis methods")
local b=Instance.new("Bone",p);b.CFrame=CFrame.new(1,0,0);b.Transform=CFrame.new(0,2,0)
local c=Instance.new("Bone",b);c.CFrame=CFrame.new(0,0,3);c.Transform=CFrame.new(4,0,0)
check(c.WorldCFrame==CFrame.new(11,0,3),"bone reference pose")
check(c.TransformedWorldCFrame==CFrame.new(15,2,3) and c.TransformedCFrame==CFrame.new(4,0,3),"bone animated pose")
local hinge=Instance.new("HingeConstraint",m);hinge.Attachment0=a;hinge.Attachment1=a
check(#a:GetConstraints()==1 and a:GetConstraints()[1]==hinge,"constraint deduplication")
hinge.Attachment0=nil;hinge.Attachment1=nil
check(#a:GetConstraints()==0,"constraint disconnection")
local copy=p:Clone();copy.CFrame=CFrame.new(50,0,0)
check(vec(copy.A.WorldPosition,(copy.CFrame*a.CFrame).Position),"clone binds its own parent")
copy:Destroy()
local before=a.CFrame
check(not pcall(function()a.WorldRotation=Vector3.zero end) and not pcall(function()c.TransformedCFrame=CFrame.new()end),"read-only poses")
check(not pcall(function()a.Axis=Vector3.zero end) and frame(a.CFrame,before),"invalid axis rollback")
local localChanges,worldChanges=0,0
local localConnection=a:GetPropertyChangedSignal("Position"):Connect(function()localChanges+=1 end)
local worldConnection=a:GetPropertyChangedSignal("WorldPosition"):Connect(function()worldChanges+=1 end)
a.WorldPosition=Vector3.new(60,70,80);task.wait()
check(localChanges==1 and worldChanges==1,"atomic pose notifications")
p.CFrame=CFrame.new(20,0,0);task.wait()
check(localChanges==1 and worldChanges==2 and vec(a.WorldPosition,Vector3.new(70,70,80)),"parent notifications")
m:ScaleTo(2);task.wait()
check(worldChanges==3 and vec(a.WorldPosition,Vector3.new(140,140,160)),"model scaling integration")
localConnection:Disconnect();worldConnection:Disconnect();m:Destroy()
return {passed=passed,backend="rust attachment and bone transforms",live_game_session=false}
