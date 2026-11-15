-- tiny rig, real tracks. no asset download needed.
local passed = 0
local function check(value, label)
    assert(value, label)
    passed += 1
end
local function near(a, b) return math.abs(a-b) < 0.001 end
local rig = Instance.new("Model", workspace)
rig.Name = "AnimationSmoke"
local root = Instance.new("Part", rig)
root.Name = "Root"
root.RootPriority = 100
local arm = Instance.new("Part", rig)
arm.Name = "Arm"
arm.CFrame = CFrame.new(4,0,0)
local joint = Instance.new("Motor6D", root)
joint.Name = "this name is ignored"
joint.Part0, joint.Part1, joint.C0 = root, arm, CFrame.new(4,0,0)
local bone = Instance.new("Bone", arm)
bone.Name = "Tip"
bone.CFrame = CFrame.new(0,2,0)
local controller = Instance.new("AnimationController", rig)
local animator = Instance.new("Animator", controller)
local sequence = Instance.new("KeyframeSequence")
sequence.Loop = true
sequence.Priority = Enum.AnimationPriority.Movement
local endPose
for _, time in {0,2} do
    local key = Instance.new("Keyframe", sequence)
    key.Time = time
    key.Name = time == 0 and "Start" or "Finish"
    local parent = Instance.new("Pose", key)
    parent.Name = "Root"
    local pose = Instance.new("Pose", parent)
    pose.Name = "Arm"
    pose.CFrame = CFrame.new(time*2,0,0)
    pose.EasingStyle = Enum.PoseEasingStyle.Linear
    pose.EasingDirection = Enum.PoseEasingDirection.In
    local tip = Instance.new("Pose", pose)
    tip.Name = "Tip"
    tip.CFrame = CFrame.new(time,0,0)
    if time == 2 then endPose = pose end
end
local provider = game:GetService("AnimationClipProvider")
local reference = provider:RegisterActiveAnimationClip(sequence)
check(string.sub(reference,1,9) == "active://", "local preview ID")
check(reference == provider:RegisterActiveAnimationClip(sequence), "same sequence reuses clip")
local animation = Instance.new("Animation")
animation.AnimationId = reference
local track = animator:LoadAnimation(animation)
check(track.Length == 2 and track.Looped, "clip metadata")
check(track.Priority == Enum.AnimationPriority.Movement, "clip priority")
check(track:GetTimeOfKeyframe("Finish") == 2, "named keyframe")
endPose.CFrame = CFrame.new(20,0,0)
local changed = provider:RegisterActiveAnimationClip(sequence)
check(changed ~= reference, "registration snapshots its input")
track:Play(0,1,0)
track.TimePosition = 1
local phases = {}
local run = game:GetService("RunService")
run.PreAnimation:Once(function() table.insert(phases,"animation") end)
run.PreSimulation:Once(function()
    table.insert(phases,"pre")
    check(near(joint.Transform.X,2), "sample before simulation")
    check(near(arm.Position.X,4), "part movement is deferred")
end)
run.PostSimulation:Once(function()
    table.insert(phases,"post")
    check(near(arm.Position.X,6), "motor moves child")
end)
task.wait()
check(table.concat(phases,",") == "animation,pre,post", "frame order")
check(near(root.Position.X,0) and near(joint.C0.X,4), "root and joint offsets stay fixed")
check(near(bone.Transform.X,1) and near(bone.TransformedWorldCFrame.X,7), "bone hierarchy follows animation")
track:AdjustWeight(0.25,0)
task.wait()
check(near(joint.Transform.X,0.5), "neutral remainder")
local overlay = animator:LoadAnimation(animation)
overlay.Priority = Enum.AnimationPriority.Action
overlay:Play(0,1,0)
overlay.TimePosition = 0.5
task.wait()
check(near(joint.Transform.X,1), "higher priority overrides per joint")
overlay:Stop(0)
task.wait()
check(near(joint.Transform.X,0.5), "lower priority resumes")
track:Stop(0)
task.wait()
check(near(joint.Transform.X,0) and near(bone.Transform.X,0), "stop releases pose")
joint.Transform = CFrame.new(0,3,0)
task.wait()
check(near(joint.Transform.Y,3), "stopped animator leaves later script edits alone")
check(#animator:GetPlayingAnimationTracks() == 0, "tracks stopped")
rig:Destroy()
sequence:Destroy()
animation:Destroy()
return {passed=passed, suite="animation poses"}
