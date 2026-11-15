-- offsets should move the rig, not just change a number on the joint
local passed=0
local function check(ok,name) assert(ok,name);passed+=1 end
local function frame(a,b)
    local aa,bb={a:GetComponents()},{b:GetComponents()}
    for i=1,12 do if math.abs(aa[i]-bb[i])>.001 then return false end end
    return true
end
local m=Instance.new("Model",workspace);m.Name="JointRig"
local function part(name,x)
    local p=Instance.new("Part",m);p.Name=name;p.CFrame=CFrame.new(x,0,0);return p
end
local a,b,c=part("A",0),part("B",4),part("C",8);a.RootPriority=100
local ab=Instance.new("Motor6D",a);ab.Name="AB";ab.Part0=a;ab.Part1=b;ab.C0=CFrame.new(4,0,0)
local bc=Instance.new("Motor6D",b);bc.Name="BC";bc.Part0=b;bc.Part1=c;bc.C0=CFrame.new(4,0,0)
ab.C0=CFrame.new(6,2,0)
check(frame(a.CFrame,CFrame.identity),"root stays fixed")
check(b.Position==Vector3.new(6,2,0),"C0 moves child immediately")
check(c.Position==Vector3.new(10,2,0),"branch follows child")
ab.C1=CFrame.new(1,0,0)
check(b.Position==Vector3.new(5,2,0),"C1 inverse offset")
ab.Transform=CFrame.new(0,3,0)
check(b.Position==Vector3.new(5,2,0),"motor pose is deferred")
check(ab.Transform.Y==3,"motor target is readable immediately")
ab.C0=CFrame.new(7,2,0)
check(b.Position==Vector3.new(6,2,0),"offset uses last applied transform")
task.wait()
check(b.Position==Vector3.new(6,5,0) and c.Position==Vector3.new(10,5,0),"motor batch moves whole branch")
ab.Transform=CFrame.new(0,7,0);ab.Transform=CFrame.new(0,4,0);task.wait()
check(b.Position==Vector3.new(6,6,0),"last target wins without drift")
local root,child=a.CFrame,c.CFrame
b.Position+=Vector3.new(1,0,0)
check(frame(a.CFrame,root) and frame(c.CFrame,child),"Position only moves edited part")
check(frame(a.CFrame*ab.C0*ab.Transform,b.CFrame*ab.C1),"Position rebases joint offset")
b.Orientation=Vector3.new(10,20,30)
check(frame(a.CFrame,root) and frame(c.CFrame,child),"Orientation preserves neighbors")
ab.Enabled=false;local before=b.CFrame;ab.C0=CFrame.new(123,0,0)
check(frame(b.CFrame,before),"inactive joint does not move parts")
ab.Enabled=true;task.wait()
check(frame(b.CFrame,a.CFrame*ab.C0*ab.Transform*ab.C1:Inverse()),"reenabling settles rig")
local service=game:GetService("RunService")
local observed=false
service.PreSimulation:Once(function()ab.Transform=CFrame.new(0,5,0)end)
service.Heartbeat:Once(function()
    observed=frame(b.CFrame,a.CFrame*ab.C0*ab.Transform*ab.C1:Inverse())
end)
task.wait()
check(observed,"PreSimulation target applied before Heartbeat")
local copy=m:Clone();copy.Name="JointCopy";copy.Parent=workspace;task.wait()
check(copy.A.AB.Part1==copy.B and frame(copy.B.CFrame,b.CFrame),"cloned rig retains pose and remaps joints")
before=b.CFrame
local ok=pcall(function()ab.C0=CFrame.new(0,0,0,2,0,0,0,1,0,0,0,1)end)
check(not ok and frame(b.CFrame,before),"invalid rotation leaves pose untouched")
local accessory=part("Accessory",200);accessory.Massless=true
local weld=Instance.new("WeldConstraint",c);weld.Part0=c;weld.Part1=accessory
local relative=c.CFrame:ToObjectSpace(accessory.CFrame)
ab.C0=CFrame.new(6,0,0)*CFrame.Angles(.2,.3,.4)
check(frame(accessory.CFrame,c.CFrame*relative),"WeldConstraint branch preserves offset")
return {passed=passed,total=18,suite="joint offsets and deferred motor poses"}
