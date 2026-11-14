# part collision queries

The Rust host implements `WorldRoot:Shapecast` and `WorldRoot:GetPartsInPart`
on Workspace and WorldModel. Scripts can sweep a part through a scene or find
parts intersecting its occupied volume. Both use the current world catalog,
including received creates, property updates, removals and local edits.
Nested WorldModels have separate collision worlds.

```lua
local hit = workspace:Shapecast(character.HumanoidRootPart, Vector3.new(0,-8,0))
if hit then
    print(hit.Instance, hit.Distance, hit.Position, hit.Normal)
end

local params = OverlapParams.new()
params.ExcludeInstances = {character}
for _, part in workspace:GetPartsInPart(sensor, params) do
    print(part.Name)
end
```

Shapecast excludes the source part and ignores candidates already touching
or intersecting it at the start. Optional CFrame and Size overrides affect only
the query; they do not mutate the source or fire property signals. RaycastParams
Margin expands the source by a constant distance, while SizeGradient scales its
geometry over the sweep. Zero sizes are accepted for hypothetical queries.
GetPartsInPart requires its source to belong to the queried WorldRoot; a sweep
may use an unparented template from the same runtime.

Queries apply include/exclude and legacy descendant filters, collision groups,
CanQuery, RespectCanCollide and BruteForceAllSlow. OverlapParams additionally
supports MaxParts and Tolerance. Shapecast returns the first contact as a
RaycastResult, including the target, travel distance, target contact point,
surface normal and material.

## geometry and limits

Boxes, balls/ellipsoids, cylinders, wedges and corner wedges use their oriented
convex geometry. MeshPart/union queries require decoded `triangle_mesh`
CollisionGeometry already present in the world. Closed manifold meshes support
containment and cavities using odd/even shell parity; open meshes represent
surfaces. The host does not retrieve mesh assets, cook convex decompositions,
or infer collision geometry from an asset ID or bounding box.

Shapecast also queries received voxel and triangle Terrain geometry and honors
IgnoreWater. GetPartsInPart returns BaseParts and does not return Terrain.
These queries are local observations; they do not move a character or send
physics updates to a server.

Each query retains the source and at most one decoded candidate. There is no
persistent duplicate scene mesh cache. Limits are 100,000 catalog nodes,
65,536 vertices and 131,072 triangles per decoded mesh, and a shared bounded
geometry-work budget. Shapecast limits travel to 1,024 studs and expanded size
to 512 studs per axis. Missing geometry, invalid receipts, nonconvergence and
exhausted work budgets produce errors, not fabricated hits or misses. Large
mesh pairs can reach the work budget before all triangles are compared.

The implementation follows the documented API shape and has analytic and
real-Luau regression tests. It has not been differentially validated against
Roblox's native collision solver. Self-intersecting/nonmanifold meshes,
native mesh cooking, smooth Terrain interpolation and numerical edge cases
can differ from Roblox.

Reference: [WorldRoot](https://create.roblox.com/docs/reference/engine/classes/WorldRoot),
[RaycastParams](https://create.roblox.com/docs/reference/engine/datatypes/RaycastParams),
[OverlapParams](https://create.roblox.com/docs/reference/engine/datatypes/OverlapParams).

## local verification

```sh
cargo test --locked --manifest-path rust-client/Cargo.toml --test part_queries
rove run --snapshot tests/fixtures/world_snapshot.json --script examples/spatial-query-smoke.luau --json
```

The smoke script creates and destroys its own WorldModel and runs 12 checks.
Windows CI runs the same integration suite, builds the release package and
executes the bundled smoke script with `rove.exe`.
