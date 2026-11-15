use super::*;

fn backend() -> LocalTextBackend {
    LocalTextBackend::new(Path::new(env!("CARGO_MANIFEST_DIR")).join("../tests/fixtures"))
}

fn request(text: &str, width: f64) -> Value {
    json!({"kind":"bounds","request_id":1,"text":text,"size":20,"width":width,
        "font":{"family":"rbxasset://fonts/families/NotoSans.json","weight":400,"style":0}})
}

#[test]
fn family_lookup_and_asset_paths_stay_under_the_selected_root() {
    let backend = backend();
    let result = backend
        .handle(&TextRequest::Family {
            request_id: 1,
            family: "rbxasset://fonts/families/NotoSans.json".into(),
        })
        .unwrap();
    assert_eq!(result["Name"], "Noto Sans");
    assert_eq!(result["Faces"][0]["Weight"], 400);
    for suffix in [
        "fonts/../fonts/NotoSans-Regular.ttf",
        "/fonts/NotoSans-Regular.ttf",
        "fonts\\NotoSans-Regular.ttf",
        "C:/fonts/file.ttf",
        "fonts/file.ttf:stream",
    ] {
        assert!(backend.checked_local_path(suffix).is_err(), "{suffix}");
    }
    assert!(backend.family_data("rbxassetid://1").is_err());
    assert!(backend
        .family_data("rbxasset://fonts/families/missing.json")
        .is_err());
}

#[test]
fn portable_bounds_wrap_words_and_keep_combining_graphemes_together() {
    let backend = backend();
    let line = backend.bounds(&request("Hello", 0.0)).unwrap();
    let wrapped = backend.bounds(&request("Hello world", 55.0)).unwrap();
    assert!(line[0].as_u64().unwrap() > 0);
    assert_eq!(line[1], 20);
    assert_eq!(wrapped[1], 40);
    assert!(wrapped[0].as_u64().unwrap() <= 55);
    assert_eq!(
        backend.bounds(&request("e\u{301}e\u{301}", 1.0)).unwrap()[1],
        40
    );
    assert_eq!(backend.bounds(&request("", 1.0)).unwrap(), json!([0, 0]));
}

#[test]
fn rich_sizes_crlf_and_unicode_newlines_have_explicit_line_heights() {
    let backend = backend();
    let mut req = request("A\r\nB", 0.0);
    req["runs"] = json!([
        {"text":"A\r","style":{"size":20}},
        {"text":"\nB","style":{"size":40}}
    ]);
    req["line_height"] = json!(1.5);
    assert_eq!(backend.bounds(&req).unwrap()[1], 70);
    assert_eq!(
        backend.bounds(&request("A\u{2028}B\u{2029}", 0.0)).unwrap()[1],
        60
    );
    assert_eq!(backend.bounds(&request("A\r\nB\rC", 0.0)).unwrap()[1], 60);
}

#[test]
fn nonbreaking_spaces_keep_their_advance_and_missing_glyphs_fail_explicitly() {
    let backend = backend();
    let plain = backend.bounds(&request("A", 0.0)).unwrap();
    let nbsp = backend.bounds(&request("A\u{a0}", 0.0)).unwrap();
    assert!(nbsp[0].as_u64().unwrap() > plain[0].as_u64().unwrap());
    assert!(!breakable_space('\u{a0}'));
    assert!(!breakable_space('\u{202f}'));
    let error = backend.bounds(&request("\u{10ffff}", 0.0)).unwrap_err();
    assert!(error.contains("required glyph"), "{error}");
}

struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let mut random = [0u8; 8];
        rove_client_core::entropy::fill(&mut random).unwrap();
        let path =
            std::env::temp_dir().join(format!("rove-fonts-{:016x}", u64::from_le_bytes(random)));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn font_cache_evicts_real_files_and_rejects_changed_corrupt_data() {
    let temp = Temp::new();
    let backend = LocalTextBackend::new(&temp.0);
    let raw = include_bytes!("../../../tests/fixtures/fonts/NotoSans-Regular.ttf");
    for index in 0..12 {
        let path = temp.0.join(format!("font-{index}.ttf"));
        fs::write(&path, raw).unwrap();
        backend.load_font(&path).unwrap();
        let cache = backend.fonts.lock().unwrap();
        assert!(cache.bytes <= MAX_CACHED_FONT_BYTES);
        assert!(cache.entries.len() <= MAX_CACHED_FONTS);
    }
    let cache = backend.fonts.lock().unwrap();
    assert!(!cache
        .entries
        .iter()
        .any(|entry| entry.path.ends_with("font-0.ttf")));
    drop(cache);
    let path = temp.0.join("font-11.ttf");
    fs::write(&path, b"not a font").unwrap();
    assert!(backend.load_font(&path).is_err());
    let path = temp.0.join("oversized.ttf");
    File::create(&path)
        .unwrap()
        .set_len(MAX_FONT_BYTES as u64 + 1)
        .unwrap();
    assert!(backend.load_font(&path).is_err());
}

#[cfg(unix)]
#[test]
fn font_asset_symlinks_cannot_escape_the_content_root() {
    let temp = Temp::new();
    let outside = Temp::new();
    let path = outside.0.join("font.ttf");
    fs::write(
        &path,
        include_bytes!("../../../tests/fixtures/fonts/NotoSans-Regular.ttf"),
    )
    .unwrap();
    std::os::unix::fs::symlink(path, temp.0.join("font.ttf")).unwrap();
    assert!(LocalTextBackend::new(&temp.0)
        .checked_local_path("font.ttf")
        .is_err());
}

#[test]
fn malformed_and_oversized_runs_do_not_reach_the_shaper() {
    let backend = backend();
    let mut req = request("x", 0.0);
    req["runs"] = json!([{ "text":"x".repeat(4097), "style":{} }]);
    assert!(backend.bounds(&req).is_err());
    assert!(TextRequest::parse(&req).is_err());
    req["runs"] = json!([{ "text":"different", "style":{} }]);
    assert!(backend.bounds(&req).is_err());
    assert!(backend.bounds(&request(&"x".repeat(4097), 0.0)).is_err());
    let mut req = request("x", 0.0);
    req["runs"] = json!([{ "text":"x", "style":{"size":1001} }]);
    assert!(backend.bounds(&req).is_err());
}
