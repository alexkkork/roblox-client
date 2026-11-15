# editable animation curves

The Rust host implements FloatCurve, RotationCurve, Vector3Curve and
EulerRotationCurve, plus the mutable FloatCurveKey datatype. The official Luau VM
runs the scripts; curve storage and sampling stay in Rust.

FloatCurve and RotationCurve support GetKeys, GetKeyAtIndex,
GetKeyIndicesAtTime, InsertKey, RemoveKeyAtIndex, SetKeys and GetValueAtTime.
Length reports the number of keys. Input and returned keys are independent
copies. SetKeys sorts keys by time and keeps the first input at each duplicate
time; InsertKey replaces an existing key at the same time. Invalid edits leave
the existing curve intact.

Scalar samples support Constant, Linear and cubic Hermite segments. Cubic
tangents are slopes scaled by the segment duration; an unassigned tangent is
zero. Sampling outside the key range returns the nearest endpoint. Empty scalar
curves return nil. Rotation curves ignore translation, follow the shortest
rotation arc, and return identity when empty. Their cubic mode evaluates a
Hermite parameter before spherical interpolation.

Vector3Curve and EulerRotationCurve expose X(), Y() and Z(). Each finds the first
FloatCurve child with that axis name, or creates one. Sampling does not create
children. Vector samples and GetAnglesAtTime return tables whose missing channels
are nil. GetRotationAtTime treats missing angles as zero and composes radians
using RotationOrder; all six orders are implemented.

Cloning shares immutable key storage until an edit replaces it. Actor changes
carry a fingerprint of the curve they edited, so stale edits cannot overwrite a
newer root change. Clone creation records preserve the keys at clone time, and
Length notifications run after the new keys are committed.

Limits: 16,384 keys per curve, 8 MiB of accounted curve storage per world, and the
existing 1 MiB / 4,096-row mutation journal boundary. Inputs must be finite and
fit a float. Oversized or malformed changes fail before replacing stored keys.

This implements editable local curves and their sampling APIs. It does not yet
decode received ValuesAndTimes binary payloads or drive AnimationTracks from
CurveAnimation assets. Missing received key data stays unavailable; SetKeys can
explicitly replace it. Native Studio differential checks, including nonzero
rotation tangents and duplicate-key edge cases, remain to be measured.

Run `examples/animation-curves-smoke.luau` with the normal offline snapshot to
exercise the packaged executable.

API references: [FloatCurve](https://create.roblox.com/docs/reference/engine/classes/FloatCurve),
[RotationCurve](https://create.roblox.com/docs/reference/engine/classes/RotationCurve),
[Vector3Curve](https://create.roblox.com/docs/reference/engine/classes/Vector3Curve),
[EulerRotationCurve](https://create.roblox.com/docs/reference/engine/classes/EulerRotationCurve),
[FloatCurveKey](https://create.roblox.com/docs/reference/engine/datatypes/FloatCurveKey).
