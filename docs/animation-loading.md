# animation loading

`Animator:LoadAnimation` now requests the asset through a Rust worker. A track can
start before loading finishes: Length becomes available when the decoded clip is
installed, then normal timeline, marker and pose playback takes over.

Supported inputs are binary RBXM and XML RBXMX KeyframeSequence files, including
gzip-wrapped data. The decoder runs in process; no Python or decoder subprocess
is involved. CurveAnimation and facial/retargeting playback remain unfinished.

## local files

Set AnimationId to `rbxasset://animations/two-keyframes.rbxmx`. The loader resolves
it under `--content-root`, `ROBLOX_CONTENT_DIR`, or the discovered Roblox content
directory. Paths cannot escape that directory. Offline commands do not use saved
account cookies or download remote assets.

```powershell
.\rove.exe run --snapshot tests/fixtures/world_snapshot.json --script examples/animation-assets-smoke.luau --content-root tests/fixtures --json
```

This checks asynchronous loading, clip metadata, Motor6D/Bone playback, part
movement, clip reuse and stopping through the packaged executable.

## live sessions

`rove join --script ...` gives the animation worker the same authenticated HTTP
session used for the join. `rbxassetid://123` and Roblox `/asset?id=123` URLs are
normalized to one asset request. The worker calls Roblox's asset-delivery API,
then reads the returned HTTPS content URL without forwarding the account cookie.
Only Roblox's content-delivery host and rbxcdn hosts are accepted, including on
redirects. Account/asset permissions still apply; a challenge or failed request
becomes a loading error, not a replacement clip.

Root and Actor VMs share the worker and download cache. Completion is routed to
the requesting VM generation, so a late reply cannot initialize a recreated
Actor. Manually supplied `animation_responses` still work and take precedence
over a delayed automatic completion.

## limits and errors

There is one loading thread, at most 128 cached/pending assets and 256 pending
requesters. Input is limited to 4 MiB, including gzip expansion. Existing binary
container limits also apply, including 65,536 instances. Default pose weights and
easing values are omitted from the decoded JSON without changing their meaning.
Serialized decoded results share an 8 MiB cache;
one in-flight result can temporarily add another 8 MiB. Decoder DOMs, request
objects and per-VM compiled clips are additional memory, not covered by that
serialized-cache limit. The worker applies backpressure when completed results
fill the cache. A frame receives up to four completions and normally 2 MiB of
results; one larger result is allowed by itself. Downloads have finite timeouts
and runtime shutdown does not wait on an in-flight request.

`animation_load_errors` records a reference and a stable error code. A failed
asset stops its waiting tracks; replaying one reports the failure. A malformed
or unsupported clip fails only its own completion, leaving the session and other
animations running. Signed delivery URLs, cookies and response bodies are not
included in these errors.

For an explicitly authorized remote asset check, set `ROVE_ANIMATION_COOKIE_FILE`
to the private account file and `ROVE_ANIMATION_ASSET_ID` to an accessible clip ID,
then run the ignored `authorized_asset_delivery_decodes_a_real_clip` integration
test. The normal suite uses authored local files and requires no account.
