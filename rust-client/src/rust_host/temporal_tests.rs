use super::{install, local_offset_ms};
use crate::rust_host::values;
use mlua::Lua;
use serde_json::json;

fn runtime() -> Lua {
    let lua = Lua::new();
    install(&lua).expect("DateTime and Random install");
    lua
}

#[test]
fn datetime_handles_leap_days_epoch_flooring_and_iso_format() {
    let lua = runtime();
    let (stamp, iso, year, month, day, ms): (f64, String, i64, i64, i64, i64) = lua
        .load(
            r#"
            local d = DateTime.fromUniversalTime(2024, 2, 29, 23, 59, 58, 123)
            local parts = d:ToUniversalTime()
            local preEpoch = DateTime.fromUnixTimestamp(-0.0001)
            return d.UnixTimestampMillis, d:ToIsoDate(), parts.Year, parts.Month,
                parts.Day, DateTime.fromUnixTimestampMillis(-1).UnixTimestampMillis
        "#,
        )
        .eval()
        .expect("UTC DateTime behavior");
    assert_eq!(iso, "2024-02-29T23:59:58Z");
    assert_eq!((year, month, day, ms), (2024, 2, 29, -1));
    assert!(stamp > 1_700_000_000_000.0);
    let epoch: f64 = lua
        .load("return DateTime.fromUnixTimestamp(-0.0001).UnixTimestampMillis")
        .eval()
        .expect("negative subsecond behavior");
    assert_eq!(epoch, 0.0);
    let formatted: String = lua
        .load("return DateTime.fromIsoDate('2020-02-29T03:04:05.006Z'):FormatUniversalTime('%Y-%m-%d %H:%M:%S.%L')")
        .eval()
        .expect("DateTime format");
    assert_eq!(formatted, "2020-02-29 03:04:05.006");
    let invalid: bool = lua
        .load("return DateTime.fromIsoDate('2023-02-29') == nil and not pcall(function() DateTime.fromUniversalTime(2023, 2, 29) end)")
        .eval()
        .expect("invalid calendar date");
    assert!(invalid);
}

#[test]
fn datetime_local_constructors_use_the_host_current_offset() {
    let lua = runtime();
    let (local, utc): (f64, f64) = lua
        .load(r#"
            local d = DateTime.fromLocalTime(2024, 6, 1, 12, 30, 0, 250)
            local p = d:ToLocalTime()
            assert(p.Year == 2024 and p.Month == 6 and p.Day == 1)
            assert(p.Hour == 12 and p.Minute == 30 and p.Millisecond == 250)
            return d.UnixTimestampMillis, DateTime.fromUniversalTime(2024, 6, 1, 12, 30, 0, 250).UnixTimestampMillis
        "#)
        .eval()
        .expect("local DateTime behavior");
    assert_eq!(local + local_offset_ms() as f64, utc);
}

#[test]
fn datetime_codec_round_trip_and_range_validation() {
    let lua = runtime();
    let tag = json!({"$datetime": 1_577_934_245_006i64});
    let decoded = values::from_json(&lua, &tag).expect("DateTime decode");
    assert_eq!(
        values::to_json(&lua, &decoded).expect("DateTime encode"),
        tag
    );
    assert!(values::from_json(&lua, &json!({"$datetime": 1_000_000_000_000_000i64})).is_err());
}

#[test]
fn random_clone_seed_truncation_distributions_and_shuffle_are_deterministic() {
    let lua = runtime();
    let (same_seed, clone_next, in_range, same_shuffle): (bool, bool, bool, bool) = lua
        .load(
            r#"
            local a = Random.new(123.9)
            local b = Random.new(123)
            local sameSeed = a:NextNumber() == b:NextNumber()
            local c = a:Clone()
            local cloneNext = a:NextNumber() == c:NextNumber()
            local n = a:NextInteger(9, -4)
            local inRange = n >= -4 and n <= 9
            local x, y = Random.new(7), Random.new(7)
            local ax, ay = {1,2,3,4,5,6}, {1,2,3,4,5,6}
            x:Shuffle(ax); y:Shuffle(ay)
            local sameShuffle = true
            for i=1,#ax do if ax[i] ~= ay[i] then sameShuffle = false end end
            return sameSeed, cloneNext, inRange, sameShuffle
        "#,
        )
        .eval()
        .expect("Random compatibility behavior");
    assert!(same_seed && clone_next && in_range && same_shuffle);
    let rejected: bool = lua
        .load("local r=Random.new(1); local frozen=table.freeze({1,2,3}); return not pcall(function() r:Shuffle(frozen) end)")
        .eval()
        .expect("frozen shuffle rejection");
    assert!(rejected);
}
