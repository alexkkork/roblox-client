//! Standard local cryptography helpers for a single Luau VM.
//!
//! Random bytes come from the operating system. RustCrypto supplies the same
//! AES and digest implementations on Windows, macOS, and Linux.

use aes::cipher::{
    block_padding::Pkcs7, AsyncStreamCipher, BlockDecryptMut, BlockEncryptMut, KeyInit, KeyIvInit,
    StreamCipher,
};
use aes_gcm::aead::Aead;
use mlua::{Function, Lua, LuaString, Table};
use sha2::{Digest, Sha256, Sha384, Sha512};
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_CRYPTO_INPUT: usize = 256 * 1024;
const MAX_RANDOM_BYTES: usize = 128 * 1024;

fn error(message: impl Into<String>) -> mlua::Error {
    mlua::Error::RuntimeError(message.into())
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        result.push(HEX[(byte >> 4) as usize] as char);
        result.push(HEX[(byte & 15) as usize] as char);
    }
    result
}

fn secure_bytes(size: usize) -> mlua::Result<Vec<u8>> {
    if size > MAX_RANDOM_BYTES {
        return Err(error("random byte count must be at most 131072"));
    }
    let mut bytes = vec![0; size];
    if size > 0 {
        rove_client_core::entropy::fill(&mut bytes).map_err(|_| error("OS entropy unavailable"))?;
    }
    Ok(bytes)
}

fn hmac_sha256(key: &[u8], message: &[u8]) -> Vec<u8> {
    const BLOCK: usize = 64;
    let mut key_block = [0u8; BLOCK];
    if key.len() > BLOCK {
        key_block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }
    let mut inner = Vec::with_capacity(BLOCK + message.len());
    let mut outer = Vec::with_capacity(BLOCK + 32);
    inner.extend(key_block.iter().map(|byte| byte ^ 0x36));
    inner.extend_from_slice(message);
    outer.extend(key_block.iter().map(|byte| byte ^ 0x5c));
    outer.extend_from_slice(&Sha256::digest(inner));
    Sha256::digest(outer).to_vec()
}

fn digest(algorithm: &str, bytes: &[u8]) -> mlua::Result<Vec<u8>> {
    if bytes.len() > MAX_CRYPTO_INPUT {
        return Err(error("crypt input exceeds 256 KiB"));
    }
    let result = match algorithm {
        "sha256" | "sha-256" => Sha256::digest(bytes).to_vec(),
        "sha384" => Sha384::digest(bytes).to_vec(),
        "sha512" => Sha512::digest(bytes).to_vec(),
        "sha1" => sha1::Sha1::digest(bytes).to_vec(),
        "md5" => md5::Md5::digest(bytes).to_vec(),
        "sha3-224" => sha3::Sha3_224::digest(bytes).to_vec(),
        "sha3-256" => sha3::Sha3_256::digest(bytes).to_vec(),
        "sha3-512" => sha3::Sha3_512::digest(bytes).to_vec(),
        "blake2b" | "blake2b-256" | "blake3" => {
            let parameter = match algorithm {
                "blake2b" | "blake2b-256" => 0,
                _ => 1,
            };
            super::codec(6, bytes, parameter)?
        }
        _ => return Err(error("unsupported hash algorithm")),
    };
    Ok(result)
}

fn cipher_mode(mode: Option<&str>) -> mlua::Result<(&'static str, usize, bool)> {
    match mode.unwrap_or("CBC") {
        "CBC" => Ok(("cbc", 16, false)),
        "CFB" => Ok(("cfb", 16, false)),
        "OFB" => Ok(("ofb", 16, false)),
        "CTR" => Ok(("ctr", 16, false)),
        "ECB" => Ok(("ecb", 0, false)),
        "GCM" => Ok(("gcm", 12, true)),
        _ => Err(error("unsupported AES mode")),
    }
}

fn normalize_iv(input: &[u8], size: usize, encrypt: bool) -> mlua::Result<Vec<u8>> {
    if size == 0 {
        return Ok(Vec::new());
    }
    if input.is_empty() {
        if encrypt {
            return secure_bytes(size);
        }
        return Err(error("decryption requires an IV"));
    }
    if input.len() == size {
        return Ok(input.to_vec());
    }
    Ok((0..size).map(|index| input[index % input.len()]).collect())
}

fn aes_crypt(
    input: &[u8],
    key: &[u8],
    iv: &[u8],
    mode: &str,
    decrypt: bool,
) -> mlua::Result<Vec<u8>> {
    // Encryption can add one padding block or an authentication tag.
    let limit = MAX_CRYPTO_INPUT + if decrypt { 16 } else { 0 };
    if input.len() > limit {
        return Err(error("crypt input exceeds configured limits"));
    }
    let invalid_key = || error("invalid AES key or IV length");
    let invalid_ciphertext = || error("AES operation failed (invalid ciphertext or padding)");
    macro_rules! cbc {
        ($cipher:ty) => {
            if decrypt {
                cbc::Decryptor::<$cipher>::new_from_slices(key, iv)
                    .map_err(|_| invalid_key())?
                    .decrypt_padded_vec_mut::<Pkcs7>(input)
                    .map_err(|_| invalid_ciphertext())
            } else {
                Ok(cbc::Encryptor::<$cipher>::new_from_slices(key, iv)
                    .map_err(|_| invalid_key())?
                    .encrypt_padded_vec_mut::<Pkcs7>(input))
            }
        };
    }
    if mode == "cbc" {
        return match key.len() {
            16 => cbc!(aes::Aes128),
            24 => cbc!(aes::Aes192),
            32 => cbc!(aes::Aes256),
            _ => Err(invalid_key()),
        };
    }
    if key.len() != 32 {
        return Err(invalid_key());
    }
    match mode {
        "gcm" => {
            if iv.len() != 12 {
                return Err(invalid_key());
            }
            let cipher = aes_gcm::Aes256Gcm::new_from_slice(key).map_err(|_| invalid_key())?;
            let nonce = aes_gcm::Nonce::from_slice(iv);
            if decrypt {
                cipher
                    .decrypt(nonce, input)
                    .map_err(|_| error("AES-GCM authentication failed"))
            } else {
                cipher
                    .encrypt(nonce, input)
                    .map_err(|_| error("AES-GCM encryption failed"))
            }
        }
        "ecb" => {
            if !iv.is_empty() {
                return Err(invalid_key());
            }
            if decrypt {
                ecb::Decryptor::<aes::Aes256>::new_from_slice(key)
                    .map_err(|_| invalid_key())?
                    .decrypt_padded_vec_mut::<Pkcs7>(input)
                    .map_err(|_| invalid_ciphertext())
            } else {
                Ok(ecb::Encryptor::<aes::Aes256>::new_from_slice(key)
                    .map_err(|_| invalid_key())?
                    .encrypt_padded_vec_mut::<Pkcs7>(input))
            }
        }
        "cfb" | "ofb" | "ctr" => {
            let mut output = input.to_vec();
            match mode {
                "cfb" if decrypt => cfb_mode::Decryptor::<aes::Aes256>::new_from_slices(key, iv)
                    .map_err(|_| invalid_key())?
                    .decrypt(&mut output),
                "cfb" => cfb_mode::Encryptor::<aes::Aes256>::new_from_slices(key, iv)
                    .map_err(|_| invalid_key())?
                    .encrypt(&mut output),
                "ofb" => ofb::Ofb::<aes::Aes256>::new_from_slices(key, iv)
                    .map_err(|_| invalid_key())?
                    .apply_keystream(&mut output),
                _ => ctr::Ctr128BE::<aes::Aes256>::new_from_slices(key, iv)
                    .map_err(|_| invalid_key())?
                    .apply_keystream(&mut output),
            }
            Ok(output)
        }
        _ => Err(error("unsupported AES mode")),
    }
}

fn install_crypt(
    lua: &Lua,
    globals: &Table,
    encode: Function,
    decode: Function,
) -> mlua::Result<()> {
    let base64 = lua.create_table()?;
    base64.set("encode", encode.clone())?;
    base64.set("decode", decode.clone())?;
    base64.set_readonly(true);
    globals.set("base64", base64.clone())?;
    for name in ["base64encode", "base64_encode"] {
        globals.set(name, encode.clone())?;
    }
    for name in ["base64decode", "base64_decode"] {
        globals.set(name, decode.clone())?;
    }

    let crypt = lua.create_table()?;
    crypt.set("base64", base64)?;
    for name in ["base64encode", "base64_encode"] {
        crypt.set(name, encode.clone())?;
    }
    for name in ["base64decode", "base64_decode"] {
        crypt.set(name, decode.clone())?;
    }
    crypt.set(
        "hash",
        lua.create_function(|lua, (input, algorithm): (LuaString, String)| {
            let output = digest(&algorithm.to_ascii_lowercase(), input.as_bytes().as_ref())?;
            lua.create_string(hex(&output))
        })?,
    )?;
    crypt.set(
        "hmac",
        lua.create_function(
            |lua, (input, key, algorithm): (LuaString, LuaString, String)| {
                if input.as_bytes().len() > MAX_CRYPTO_INPUT
                    || key.as_bytes().len() > MAX_CRYPTO_INPUT
                {
                    return Err(error("crypt input exceeds 256 KiB"));
                }
                if !matches!(algorithm.as_str(), "sha256" | "sha-256") {
                    return Err(error("unsupported HMAC algorithm"));
                }
                lua.create_string(hex(&hmac_sha256(
                    key.as_bytes().as_ref(),
                    input.as_bytes().as_ref(),
                )))
            },
        )?,
    )?;
    let hmac = crypt.get::<Function>("hmac")?;
    globals.set("hmac", hmac)?;
    let encode_for_bytes = encode.clone();
    crypt.set(
        "generatebytes",
        lua.create_function(move |lua, size: usize| {
            if size > MAX_RANDOM_BYTES {
                return Err(error("random byte count must be 0..131072"));
            }
            let raw = secure_bytes(size)?;
            encode_for_bytes.call::<LuaString>(lua.create_string(raw)?)
        })?,
    )?;
    let encode_for_key = encode.clone();
    crypt.set(
        "generatekey",
        lua.create_function(move |lua, ()| {
            let raw = secure_bytes(32)?;
            encode_for_key.call::<LuaString>(lua.create_string(raw)?)
        })?,
    )?;

    let decode_for_encrypt = decode.clone();
    let encode_for_encrypt = encode.clone();
    crypt.set(
        "encrypt",
        lua.create_function(
            move |lua,
                  (input, encoded_key, encoded_iv, mode): (
                LuaString,
                String,
                Option<String>,
                Option<String>,
            )| {
                if input.as_bytes().len() > MAX_CRYPTO_INPUT {
                    return Err(error("crypt input exceeds 256 KiB"));
                }
                let key = decode_for_encrypt.call::<LuaString>(encoded_key)?;
                let key = key.as_bytes();
                if !matches!(key.len(), 16 | 24 | 32) {
                    return Err(error("AES key must decode to 16, 24, or 32 bytes"));
                }
                let (mode_name, iv_size, _) = cipher_mode(mode.as_deref())?;
                if mode_name != "cbc" && key.len() != 32 {
                    return Err(error("expected a 32-byte AES key (base64-decoded)"));
                }
                let decoded_iv = encoded_iv
                    .map(|value| decode_for_encrypt.call::<LuaString>(value))
                    .transpose()?;
                let iv_bytes = decoded_iv
                    .as_ref()
                    .map(|value| value.as_bytes().as_ref().to_vec())
                    .unwrap_or_default();
                let iv = normalize_iv(&iv_bytes, iv_size, true)?;
                let encrypted = aes_crypt(
                    input.as_bytes().as_ref(),
                    key.as_ref(),
                    &iv,
                    mode_name,
                    false,
                )?;
                let encoded_ciphertext: LuaString =
                    encode_for_encrypt.call(lua.create_string(encrypted)?)?;
                let encoded_iv: LuaString = encode_for_encrypt.call(lua.create_string(iv)?)?;
                Ok((encoded_ciphertext, encoded_iv))
            },
        )?,
    )?;

    let decode_for_decrypt = decode;
    crypt.set(
        "decrypt",
        lua.create_function(
            move |lua,
                  (encoded_input, encoded_key, encoded_iv, mode): (
                String,
                String,
                Option<String>,
                Option<String>,
            )| {
                let input = decode_for_decrypt.call::<LuaString>(encoded_input)?;
                let key = decode_for_decrypt.call::<LuaString>(encoded_key)?;
                let key = key.as_bytes();
                if !matches!(key.len(), 16 | 24 | 32) {
                    return Err(error("AES key must decode to 16, 24, or 32 bytes"));
                }
                let (mode_name, iv_size, _) = cipher_mode(mode.as_deref())?;
                if mode_name != "cbc" && key.len() != 32 {
                    return Err(error("expected a 32-byte AES key (base64-decoded)"));
                }
                let decoded_iv = encoded_iv
                    .map(|value| decode_for_decrypt.call::<LuaString>(value))
                    .transpose()?;
                let iv_bytes = decoded_iv
                    .as_ref()
                    .map(|value| value.as_bytes().as_ref().to_vec())
                    .unwrap_or_default();
                let iv = normalize_iv(&iv_bytes, iv_size, false)?;
                let decrypted = aes_crypt(
                    input.as_bytes().as_ref(),
                    key.as_ref(),
                    &iv,
                    mode_name,
                    true,
                )?;
                lua.create_string(decrypted)
            },
        )?,
    )?;
    crypt.set_readonly(true);
    globals.set("crypt", crypt)?;
    Ok(())
}

/// Installs local host crypto and the legacy VM timing/GC aliases.
pub(super) fn install(
    lua: &Lua,
    globals: &Table,
    encode: Function,
    decode: Function,
) -> mlua::Result<()> {
    let now = lua.create_function(|_, ()| {
        let seconds = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(mlua::Error::external)?
            .as_secs_f64();
        Ok(seconds)
    })?;
    globals.set("tick", now)?;
    globals.set(
        "collectgarbage",
        lua.create_function(|lua, option: String| {
            if option != "count" {
                return Err(error("collectgarbage only supports the count option"));
            }
            let whole_kib = unsafe { mlua::ffi::lua_gc(lua.state(), mlua::ffi::LUA_GCCOUNT, 0) };
            let remainder = unsafe { mlua::ffi::lua_gc(lua.state(), mlua::ffi::LUA_GCCOUNTB, 0) };
            Ok(whole_kib as f64 + remainder as f64 / 1024.0)
        })?,
    )?;
    install_crypt(lua, globals, encode, decode)
}

#[cfg(test)]
#[path = "source_crypto_tests.rs"]
mod tests;
