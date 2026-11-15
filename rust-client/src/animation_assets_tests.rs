use super::*;

#[test]
fn references_normalize_only_roblox_asset_ids() {
    for reference in [
        "rbxassetid://00123",
        "http://www.roblox.com/asset/?id=123",
        "https://roblox.com/asset?id=123",
    ] {
        assert_eq!(
            Reference::parse(reference).unwrap().canonical(),
            "rbxassetid://123"
        );
    }
    for reference in [
        "rbxassetid://0",
        "rbxassetid://-1",
        "https://roblox.com.evil.test/asset?id=123",
        "https://roblox.com/asset?id=1&id=2",
        "https://name@roblox.com/asset?id=1",
        "https://roblox.com:444/asset?id=1",
        "https://roblox.com/asset?id=1#fragment",
        "file:///clip",
        "123",
        "active://other-vm",
    ] {
        assert!(Reference::parse(reference).is_err(), "{reference}");
    }
}

#[test]
fn local_paths_reject_traversal_and_windows_device_syntax() {
    for reference in [
        "rbxasset:///tmp/a",
        "rbxasset://../a",
        "rbxasset://a/../b",
        "rbxasset://a//b",
        "rbxasset://a\\b",
        "rbxasset://C:/file",
        "rbxasset://file:stream",
        "rbxasset://%2e%2e/file",
        "rbxasset://a?b",
    ] {
        assert_eq!(Reference::parse(reference), Err(AssetError::InvalidPath));
    }
    assert_eq!(
        Reference::parse("rbxasset://animations/walk.rbxm")
            .unwrap()
            .canonical(),
        "rbxasset://animations/walk.rbxm"
    );
}

#[test]
fn delivery_urls_never_accept_an_unrelated_host_or_credentials() {
    assert!(cdn_url("https://contentdelivery.roblox.com/v1/asset/opaque-signed-reference").is_ok());
    assert!(cdn_url("https://subdomain.contentdelivery.roblox.com/file").is_err());
    let url = "https://c0.rbxcdn.com/animation?token=not-a-real-secret";
    assert_eq!(
        delivery_location(&json!({"locations":[{"location":url}]})).unwrap(),
        url
    );
    for url in [
        "http://c0.rbxcdn.com/file",
        "https://rbxcdn.com.evil.test/file",
        "https://user:password@c0.rbxcdn.com/file",
        "https://c0.rbxcdn.com:444/file",
        "https://127.0.0.1/file",
        "https://roblox.com/file",
        "file:///tmp/file",
    ] {
        assert_eq!(cdn_url(url), Err(AssetError::InvalidRedirect));
    }
    for data in [
        json!({}),
        json!({"locations":[]}),
        json!({"locations":[{"location":url}],"errors":[{"message":"denied"}]}),
    ] {
        assert_eq!(delivery_location(&data), Err(AssetError::InvalidDelivery));
    }
}

#[test]
fn offline_backend_decodes_real_xml_and_reports_missing_authentication() {
    let backend = AnimationAssetBackend::local(Some(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../tests/fixtures"),
    ));
    let clip = backend
        .load("rbxasset://animations/two-keyframes.rbxmx")
        .unwrap();
    assert_eq!(clip["length"], 2.0);
    assert_eq!(
        clip["keyframes"][1]["poses"][1]["position"],
        json!([4.0, 0.0, 0.0])
    );
    assert_eq!(clip["source_sha256"].as_str().unwrap().len(), 64);
    assert_eq!(
        backend.load("rbxassetid://123"),
        Err(AssetError::AuthenticationRequired)
    );
    assert_eq!(
        backend.load("rbxasset://animations/missing.rbxm"),
        Err(AssetError::LocalUnavailable)
    );
}

#[test]
fn bounded_reads_reject_empty_and_oversized_assets() {
    assert_eq!(read_bounded(&[][..]), Err(AssetError::TooLarge));
    assert_eq!(
        read_bounded(std::io::repeat(0).take((MAX_BYTES + 1) as u64)),
        Err(AssetError::TooLarge)
    );
}

#[cfg(unix)]
#[test]
fn local_symlinks_cannot_escape_the_content_root() {
    let base = std::env::temp_dir().join(format!("rove-animation-symlink-{}", std::process::id()));
    fs::create_dir_all(base.join("content")).unwrap();
    fs::write(base.join("outside"), b"not an animation").unwrap();
    std::os::unix::fs::symlink(base.join("outside"), base.join("content/clip")).unwrap();
    let result = AnimationAssetBackend::local(Some(base.join("content"))).load("rbxasset://clip");
    fs::remove_dir_all(base).unwrap();
    assert_eq!(result, Err(AssetError::InvalidPath));
}
