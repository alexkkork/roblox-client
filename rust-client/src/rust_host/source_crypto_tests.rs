use super::*;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use mlua::{LuaString, Value};

fn crypto_lua() -> Lua {
    let lua = Lua::new();
    let globals = lua.globals();
    let encode = lua
        .create_function(|lua, input: LuaString| {
            lua.create_string(STANDARD.encode(input.as_bytes().as_ref()))
        })
        .unwrap();
    let decode = lua
        .create_function(|lua, input: String| {
            let bytes = STANDARD
                .decode(input)
                .map_err(|_| error("invalid Base64 data"))?;
            lua.create_string(bytes)
        })
        .unwrap();
    install(&lua, &globals, encode, decode).unwrap();
    lua
}

#[test]
fn supported_hashes_and_sha256_hmac_match_known_vectors() {
    let lua = crypto_lua();
    assert_eq!(
        lua.load("return crypt.hash('abc','sha256')")
            .eval::<String>()
            .unwrap(),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert_eq!(
        lua.load("return crypt.hash('abc','sha384')")
            .eval::<String>()
            .unwrap(),
        "cb00753f45a35e8bb5a03d699ac65007272c32ab0eded1631a8b605a43ff5bed8086072ba1e7cc2358baeca134c825a7"
    );
    assert_eq!(
        lua.load("return crypt.hash('abc','sha3-256')")
            .eval::<String>()
            .unwrap(),
        "3a985da74fe225b2045c172d6bd390bd855f086e3e9d525b46bfe24511431532"
    );
    assert_eq!(
        lua.load("return crypt.hmac('The quick brown fox jumps over the lazy dog','key','sha256')")
            .eval::<String>()
            .unwrap(),
        "f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8"
    );
}

#[test]
fn aes_roundtrips_and_gcm_checks_authentication() {
    let lua = crypto_lua();
    let result: mlua::Result<(String, String)> = lua
        .load(
            "local key=base64encode(string.rep('k',32)); local iv=base64encode('0123456789abcdef'); return crypt.encrypt('a\\0b\\255',key,iv,'CBC')",
        )
        .eval();
    let (ciphertext, iv) = result.unwrap();
    let plaintext: LuaString = lua
        .load("return crypt.decrypt(...)")
        .call((ciphertext, STANDARD.encode([b'k'; 32]), iv, "CBC"))
        .unwrap();
    assert_eq!(plaintext.as_bytes().as_ref(), &[b'a', 0, b'b', 255]);

    let (sealed, nonce): (String, String) = lua
        .load(
            "local key=base64encode(string.rep('g',32)); return crypt.encrypt('private',key,nil,'GCM')",
        )
        .eval()
        .unwrap();
    let mut tampered = STANDARD.decode(sealed).unwrap();
    tampered[0] ^= 1;
    let tampered = STANDARD.encode(tampered);
    assert!(lua
        .load("return crypt.decrypt(...)")
        .call::<String>((tampered, STANDARD.encode([b'g'; 32]), nonce, "GCM"))
        .is_err());
}

#[test]
fn secure_generators_base64_aliases_tick_and_gc_count_are_real() {
    let lua = crypto_lua();
    let (bytes, key, aliases_match, tick, gc_kib): (String, String, bool, f64, f64) = lua
        .load(
            "local bytes=crypt.generatebytes(24); local key=crypt.generatekey(); return bytes,key,base64.encode==base64encode and base64.decode==base64decode,tick(),collectgarbage('count')",
        )
        .eval()
        .unwrap();
    assert_eq!(STANDARD.decode(bytes).unwrap().len(), 24);
    assert_eq!(STANDARD.decode(key).unwrap().len(), 32);
    assert!(aliases_match);
    assert!(tick > 1_600_000_000.0);
    assert!(gc_kib > 0.0);
    assert!(lua
        .load("return collectgarbage('collect')")
        .eval::<Value>()
        .is_err());
    assert!(lua
        .load("return crypt.generatebytes(131073)")
        .eval::<Value>()
        .is_err());
}

#[test]
fn aes_modes_match_nist_first_blocks_and_roundtrip_binary_data() {
    fn unhex(text: &str) -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
            .collect()
    }
    // NIST SP 800-38A, F: AES-256 examples. Padding adds a second CBC/ECB
    // block, so compare the published first block and verify full decryption.
    let key = unhex("603deb1015ca71be2b73aef0857d77811f352c073b6108d72d9810a30914dff4");
    let plain = unhex("6bc1bee22e409f96e93d7e117393172a");
    for (mode, iv, expected) in [
        (
            "cbc",
            "000102030405060708090a0b0c0d0e0f",
            "f58c4c04d6e5f1ba779eabfb5f7bfbd6",
        ),
        ("ecb", "", "f3eed1bdb5d2a03c064b5a7e3db181f8"),
        (
            "cfb",
            "000102030405060708090a0b0c0d0e0f",
            "dc7e84bfda79164b7ecd8486985d3860",
        ),
        (
            "ofb",
            "000102030405060708090a0b0c0d0e0f",
            "dc7e84bfda79164b7ecd8486985d3860",
        ),
        (
            "ctr",
            "f0f1f2f3f4f5f6f7f8f9fafbfcfdfeff",
            "601ec313775789a5b7a7f504bbf3d228",
        ),
    ] {
        let iv = unhex(iv);
        let encrypted = aes_crypt(&plain, &key, &iv, mode, false).unwrap();
        assert_eq!(hex(&encrypted[..16]), expected, "{mode}");
        assert_eq!(
            aes_crypt(&encrypted, &key, &iv, mode, true).unwrap(),
            plain,
            "{mode}"
        );
        for size in [0, 1, 15, 17, 33, MAX_CRYPTO_INPUT] {
            let plain: Vec<_> = (0..size).map(|n| n as u8).collect();
            let encrypted = aes_crypt(&plain, &key, &iv, mode, false).unwrap();
            assert_eq!(
                aes_crypt(&encrypted, &key, &iv, mode, true).unwrap(),
                plain,
                "{mode}/{size}"
            );
        }
    }
    // AES-256-GCM: all-zero key, nonce, and one-block plaintext.
    let encrypted = aes_crypt(&[0; 16], &[0; 32], &[0; 12], "gcm", false).unwrap();
    assert_eq!(
        hex(&encrypted),
        "cea7403d4d606b6e074ec5d3baf39d18d0d1c8a799996bf0265b98b5d48ab919"
    );
    assert_eq!(
        aes_crypt(&encrypted, &[0; 32], &[0; 12], "gcm", true).unwrap(),
        [0; 16]
    );
    for length in [16, 24, 32] {
        let key = vec![0; length];
        let ciphertext = aes_crypt(b"abc", &key, &[0; 16], "cbc", false).unwrap();
        assert_eq!(
            aes_crypt(&ciphertext, &key, &[0; 16], "cbc", true).unwrap(),
            b"abc"
        );
    }
}
