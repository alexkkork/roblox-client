local cases={
{"LineForce","Magnitude","InverseSquareLaw",true,4.25},
{"AlignPosition","Position","Mode",Enum.PositionAlignmentMode.OneAttachment,Vector3.new(4,6,8)},
{"LinearVelocity","PlaneVelocity","VelocityConstraintMode",Enum.VelocityConstraintMode.Plane,Vector2.new(4,6)},
{"LinearVelocity","MaxPlanarAxesForce","VelocityConstraintMode",Enum.VelocityConstraintMode.Plane,Vector2.new(4,6)},
{"BodyForce","Force","Parent",nil,Vector3.new(math.huge,4,6)},
}
local report={version=version(),cases={}}
for _,c in cases do
 local m=Instance.new("Model");local p=Instance.new("Part",m);p.Anchored=true;p.CFrame=CFrame.new(3,5,7);m.PrimaryPart=p
 local obj=Instance.new(c[1],p)
 if c[3]~="Parent" then obj[c[3]]=c[4] end
 local function enc(v)
  if typeof(v)=="Vector3" then return {v.X==math.huge and "inf" or v.X,v.Y,v.Z}
  elseif typeof(v)=="Vector2" then return {v.X,v.Y} else return v end
 end
 local ok,err=pcall(function()obj[c[2]]=c[5]end)
 local r={class=c[1],property=c[2],mode=tostring(c[4]),seed_ok=ok}
 if ok then r.before=enc(obj[c[2]]);local s,e=pcall(function()m:ScaleTo(2)end);r.scale_ok=s;if s then r.after=enc(obj[c[2]]) else r.error=tostring(e)end else r.error=tostring(err) end
 table.insert(report.cases,r);m:Destroy()
end
return game:GetService("HttpService"):JSONEncode(report)
