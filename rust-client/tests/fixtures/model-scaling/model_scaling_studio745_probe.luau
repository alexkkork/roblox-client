
local fields=game:GetService("HttpService"):JSONDecode([==[{"BodyForce":["Force","force"],"BodyVelocity":["MaxForce","P","Velocity","maxForce","velocity"],"BodyPosition":["D","MaxForce","P","Position","maxForce","position"],"BodyAngularVelocity":["AngularVelocity","MaxTorque","P","angularvelocity","maxTorque"],"BodyGyro":["CFrame","D","MaxTorque","P","cframe","maxTorque"],"BodyThrust":["Force","Location","force","location"],"RocketPropulsion":["CartoonFactor","MaxSpeed","MaxThrust","MaxTorque","TargetOffset","TargetRadius","ThrustD","ThrustP","TurnD","TurnP"],"VectorForce":["Force"],"Torque":["Torque"],"LineForce":["Magnitude","MaxForce"],"LinearVelocity":["LineDirection","LineVelocity","MaxAxesForce","MaxForce","PrimaryTangentAxis","SecondaryTangentAxis","VectorVelocity"],"AngularVelocity":["AngularVelocity","MaxTorque"],"AlignPosition":["MaxAxesForce","MaxForce","MaxVelocity","Position","Responsiveness"],"AlignOrientation":["CFrame","LookAtPosition","MaxAngularVelocity","MaxTorque","PrimaryAxis","Responsiveness","SecondaryAxis"],"AnimationConstraint":["AngularDamping","AngularStrength","LinearDamping","LinearStrength","MaxForce","MaxTorque","Transform"],"HingeConstraint":["AngularResponsiveness","AngularSpeed","AngularVelocity","LowerAngle","MotorMaxAcceleration","MotorMaxTorque","Radius","Restitution","ServoMaxTorque","TargetAngle","UpperAngle"],"SpringConstraint":["Coils","Damping","FreeLength","MaxForce","MaxLength","MinLength","Radius","Stiffness","Thickness"],"RopeConstraint":["Length","Restitution","Thickness","WinchForce","WinchResponsiveness","WinchSpeed","WinchTarget"],"RodConstraint":["Length","LimitAngle0","LimitAngle1","Thickness"],"PrismaticConstraint":["LinearResponsiveness","LowerLimit","MotorMaxAcceleration","MotorMaxForce","Restitution","ServoMaxForce","Size","Speed","TargetPosition","UpperLimit","Velocity"],"CylindricalConstraint":["AngularResponsiveness","AngularRestitution","AngularSpeed","AngularVelocity","InclinationAngle","LinearResponsiveness","LowerAngle","LowerLimit","MotorMaxAcceleration","MotorMaxAngularAcceleration","MotorMaxForce","MotorMaxTorque","Restitution","ServoMaxForce","ServoMaxTorque","Size","Speed","TargetAngle","TargetPosition","UpperAngle","UpperLimit","Velocity"],"BallSocketConstraint":["MaxFrictionTorque","Radius","Restitution","TwistLowerAngle","TwistUpperAngle","UpperAngle"],"TorsionSpringConstraint":["Coils","Damping","MaxAngle","MaxTorque","Radius","Restitution","Stiffness"],"UniversalConstraint":["MaxAngle","Radius","Restitution"],"PlaneConstraint":[],"NoCollisionConstraint":[],"RigidConstraint":[],"WeldConstraint":[],"Humanoid":["CameraOffset","Health","HealthDisplayDistance","HipHeight","JumpHeight","JumpPower","MaxHealth","MaxSlopeAngle","NameDisplayDistance","TargetPoint","WalkSpeed","WalkToPoint","maxHealth"],"Part":["AssemblyAngularVelocity","AssemblyLinearVelocity","BackParamA","BackParamB","BottomParamA","BottomParamB","CFrame","CollisionGroupId","Elasticity","Friction","FrontParamA","FrontParamB","LeftParamA","LeftParamB","LocalTransparencyModifier","Orientation","PivotOffset","Position","Reflectance","RightParamA","RightParamB","RootPriority","RotVelocity","Rotation","Size","TopParamA","TopParamB","Transparency","Velocity"]}]==])
local function encode(v)
 local t=typeof(v)
 if t=="Vector3" then return {v.X,v.Y,v.Z}
 elseif t=="Vector2" then return {v.X,v.Y}
 elseif t=="CFrame" then return {v:GetComponents()}
 elseif t=="number" then
  if v==math.huge then return "inf" elseif v==-math.huge then return "-inf" elseif v~=v then return "nan" end
  return v
 else return tostring(v) end
end
local report={version=version(),cases={}}
for cls,props in fields do
 local m=Instance.new("Model")
 local root=Instance.new("Part",m);root.Anchored=true;root.CFrame=CFrame.new(3,5,7);root.Size=Vector3.new(2,3,4)
 local other=Instance.new("Part",m);other.Anchored=true;other.CFrame=CFrame.new(9,11,13)
 m.PrimaryPart=root
 local at0=Instance.new("Attachment",root);at0.CFrame=CFrame.new(1,2,3)
 local at1=Instance.new("Attachment",other);at1.CFrame=CFrame.new(-2,-3,-4)
 local ok,obj=pcall(Instance.new,cls)
 local case={class=cls,properties={},errors={}}
 if ok then
  obj.Parent=root
  pcall(function()obj.Attachment0=at0 end);pcall(function()obj.Attachment1=at1 end)
  pcall(function()obj.Part0=root;obj.Part1=other end)
  if cls=="Humanoid" then obj.Parent=m;obj.AutomaticScalingEnabled=false end
  for _,name in props do
   if string.sub(name,1,1)==string.upper(string.sub(name,1,1)) then
    local valid,value=pcall(function()return obj[name]end)
    if valid then
     local typ=typeof(value)
     local seeded=typ=="number" and 4.25 or typ=="Vector3" and Vector3.new(4,6,8)
       or typ=="Vector2" and Vector2.new(4,6) or typ=="CFrame" and CFrame.new(4,6,8)*CFrame.Angles(.2,.3,.4)
     if seeded then
      local set,err=pcall(function()obj[name]=seeded end)
      if set then case.properties[name]={type=typ} else case.errors[name]=tostring(err) end
     end
    end
   end
  end
  for name,values in case.properties do values.before=encode(obj[name]) end
  for _,scale in {2,.5,1} do
   local scaled,err=pcall(function()m:ScaleTo(scale)end)
   if not scaled then case.errors["scale_"..tostring(scale)]=tostring(err)
   else
    for name,values in case.properties do values["at_"..tostring(scale)]=encode(obj[name]) end
   end
  end
 else case.errors.create=tostring(obj) end
 m:Destroy()
 table.insert(report.cases,case)
end
return game:GetService("HttpService"):JSONEncode(report)

