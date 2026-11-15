# model scaling reference

These JSON observations and matching Luau probes were copied unchanged from the
existing project's Studio 0.745 capture. The recorded version is
`0.745.0.7451137`. They have not been recaptured during this Rust port.

`model_scaling_studio745.json` contains 30 class cases. The Rust reference test
replays 453 values across 29 classes at scales 2, 0.5 and 1. Its Part case also
contains read-only assembly properties, so Part geometry, mass and velocity are
checked with separate Rust integration tests. Empty property lists are valid
for constraints without dimensional properties.

`model_scaling_studio745_modes.json` records five additional mode/infinity
observations used by the corresponding integration regressions. Both original
probe scripts are retained so the capture can be reproduced in Studio.
