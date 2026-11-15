-- scale the whole model, then put it back. runs offline on windows too.
local passed=0
local function check(ok,message) assert(ok,message);passed+=1 end
local function near(a,b) return math.abs(a-b)<.001 end
local m=Instance.new("Model",workspace);m.WorldPivot=CFrame.new()
local p=Instance.new("Part",m);p.Name="P";p.Anchored=true
p.CFrame=CFrame.new(.137,2.281,3.319);p.Size=Vector3.new(2,3,4)
local original,size,mass=p.CFrame,p.Size,p:GetMass()
local a=Instance.new("Attachment",p);a.CFrame=CFrame.new(1,2,3)
local joint=Instance.new("Motor6D",p);joint.C0=CFrame.new(2,3,4)
local spring=Instance.new("SpringConstraint",p);spring.FreeLength=3;spring.Stiffness=4;spring.Damping=4
local force=Instance.new("BodyForce",p);force.Force=Vector3.new(4,6,8)
local beam=Instance.new("Beam",p);beam.Width0=3
local emitter=Instance.new("ParticleEmitter",p);emitter.Speed=NumberRange.new(2,6)
local sound=Instance.new("Sound",p);sound.RollOffMaxDistance=100
check(m:GetScale()==1,"new model scale")
m:ScaleTo(2)
check(p.Size==size*2 and m:GetScale()==2,"absolute model scale")
check(p.CFrame.Position==original.Position*2,"part position")
check(near(p:GetMass(),mass*8),"scaled mass")
check(a.CFrame.Position==Vector3.new(2,4,6) and a.Position==a.CFrame.Position,"attachment offset")
check(joint.C0.Position==Vector3.new(4,6,8),"joint offset")
check(force.Force==Vector3.new(32,48,64),"cubic force scaling")
check(spring.FreeLength==6 and spring.Stiffness==16 and near(spring.Damping,4*2^2.5),"spring units")
check(beam.Width0==6 and sound.RollOffMaxDistance==200,"effect dimensions")
check(emitter.Speed.Min==4 and emitter.Speed.Max==12,"particle range")
m:ScaleTo(3.79)
local copy=m:Clone();copy:ScaleTo(1)
check(copy.P.CFrame==original and copy.P.Size==size,"clone keeps original geometry")
copy:Destroy();m:ScaleTo(1)
check(p.CFrame==original and p.Size==size,"exact scale back")
check(not pcall(function()m:ScaleTo(0)end) and p.Size==size,"invalid scale is atomic")
check(not pcall(function()return m.Scale end),"editor-only scale stays private")
local n=Instance.new("Model",m);local q=Instance.new("Part",n);q.Anchored=true
m:ScaleTo(2)
check(n:GetScale()==2,"nested canonical scale")
m:Destroy()
return {passed=passed,backend="rust model scaling",live_game_session=false}
