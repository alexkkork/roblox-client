# joint poses

`C0` and `C1` now move the connected branch in Rust while its assembly root stays
fixed. Unchanged branches keep their observed relative poses, including
WeldConstraints. Re-enabling, connecting, reparenting or replacing a joint settles
the newly active branch at the next simulation phase.

`Motor6D.Transform` is readable as soon as it is assigned. Its pose is applied as
one batch after `PreSimulation` and before `PostSimulation`/`Heartbeat`. Repeated
assignments use the last target. An immediate offset edit uses the last applied
transform while a newer target is pending. The root VM and Actors share the same
frame order and bounded scheduler. Ordinary `task.wait` sleepers resume after
the batch; callbacks that yield or exhaust their CPU slice resume later.

Position, Orientation and Rotation edits move only the selected part and rebase
its active joint offsets. CFrame assignment still moves the whole rigid assembly.

Received joint edits solve together before callbacks. Explicit part CFrames in
that receipt take precedence, and the initial snapshot stays authoritative.
Derived part properties report `derived_received`; they do not produce authored
mutation rows. Missing required frames stay unavailable. A bad later joint
rejects the complete pose batch, keeping all parts at their previous poses.
`joint_pose_errors` reports deferred failures. A failed motor target remains
readable and can be assigned again after its missing data or budget is repaired.

Actor offset edits check their input poses, topology, root-selection inputs and
received sequence before replay. Motor checkpoints solve against current root
poses, so an earlier physics move is preserved. Rejected commands discard the
Actor's speculative poses. Other Actor VMs receive resulting property events.

## limits

This adds local kinematics for Weld, Snap, ManualWeld, Motor and Motor6D. A legacy
Motor with an unknown or nonzero CurrentAngle remains unsupported. KeyframeSequence
sampling is covered in [animation-poses.md](animation-poses.md). AnimationConstraint
rigs, dynamic constraint forces and server movement acceptance remain unfinished.
The graph's deterministic cycle and root-tie rules
have not been compared with native Studio.

A batch is limited to 4,096 joints and 4,096 connected parts. Existing mutation,
VM memory and task budgets still apply. Deferred error history is capped at 128
entries per output batch. The implementation uses portable Rust and the official
Luau VM; Windows CI runs the same regression suite and packaged smoke script.

```powershell
.\rove.exe run --snapshot tests/fixtures/world_snapshot.json --script examples/joint-poses-smoke.luau --json
```

The smoke script returns 18 checks. Rust tests also cover received batches,
partial metadata, stale Actor commands, callback phases and atomic budget failure.
Timing and root behavior follow the documented contracts for
[JointInstance](https://create.roblox.com/docs/reference/engine/classes/JointInstance)
and [Motor6D](https://create.roblox.com/docs/reference/engine/classes/Motor6D).
