`Require/` is from the official Luau repository at commit
`5bc7f4b23756f69f4669b419fa9034f117ccd6fe`. This is provenance for the
optional Require library only; it does not identify the VM revision in
`vendor/luau`. The current `vendor/luau/Common` has a newer `DenseHashSet`
constructor API, so `AliasCycleTracker` uses its default constructor instead
of the removed empty-string sentinel constructor. This only adapts the source
to the checked-in Common headers.
