# model pivots and bounds

The Rust host implements `GetPivot`, `GetBoundingBox`, `GetExtentsSize` and
`GetPrimaryPartCFrame`. `PivotTo` now honors `PivotOffset` and model pivots,
including nested models, parts parented beneath parts, and cameras.

```lua
local pivot = model:GetPivot()
local box, size = model:GetBoundingBox()
model:PivotTo(pivot * CFrame.new(0, 5, 0))
```

A part's pivot combines its CFrame and PivotOffset. A model with a PrimaryPart
uses that part's pivot. A new model without a primary part initially follows
the center of its contents; setting WorldPivot or using PivotTo ends that
automatic behavior. Empty models have zero bounds and can still be moved.
Bounding boxes use the pivot's orientation and all descendant part extents.

WorldPivot edits change the pivot without moving geometry. Clones preserve
whether the source pivot was automatic or explicitly set, including an explicit
identity. Legacy TranslateBy preserves untouched automatic pivots and moves
explicit pivots along with their parts. SetPrimaryPartCFrame continues to target
the primary part's CFrame rather than its offset pivot.

PivotTo validates all targets and calculated transforms before committing a
batch. Changed callbacks then see all moved parts; Position, Orientation and
Rotation notifications are suppressed for this operation while CFrame
notifications remain. Actor moves use one journal batch with per-node revision
checks. If a target changed in the root world, the entire stale Actor batch is
rejected rather than partially applied.

Repeated model moves reuse numeric relative transforms. Cache entries are
invalidated by actual pose or hierarchy changes, and received creates, property
updates and removals are visible to the next query. Automatic nested pivots fold
their bounds from children to parents in one pass.

## bounds on work and storage

Traversal accepts at most 100,000 nodes. Numeric pivot caches retain up to
64 models and 4 MiB per VM, with least-recently-used eviction; a single model
larger than that cache budget returns an error before moving. Evicted entries
are rebuilt from current transforms when next used. Ordinary host VM, task,
response and mutation limits still apply.

A root transform batch accepts up to 8 MiB of encoded mutations and at most
16,384 pending journal rows. Actor journals retain their aggregate 1 MiB / 4,096
row limits. These are component budgets, not a total-process RAM claim. Long
animations should yield or submit updates over multiple host ticks so mutation
output is drained within the normal response limit.

Received CFrame, Size, PrimaryPart and pivot metadata must be available when
needed. Missing fields are errors; a dormant, unreceived WorldPivot is left
unknown when a known PrimaryPart supplies the effective pivot. These methods
update local world state and author local mutations. They do not establish
server physics ownership or native Roblox solver parity.

Reference: [Model](https://create.roblox.com/docs/reference/engine/classes/Model),
[PVInstance](https://create.roblox.com/docs/reference/engine/classes/PVInstance),
[BasePart](https://create.roblox.com/docs/reference/engine/classes/BasePart).

## verification

```sh
cargo test --locked --manifest-path rust-client/Cargo.toml --test model_pivots
rove run --snapshot tests/fixtures/world_snapshot.json --script examples/model-pivots-smoke.luau --json
```

The real-Luau tests cover analytic rotated bounds, pivot offsets, cloning,
received updates, empty and nested models, event visibility, invalid-input
rollback and 800 moves across persistent runtime calls. Actor regression tests
exercise atomic merge and stale-target rejection. Windows CI also runs the
12-check smoke script from the release package.
