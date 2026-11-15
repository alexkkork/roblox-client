# batch movement and joint queries

`WorldRoot:BulkMoveTo` now runs in Rust. It validates both arrays, expands each
registered rigid component once, then commits the entire pose before callbacks.
Welded partners retain their current offsets. Ordinary BasePart CFrame assignment
uses the same transaction for parts in a Workspace or WorldModel.

The shared graph supplies `GetConnectedParts`, `GetJoints`, `GetRootPart`,
`AssemblyRootPart`, joint `Active`, and assembly mass calculations. It supports
Weld, Snap, ManualWeld, Motor, Motor6D and WeldConstraint rigid edges. Attachment
constraints appear in joint queries without becoming rigid edges. Registration
checks containers, endpoints, Enabled, alignment modes and actuator reference
attachments. JointsService registers JointInstances with Workspace endpoints.

`FireAllEvents` is the default. `FireCFrameChanged` suppresses Position,
Orientation and Rotation notifications while all fields still update. Attachment
world signals continue to follow the pose. An unchanged batch is silent. Received
CFrames clear stale local transform aliases and refresh derived notifications.
Joint Active and assembly-root signals track topology and priority changes; only
observed properties retain baselines (4,096 per VM).

Actor batches are replayed as one command. A fingerprint checks the received
sequence, current poses and incident graph before applying the command. A stale
batch leaves all root parts untouched and discards its stale Actor poses. Invalid
input, unknown required poses and exhausted mutation budgets also reject the
entire batch. Arrays must be dense, with at most 4,096 requests and 4,096 expanded
parts; the runtime's existing journal, response and scheduler limits still apply.

## current boundaries

This is local engine behavior, not proof of server-authoritative movement. It
does not simulate constraint forces, snap newly connected C0/C1 frames or solve
Motor6D animation. Position and angle setters still use their existing isolated
part path; joint-offset rebasing is separate work.

Conflicting targets in one rigid component use the last requested endpoint.
Duplicate/cyclic joints, anchored-island selection and equal root priorities use
stable world order. These deterministic policies preserve the older host's
behavior; they have not been measured against Studio. Detached batch targets are
accepted. The local WorldModel container has the same query behavior as Workspace.

Documented constructor defaults are supplied for VectorForce/Torque RelativeTo,
LinearVelocity RelativeTo and alignment Mode. AngularVelocity RelativeTo still
requires explicit assignment when absent. Received missing modes, endpoints and
rotations remain unavailable rather than receiving local constructor defaults.

## run it

```sh
cargo test --locked --manifest-path rust-client/Cargo.toml --test bulk_move
cargo test --locked --manifest-path rust-client/Cargo.toml --lib bulk_move_tests
rove run --snapshot tests/fixtures/world_snapshot.json --script examples/bulk-move-smoke.luau --json
```

Windows CI runs the Rust regressions and the same 20-check script in the release
package, using the official Luau VM. The tests exercise local runtime behavior;
they do not establish live Roblox acceptance or native Studio physics parity.

API references: [WorldRoot](https://create.roblox.com/docs/reference/engine/classes/WorldRoot),
[BasePart](https://create.roblox.com/docs/reference/engine/classes/BasePart),
[JointInstance](https://create.roblox.com/docs/reference/engine/classes/JointInstance),
[VectorForce](https://create.roblox.com/docs/reference/engine/classes/VectorForce),
[Torque](https://create.roblox.com/docs/reference/engine/classes/Torque),
[LinearVelocity](https://create.roblox.com/docs/reference/engine/classes/LinearVelocity),
[AlignPosition](https://create.roblox.com/docs/reference/engine/classes/AlignPosition) and
[AlignOrientation](https://create.roblox.com/docs/reference/engine/classes/AlignOrientation).
