-- the loader reads this clip from --content-root on a worker thread.
local passed = 0
local function check(value, label)
    assert(value, label)
    passed += 1
end
local function near(a,b) return math.abs(a-b)<0.001 end
local rig = Instance.new("Model", workspace)
rig.Name = "LoadedAnimation"
local root = Instance.new("Part", rig)
root.Name, root.RootPriority = "Root", 100
local arm = Instance.new("Part", rig)
arm.Name, arm.CFrame = "Arm", CFrame.new(4,0,0)
local joint = Instance.new("Motor6D", root)
joint.Part0, joint.Part1, joint.C0 = root, arm, CFrame.new(4,0,0)
local bone = Instance.new("Bone", arm)
bone.Name, bone.CFrame = "Tip", CFrame.new(0,2,0)
local controller = Instance.new("AnimationController", rig)
local animator = Instance.new("Animator", controller)
local animation = Instance.new("Animation")
animation.AnimationId = "rbxasset://animations/two-keyframes.rbxmx"
local track = animator:LoadAnimation(animation)
check(track.Length == 0, "load starts asynchronously")
track:Play(0,1,0)
local deadline = os.clock()+10
while track.Length == 0 and os.clock()<deadline do task.wait() end
check(track.Length == 2, "asset loaded through the Rust decoder")
check(track.Looped and track.Priority == Enum.AnimationPriority.Movement, "asset metadata")
check(track:GetTimeOfKeyframe("Finish") == 2, "named keyframes")
track.TimePosition = 1
task.wait()
check(near(joint.Transform.X,2), "loaded motor pose")
check(near(arm.Position.X,6), "loaded pose moves the part")
check(near(bone.Transform.X,1), "loaded bone pose")
check(near(root.Position.X,0), "rig root remains fixed")
local reused = animator:LoadAnimation(animation)
check(reused.Length == 2, "second track uses the loaded clip")
track:Stop(0)
task.wait()
check(near(joint.Transform.X,0) and near(bone.Transform.X,0), "stop releases loaded poses")
rig:Destroy()
return { passed=passed, failed=0, feature="animation asset loading" }
