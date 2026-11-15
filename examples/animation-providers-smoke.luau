-- fetch, edit, and preview the included animation without needing an animator.
local passed = 0
local function check(value, label)
    assert(value, label)
    passed += 1
end
local provider = game:GetService("AnimationClipProvider")
local legacy = game:GetService("KeyframeSequenceProvider")
local asset = "rbxasset://animations/two-keyframes.rbxmx"
local sequence = provider:GetAnimationClipAsync(asset)
check(sequence:IsA("KeyframeSequence") and sequence.Parent == nil, "editable detached result")
check(sequence.Loop and sequence.Priority == Enum.AnimationPriority.Movement, "clip metadata")
local frames = sequence:GetKeyframes()
check(#frames == 2 and sequence.Length == 2, "keyframe list and duration")
check(frames[1].Name == "Start" and frames[2].Name == "Finish", "keyframe names")
check(frames[2]:GetPoses()[1].Name == "Root", "root pose")
local arm = frames[2].Root:GetSubPoses()[1]
check(arm.Name == "Arm" and arm.CFrame.X == 4, "joint pose")
check(arm:GetSubPoses()[1].CFrame.X == 2, "nested pose")
local marker = frames[2]:GetMarkers()[1]
check(marker.Name == "Step" and marker.Value == "right", "marker contents")
frames[2]:RemoveMarker(marker)
check(marker.Parent == nil and #frames[2]:GetMarkers() == 0, "remove marker")
frames[2]:AddMarker(marker)
check(marker.Parent == frames[2], "add marker")
local copy = legacy:GetKeyframeSequenceAsync(asset)
check(copy ~= sequence and copy:GetKeyframes()[2] ~= frames[2], "independent instances")
sequence.Name, sequence.AuthoredHipHeight = "Edited", 2.5
arm.CFrame, arm.Weight = CFrame.new(7,0,0), 0.5
arm.EasingStyle = Enum.PoseEasingStyle.Cubic
frames[2].Time = 3
check(sequence.Length == 3 and copy.Length == 2, "edits do not change the cache")
local reference = provider:RegisterAnimationClip(sequence)
check(reference:sub(1,7) == "hash://", "register clip")
local edited = provider:GetAnimationClipAsync(reference)
check(edited.Name == "Edited" and edited.AuthoredHipHeight == 2.5, "authoring metadata round trip")
check(edited:GetKeyframes()[2].Root.Arm.CFrame.X == 7, "pose edit round trip")
check(edited:GetKeyframes()[2].Root.Arm.Weight == 0.5, "weight round trip")
check(edited:GetKeyframes()[2].Root.Arm.EasingStyle == Enum.PoseEasingStyle.Cubic, "easing round trip")
check(edited.Length == 3, "time edit round trip")
local folder = Instance.new("Folder")
edited:AddKeyframe(folder)
check(folder.Parent == edited and #edited:GetKeyframes() == 2, "child filtering")
edited:RemoveKeyframe(folder)
check(folder.Parent == nil, "remove does not destroy")
local ok, reason = pcall(function() return provider:GetAnimationClipAsync("rbxasset://missing-animation.rbxm") end)
check(not ok and tostring(reason):find("animation_local_asset_unavailable"), "asset errors reach pcall")
sequence:Destroy(); copy:Destroy(); edited:Destroy(); folder:Destroy()
return { passed=passed, failed=0, feature="animation clip providers" }
