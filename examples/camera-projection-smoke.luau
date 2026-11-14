-- same camera math on windows and mac. no renderer needed for this check.
local c=Instance.new("Camera",workspace)
c.CFrame=CFrame.new();c.FieldOfView=90;workspace.CurrentCamera=c
local passed=0
local function check(ok,message) assert(ok,message);passed+=1 end
local function near(a,b) return math.abs(a-b)<.0002 end
local function vnear(a,b) return (a-b).Magnitude<.0002 end
check(c.ViewportSize==Vector2.new(800,600),"host viewport")
local p,on=c:WorldToViewportPoint(Vector3.new(0,0,-10))
check(on and vnear(p,Vector3.new(400,300,10)),"center and depth")
p,on=c:WorldToViewportPoint(Vector3.new(0,0,10))
check(not on and p.Z==-10,"behind camera")
p,on=c:WorldToViewportPoint(Vector3.new(20,0,-10))
check(not on and p.X>800,"outside viewport")
p,on=c:WorldToScreenPoint(Vector3.new(0,0,-10))
check(on and vnear(p,Vector3.new(400,264,10)),"core inset")
local viewportRay=c:ViewportPointToRay(400,300)
local screenRay=c:ScreenPointToRay(400,264)
check(vnear(viewportRay.Origin,screenRay.Origin) and vnear(viewportRay.Direction,screenRay.Direction),"screen ray agreement")
check(vnear(viewportRay.Origin,Vector3.new(0,0,-.5)),"near plane")
local edge=c:ViewportPointToRay(700,300)
check(near(edge.Direction.Magnitude,1) and vnear(edge.Origin,Vector3.new(.5,0,-.5)),"off-axis unit ray")
local deep=c:ViewportPointToRay(700,300,2)
check(vnear(deep.Origin,edge.Origin+edge.Direction*2),"ray depth")
c.CoordinateFrame=CFrame.new(3,4,5)*CFrame.Angles(.2,.3,.4)
check(c.CFrame==c.CoordinateFrame,"transform aliases")
local ray=c:ViewportPointToRay(400,300)
check(vnear(ray.Direction,c.CFrame.LookVector),"rotated ray")
local q=c:WorldToViewportPoint(ray.Origin+ray.Direction*25)
check(near(q.X,400) and near(q.Y,300),"projection round trip")
c.DiagonalFieldOfView=90
check(near(math.tan(math.rad(c.FieldOfView/2)),.6),"diagonal lens")
c.FieldOfViewMode=Enum.FieldOfViewMode.MaxAxis
check(c.FieldOfViewMode==Enum.FieldOfViewMode.MaxAxis,"lens mode")
local clone=c:Clone()
check(clone.ViewportSize==Vector2.one,"inactive clone viewport")
check(not pcall(function() c.ViewportSize=Vector2.zero end) and not pcall(function() c:ViewportPointToRay(math.huge,0) end),"invalid writes and inputs")
clone:Destroy();c:Destroy()
return {passed=passed,backend="rust camera projection",live_game_session=false}
