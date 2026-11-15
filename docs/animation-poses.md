# animation poses

AnimationTrack now samples decoded KeyframeSequence poses and drives Motor6D and
Bone transforms in Rust. It interpolates translation and rotation, resolves joints
by the connected part names, and blends tracks per joint by priority and weight.
Unused weight blends toward the neutral transform. A high-priority track only
overrides channels it actually contains.

Sampling runs after PreAnimation and before PreSimulation. Motor targets move
connected parts after PreSimulation; Bone transformed frames include their animated
ancestors. The rig root and C0/C1 offsets stay fixed. Seeking, zero speed, reverse
playback, loops and fades all use the existing track timeline and marker events.
Stopping releases the last owned pose without repeatedly overwriting later script
edits. Renaming or rewiring a rig is picked up on the next evaluation.

One evaluation validates and commits the target batch before firing property
callbacks. Actor commands check the received sequence, target transforms, names
and topology, then solve against the root world's current position. A stale batch
is rejected and its speculative Actor poses are restored. Deferred failures appear
in `animation_pose_errors` or `joint_pose_errors`.

## try it locally

Local preview registration works through:

- `AnimationClipProvider:RegisterActiveAnimationClip(sequence)`
- `KeyframeSequenceProvider:RegisterActiveKeyframeSequence(sequence)`
- `KeyframeSequenceProvider:RegisterKeyframeSequence(sequence)`

Pass a KeyframeSequence built with Keyframe and Pose instances. The returned
`active://rove-...` or `hash://rove-...` ID snapshots the sequence inside that VM.
Set Animation.AnimationId to that ID, then use Animator:LoadAnimation and Play.
Registering identical contents reuses the cached clip; changing the sequence and
registering again creates a separate clip. These IDs do not upload anything and
are not portable to another VM or Roblox Studio.

```powershell
.\rove.exe run --snapshot tests/fixtures/world_snapshot.json --script examples/animation-poses-smoke.luau --json
```

The script performs 18 checks through the official Luau VM, including registration,
blending, frame order, actual part movement and Bone world transforms. Host-supplied
`animation_responses` can also provide the existing version-1 decoded clip format.
The Rust client does not automatically download those assets yet.

## limits

This implements body pose channels in KeyframeSequence clips. CurveAnimation,
NumberPose facial channels, AnimationRigData retargeting, AnimationConstraint
simulation and server acceptance remain unfinished. Local registration rejects
unsupported child types. External decoded metadata is retained but those extra
channels are not evaluated. Multiple Animators targeting the same joint are not a
native-parity guarantee.

Sparse channels hold their first/last samples outside their own keyed interval.
Constant holds until the next key; Linear, Elastic, Bounce, legacy Cubic and CubicV2
are available. The legacy cubic direction reversal is represented explicitly.
Exact easing curves, multi-rotation blending and rig matching have not been
differentially measured in Studio; passing local tests does not establish that
parity. Relevant contracts: [Animator](https://roblox.github.io/avatar-evolution/api/class/Animator),
[AnimationPriority](https://create.roblox.com/docs/reference/engine/enums/AnimationPriority),
[Pose](https://create.roblox.com/docs/reference/engine/classes/Pose) and
[local registration](https://create.roblox.com/docs/reference/engine/classes/KeyframeSequenceProvider).

The clip cache is charged against a 16 MiB input budget and stores numeric channels
without retaining duplicate JSON poses. Limits are 128 assets, 256 tracks, 20,000
pose samples per clip, 4,096 targets per frame, and 65,536 channel/binding evaluations
per frame. Existing mutation, task and VM budgets still apply. Repeated identical
evaluation failures are coalesced; error output is capped at 128 rows.
