-- moving a whole rig shouldn't mean a pile of half-finished edits
local passed=0
local function check(ok,name) assert(ok,name);passed+=1 end
local function frame(a,b)
    local aa,bb={a:GetComponents()},{b:GetComponents()}
    for i=1,12 do if math.abs(aa[i]-bb[i])>.0001 then return false end end
    return true
end
local function part(parent,name,x)
    local p=Instance.new("Part",parent);p.Name=name;p.CFrame=CFrame.new(x,0,0);return p
end
local m=Instance.new("Model",workspace);m.Name="BulkRig"
local a,b,c=part(m,"A",0),part(m,"B",4),part(m,"C",8)
local ab=Instance.new("Weld",a);ab.Name="Joint";ab.Part0=a;ab.Part1=b
local bc=Instance.new("Motor6D",b);bc.Part0=b;bc.Part1=c
local at=Instance.new("Attachment",c);at.CFrame=CFrame.new(1,2,3)
check(select('#',workspace:BulkMoveTo({},{}))==0,"empty batch")
check(#a:GetConnectedParts()==1 and a:GetConnectedParts()[1]==b,"direct connection")
check(#a:GetConnectedParts(true)==2,"recursive connections")
check(#b:GetJoints()==2 and ab.Active and bc.Active,"joint registration")
b.RootPriority=10
check(a.AssemblyRootPart==b and c:GetRootPart()==b,"assembly root")
check(math.abs(a.AssemblyMass-(a:GetMass()+b:GetMass()+c:GetMass()))<.0001,"shared mass graph")
local cfChanges,posChanges,worldChanges=0,0,0
a:GetPropertyChangedSignal("CFrame"):Connect(function()cfChanges+=1 end)
a:GetPropertyChangedSignal("Position"):Connect(function()posChanges+=1 end)
at:GetPropertyChangedSignal("WorldPosition"):Connect(function()worldChanges+=1 end)
local target=CFrame.new(10,20,30)*CFrame.Angles(.2,.4,.6)
workspace:BulkMoveTo({a},{target},Enum.BulkMoveMode.FireCFrameChanged)
check(frame(a.CFrame,target),"requested target")
check(frame(b.CFrame,target*CFrame.new(4,0,0)) and frame(c.CFrame,target*CFrame.new(8,0,0)),"rigid offsets")
check(frame(at.WorldCFrame,c.CFrame*at.CFrame),"attachment follows rig")
task.wait()
check(cfChanges==1 and posChanges==0 and worldChanges==1,"cframe-only events")
workspace:BulkMoveTo({a},{target});task.wait()
check(cfChanges==1 and worldChanges==1,"no-op stays quiet")
workspace:BulkMoveTo({a},{CFrame.new(30,0,0)});task.wait()
check(cfChanges==2 and posChanges==1,"all-events mode")
local previous=a.CFrame
local ok=pcall(function()workspace:BulkMoveTo({a,Instance.new("Folder")},{CFrame.new(100,0,0),CFrame.new()})end)
check(not ok and frame(a.CFrame,previous),"invalid batch has no partial move")
b.CFrame=CFrame.new(50,0,0)
check(a.Position.X==46 and c.Position.X==54,"individual CFrame moves rig")
ab.Enabled=false;a.CFrame=CFrame.new(5,0,0)
check(b.Position.X==50 and not ab.Active,"disabled joint splits rig")
local aa=Instance.new("Attachment",a)
local spring=Instance.new("SpringConstraint",m);spring.Attachment0=aa;spring.Attachment1=at
check(spring.Active and #a:GetConnectedParts(true)==0 and table.find(a:GetJoints(),spring)~=nil,"flexible constraint lookup")
ab.Enabled=true
local clone=m:Clone();clone.Name="Copy";clone.Parent=workspace
check(clone.A.Joint.Part0==clone.A and clone.A.Joint.Part1==clone.B,"clone endpoints")
local delta=CFrame.new(100,0,0)*clone.A.CFrame:Inverse()
local wanted=delta*clone.C.CFrame
workspace:BulkMoveTo({clone.A},{CFrame.new(100,0,0)})
check(frame(clone.C.CFrame,wanted) and a.Position.X==5,"clone moves independently")
local wm=Instance.new("WorldModel");local wp=part(wm,"P",0)
wm:BulkMoveTo({wp},{CFrame.new(1,2,3)})
check(wp.Position==Vector3.new(1,2,3) and wp.AssemblyRootPart==wp,"WorldModel batch")
local force=Instance.new("VectorForce",a);force.Attachment0=aa
check(force.RelativeTo==Enum.ActuatorRelativeTo.Attachment0 and force.Active,"single-attachment actuator")
return {passed=passed,total=20,suite="bulk move and joint graph"}
