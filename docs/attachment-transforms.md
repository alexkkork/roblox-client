# attachment and bone transforms

The Rust host computes Attachment coordinates from the current world, including
ancestor attachments, bones, parts, models and cameras. Folders do not interrupt
the transform chain. A part uses its CFrame, independently of PivotOffset.

Implemented bindings:

- CFrame, Position, Orientation, Rotation, Axis and SecondaryAxis.
- WorldCFrame, WorldPosition, WorldOrientation, WorldRotation, WorldAxis and
  WorldSecondaryAxis. WorldRotation is read-only.
- GetAxis, SetAxis, GetSecondaryAxis, SetSecondaryAxis and GetConstraints.
- Bone.Transform, TransformedCFrame and TransformedWorldCFrame. The transformed
  properties are read-only; reference frames remain separate from bone poses.

Axis writes normalize and reproject the perpendicular axis. Position writes
preserve orientation. World writes convert through the current parent frame.
Each write validates the whole pose and mutation budget before committing it.
Actor world writes also check their ancestor pose when replayed on the root VM,
including changes made by physics feedback.

Property signals follow local writes, received updates, ancestor movement,
reparenting, model pivots, scaling and Bone.Transform changes. Only observed
properties retain notification baselines; the per-VM limit is 4,096 watched
properties. A Changed listener observes all transform properties of its receiver.
Disconnected and destroyed observers are removed on refresh. Ordinary scheduler
and connection limits also apply.

Received component updates retain a canonical received pose on affected nodes;
this is necessary because successive axis projections depend on earlier poses.
Clones materialize the current local pose without retaining receipt history.
Partial snapshots keep unknown rotations unknown. World operations require the
ancestor data they use; unavailable poses or constraint references produce errors.

This adds transform bindings and constraint lookup. It does not add an Animator,
mesh skinning, a constraint solver or proof of live server replication.

## verification

```sh
cargo test --locked --manifest-path rust-client/Cargo.toml --test attachment_transforms
cargo test --locked --manifest-path rust-client/Cargo.toml attachment_tests
rove run --snapshot tests/fixtures/world_snapshot.json --script examples/attachment-transforms-smoke.luau --json
```

Windows CI runs the same Rust tests and the 18-check script inside the release
package. The tests use analytic transforms and real Luau execution; they do not
claim a live Roblox or visual rendering test.

References: [Attachment](https://create.roblox.com/docs/reference/engine/classes/Attachment)
and [Bone](https://create.roblox.com/docs/reference/engine/classes/Bone).
