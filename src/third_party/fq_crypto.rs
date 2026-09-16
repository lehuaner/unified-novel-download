//! 番茄小说 AES-128-CBC 解密 + gzip 解压缩。
//!
//! 对应 Java 端 `FqCrypto`：
//! - registerkey 解密：用固定 REG_KEY 解密响应中的 `key` 字段，取前 16 字节 hex 作为内容密钥。
//! - 内容解密：用内容密钥解密 Base64 编码的加密内容，前 16 字节为 IV，剩余为密文；
//!   解密后若以 gzip 魔数 `1f 8b` 开头则 gunzip，否则直接 UTF-8 解码。

use anyhow::{Result, anyhow};
use aes::Aes128;
use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit};
use base64::Engine;
use flate2::read::GzDecoder;
use std::io::Read;

/// registerkey 固定解密密钥（与 Java 端 `FqCrypto.REG_KEY` 一致）。
pub(crate) const REG_KEY: &str = "ac25c67ddd8f38c1b37a2348828e222e";

/// hex 字符串 → 字节。
fn hex_to_bytes(hex: &str) -> Result<Vec<u8>> {
    hex::decode(hex).map_err(|e| anyhow!("hex decode failed: {e}"))
}

/// PKCS7 去填充（块大小 16）。
fn pkcs7_unpad(data: &[u8]) -> Result<Vec<u8>> {
    if data.is_empty() {
        return Err(anyhow!("pkcs7: empty data"));
    }
    let pad = *data.last().unwrap() as usize;
    if pad == 0 || pad > 16 || pad > data.len() {
        return Err(anyhow!("pkcs7: invalid padding {pad}"));
    }
    // 验证所有填充字节
    for &b in &data[data.len() - pad..] {
        if b as usize != pad {
            return Err(anyhow!("pkcs7: padding byte mismatch"));
        }
    }
    Ok(data[..data.len() - pad].to_vec())
}

/// PKCS7 填充（块大小 16）。
fn pkcs7_pad(data: &[u8]) -> Vec<u8> {
    let pad = 16 - (data.len() % 16);
    let mut out = data.to_vec();
    out.extend(std::iter::repeat(pad as u8).take(pad));
    out
}

/// AES-128-CBC 解密（手动 CBC 模式）。
///
/// `data` = IV(16) + ciphertext，`key_hex` = 32 hex chars。
fn aes_cbc_decrypt(data: &[u8], key_hex: &str) -> Result<Vec<u8>> {
    if data.len() < 16 {
        return Err(anyhow!("encrypted data too short: {} bytes", data.len()));
    }
    let key_bytes = hex_to_bytes(key_hex)?;
    if key_bytes.len() != 16 {
        return Err(anyhow!("key must be 16 bytes, got {}", key_bytes.len()));
    }
    let (iv, cipher) = data.split_at(16);
    if cipher.len() % 16 != 0 {
        return Err(anyhow!("ciphertext length {} not multiple of 16", cipher.len()));
    }

    let cipher_key = aes::cipher::generic_array::GenericArray::from_slice(&key_bytes);
    let aes = Aes128::new(cipher_key);

    let mut prev_block: [u8; 16] = iv.try_into().unwrap();
    let mut plaintext = Vec::with_capacity(cipher.len());

    for chunk in cipher.chunks(16) {
        let mut block = aes::cipher::generic_array::GenericArray::clone_from_slice(chunk);
        aes.decrypt_block(&mut block);
        // XOR with previous ciphertext block (CBC)
        for i in 0..16 {
            block[i] ^= prev_block[i];
        }
        plaintext.extend_from_slice(&block);
        prev_block.copy_from_slice(chunk);
    }

    pkcs7_unpad(&plaintext)
}

/// AES-128-CBC 加密（手动 CBC 模式）。
///
/// 返回 IV(16) + ciphertext。
fn aes_cbc_encrypt(plaintext: &[u8], key_hex: &str) -> Result<Vec<u8>> {
    let key_bytes = hex_to_bytes(key_hex)?;
    if key_bytes.len() != 16 {
        return Err(anyhow!("key must be 16 bytes, got {}", key_bytes.len()));
    }

    let cipher_key = aes::cipher::generic_array::GenericArray::from_slice(&key_bytes);
    let aes = Aes128::new(cipher_key);

    // 随机 IV
    let iv: [u8; 16] = {
        use std::time::{SystemTime, UNIX_EPOCH};
        let mut buf = [0u8; 16];
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let mut state = nanos as u64 ^ 0xDEADBEEF;
        for b in buf.iter_mut() {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            *b = (state >> 33) as u8;
        }
        buf
    };

    let padded = pkcs7_pad(plaintext);
    let mut result = Vec::with_capacity(16 + padded.len());
    result.extend_from_slice(&iv);

    let mut prev_block: [u8; 16] = iv;
    for chunk in padded.chunks(16) {
        let mut block: [u8; 16] = [0; 16];
        for i in 0..16 {
            block[i] = chunk[i] ^ prev_block[i];
        }
        let mut ga = aes::cipher::generic_array::GenericArray::clone_from_slice(&block);
        aes.encrypt_block(&mut ga);
        block.copy_from_slice(&ga);
        result.extend_from_slice(&block);
        prev_block = block;
    }

    Ok(result)
}

/// 解密 registerkey 响应中的 `key` 字段，返回内容解密密钥（前 16 字节 hex，小写）。
///
/// 对应 Java `FqCrypto.getRealKey`。
pub(crate) fn get_real_key(registerkey_response_key: &str) -> Result<String> {
    let raw = base64::engine::general_purpose::STANDARD
        .decode(registerkey_response_key.as_bytes())
        .map_err(|e| anyhow!("base64 decode failed: {e}"))?;
    let decrypted = aes_cbc_decrypt(&raw, REG_KEY)?;
    if decrypted.len() < 16 {
        return Err(anyhow!("decrypted key too short: {} bytes", decrypted.len()));
    }
    Ok(hex::encode(&decrypted[..16]))
}

/// 解密章节内容并解压缩。
///
/// `encrypted_content` = Base64(IV(16) + AES-CBC(gzip(html) 或 html))。
/// `key_hex` = 32 hex chars（来自 `get_real_key`）。
///
/// 对应 Java `FqCrypto.decryptAndDecompressContent`。
pub(crate) fn decrypt_and_decompress(encrypted_content: &str, key_hex: &str) -> Result<String> {
    let raw = base64::engine::general_purpose::STANDARD
        .decode(encrypted_content.as_bytes())
        .map_err(|e| anyhow!("base64 decode failed: {e}"))?;
    let decrypted = aes_cbc_decrypt(&raw, key_hex)?;

    // gzip 魔数 1f 8b
    if decrypted.len() >= 2 && decrypted[0] == 0x1f && decrypted[1] == 0x8b {
        let mut decoder = GzDecoder::new(&decrypted[..]);
        let mut out = String::new();
        decoder.read_to_string(&mut out)?;
        Ok(out)
    } else {
        Ok(String::from_utf8(decrypted)?)
    }
}

/// 构建 registerkey 请求体中的 `content` 字段。
///
/// 对应 Java `FqCrypto.newRegisterKeyContent`：
/// 将 server_device_id 和 "0" 各按小端 8 字节拼接（共 16 字节），
/// 用 REG_KEY 做 AES-128-CBC 加密，输出 Base64(IV + ciphertext)。
pub(crate) fn new_register_key_content(server_device_id: &str) -> Result<String> {
    let device_id: i64 = server_device_id
        .parse()
        .map_err(|e| anyhow!("parse device_id failed: {e}"))?;
    let mut combined = [0u8; 16];
    combined[..8].copy_from_slice(&device_id.to_le_bytes());
    // val = 0，后 8 字节全零

    let encrypted = aes_cbc_encrypt(&combined, REG_KEY)?;
    Ok(base64::engine::general_purpose::STANDARD.encode(&encrypted))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hex_roundtrip() {
        let original = "ac25c67ddd8f38c1b37a2348828e222e";
        let bytes = hex_to_bytes(original).unwrap();
        assert_eq!(bytes.len(), 16);
        let back = hex::encode(&bytes);
        assert_eq!(back, original);
    }

    #[test]
    fn test_reg_key_length() {
        assert_eq!(REG_KEY.len(), 32);
    }

    #[test]
    fn test_pkcs7_roundtrip() {
        let data = b"hello world";
        let padded = pkcs7_pad(data);
        assert_eq!(padded.len() % 16, 0);
        let unpadded = pkcs7_unpad(&padded).unwrap();
        assert_eq!(unpadded, data);
    }

    #[test]
    fn test_aes_cbc_roundtrip() {
        let key = "ac25c67ddd8f38c1b37a2348828e222e";
        let plaintext = b"test plaintext data for aes cbc roundtrip!!";
        let encrypted = aes_cbc_encrypt(plaintext, key).unwrap();
        let decrypted = aes_cbc_decrypt(&encrypted, key).unwrap();
        assert_eq!(decrypted, plaintext);
    }
}
