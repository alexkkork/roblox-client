# model scaling

`Model:ScaleTo(factor)` and `Model:GetScale()` run in the Rust host with the
official Luau VM. The factor is absolute: `ScaleTo(2)` twice leaves the model at
twice its original size. `Model.Scale` remains an editor-only property.

```luau
local model = Instance.new("Model", workspace)
local part = Instance.new("Part", model)
part.Anchored = true
part.Size = Vector3.new(2, 3, 4)
model:ScaleTo(2)
assert(part.Size == Vector3.new(4, 6, 8))
model:ScaleTo(1)
```

The Rust implementation handles:

- part positions and sizes around automatic, explicit, or primary-part pivots;
- pivot offsets, attachment/bone local frames, joint offsets and Motor6D transforms;
- nested model factors and explicit pivots;
- file-mesh dimensions and primitive-mesh offsets;
- sound distances, particle sizes/speeds/acceleration, beam and trail dimensions;
- Humanoid dimensions, movement speeds and jump properties;
- measured length, force, torque, stiffness and damping rules for supported
  BodyMovers and constraints, including finite and infinite force limits.

Each changed field retains its original value and the factor at which it was
recorded. Scaling back restores that value exactly. Independently edited or
new descendants start a new baseline at the current factor. Cloning remaps the
baselines into the clone; changing the clone does not alter the original.

Geometry, canonical factors, cache state, and mutation rows commit before
callbacks. Invalid numbers, overflow, unavailable required geometry, and exhausted
budgets leave them unchanged. Spatial queries and mass calculations read the
resized local geometry. This does not establish server-authoritative scaling or
add constraint simulation.

Actor VMs send a bounded scaling command with a descendant/revision read set.
Root replays the planner in order and rejects stale geometry, changed membership,
or received-world revisions. Actor clones transfer their baseline at creation,
rather than borrowing a later state. Part CFrame edits also use atomic pose
batches so their derived Position/Orientation/Rotation writes do not conflict
with their own replay.

Baseline accounting is capped at 8 MiB per world, including serialized entries
and an allowance for containers. Actor journals retain their aggregate 1 MiB /
4096-row limits. Immutable cache entries are shared across VM forks. These are
data-structure budgets, not a process RSS measurement.

## limits and evidence

Animator descendants are rejected until skeletal pose scaling is integrated.
Terrain and unsupported mesh/constraint subclasses are also explicit errors.
Received models need known canonical scale and required geometry; unavailable
snapshot fields are not replaced with invented defaults. This change scales
attachment local frames; it does not add the missing independent attachment
world-transform API.

The tests replay 453 property observations from the saved Studio
`0.745.0.7451137` physics capture, plus regressions for exact restoration, clones,
edits, bounds, mass, callbacks, rollback, Actor replay and ownership conflicts.
The capture and its probes are in `rust-client/tests/fixtures/model-scaling/`.
Those are historical reference observations, not a new Studio or live-server test.

The Windows workflow builds and tests the same Rust code using MSVC, then runs
this 15-check script from the release package:

```powershell
.\rove.exe run --snapshot tests/fixtures/world_snapshot.json --script examples/model-scaling-smoke.luau --json
```

API semantics: [Roblox Model reference](https://create.roblox.com/docs/reference/engine/classes/Model#ScaleTo).
