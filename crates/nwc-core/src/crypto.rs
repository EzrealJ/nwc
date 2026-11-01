//! scrypt 密钥派生与 AES-256-GCM 帧加解密（对应 Node 版 src/crypto.js）。

use aes_gcm::aead::{Aead, AeadInPlace, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine;

pub const KEY_LEN: usize = 32;
pub const IV_LEN: usize = 12;
pub const TAG_LEN: usize = 16;
/// 加密帧头部长度：[4B 长度][12B IV][16B tag]
pub const FRAME_HDR: usize = 4 + IV_LEN + TAG_LEN;

/// key = scrypt(token, saltC||saltS, N=16384, r=8, p=1, dkLen=32)
/// 两端盐拼接顺序一致：客户端盐在前。对应 Node 版 deriveKey。
pub fn derive_key(token: &str, salt_c: &[u8], salt_s: &[u8]) -> [u8; KEY_LEN] {
    let params = scrypt::Params::new(14, 8, 1, KEY_LEN).expect("scrypt params");
    let mut salt = Vec::with_capacity(salt_c.len() + salt_s.len());
    salt.extend_from_slice(salt_c);
    salt.extend_from_slice(salt_s);
    let mut out = [0u8; KEY_LEN];
    scrypt::scrypt(token.as_bytes(), &salt, &params, &mut out).expect("scrypt");
    out
}

pub fn b64(data: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(data)
}

pub fn unb64(s: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::STANDARD.decode(s).ok()
}

/// 帧格式：`[4B BE 密文长度][12B 随机 IV][16B tag][密文]`
/// 长度前缀只算密文；GCM 无 AAD。对应 Node 版 encryptFrame。
pub fn encrypt_frame(key: &[u8; KEY_LEN], plain: &[u8]) -> Vec<u8> {
    let mut iv = [0u8; IV_LEN];
    getrandom::getrandom(&mut iv).expect("getrandom");
    let cipher = Aes256Gcm::new(key.into());
    let sealed = cipher
        .encrypt(Nonce::from_slice(&iv), Payload::from(plain))
        .expect("aes-gcm encrypt");
    let ct_len = sealed.len() - TAG_LEN;
    let mut out = Vec::with_capacity(FRAME_HDR + ct_len);
    out.extend_from_slice(&(ct_len as u32).to_be_bytes());
    out.extend_from_slice(&iv);
    out.extend_from_slice(&sealed[ct_len..]); // tag
    out.extend_from_slice(&sealed[..ct_len]); // ct
    out
}

/// 就地加密并直接写出（大文件流式发送用，避免再拷贝一份完整密文）。
/// plain 在调用后变成 ct||tag 布局，调用方可继续复用其容量。
pub fn encrypt_frame_in_place(
    key: &[u8; KEY_LEN],
    plain: &mut Vec<u8>,
    out: &mut dyn std::io::Write,
) -> std::io::Result<()> {
    let mut iv = [0u8; IV_LEN];
    getrandom::getrandom(&mut iv).expect("getrandom");
    let cipher = Aes256Gcm::new(key.into());
    let ct_len = plain.len();
    out.write_all(&(ct_len as u32).to_be_bytes())?;
    out.write_all(&iv)?;
    cipher
        .encrypt_in_place(Nonce::from_slice(&iv), b"", plain)
        .expect("aes-gcm encrypt");
    // plain 现在是 ct||tag
    out.write_all(&plain[ct_len..])?; // tag 在前
    out.write_all(&plain[..ct_len])?; // ct 在后
    out.flush()
}

/// 对应 Node 版 decryptFrame。tag 校验失败返回 None（调用方应断连）。
pub fn decrypt_frame(key: &[u8; KEY_LEN], iv: &[u8], tag: &[u8], ct: &[u8]) -> Option<Vec<u8>> {
    let cipher = Aes256Gcm::new(key.into());
    let mut buf = Vec::with_capacity(ct.len() + TAG_LEN);
    buf.extend_from_slice(ct);
    buf.extend_from_slice(tag);
    cipher.decrypt(Nonce::from_slice(iv), Payload::from(&buf[..])).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_roundtrip() {
        let key = derive_key("口令", b"CCCCCCCCCCCCCCCC", b"SSSSSSSSSSSSSSSS");
        let frame = encrypt_frame(&key, b"hello world");
        assert_eq!(frame.len(), FRAME_HDR + 11);
        let len = u32::from_be_bytes(frame[0..4].try_into().unwrap()) as usize;
        assert_eq!(len, 11);
        let iv = &frame[4..16];
        let tag = &frame[16..32];
        let ct = &frame[32..];
        assert_eq!(decrypt_frame(&key, iv, tag, ct).unwrap(), b"hello world");
    }

    #[test]
    fn tampered_tag_fails() {
        let key = derive_key("t", b"c".repeat(16).as_slice(), b"s".repeat(16).as_slice());
        let mut frame = encrypt_frame(&key, b"secret");
        let last = frame.len() - 1;
        frame[last] ^= 0xFF;
        let iv = &frame[4..16];
        let tag = &frame[16..32];
        let ct = &frame[32..];
        assert!(decrypt_frame(&key, iv, tag, ct).is_none());
    }

    #[test]
    fn in_place_matches_one_shot() {
        let key = derive_key("k", &[0u8; 16], &[1u8; 16]);
        let msg = b"payload-1234567890";
        let mut plain = msg.to_vec();
        let mut sink = Vec::new();
        encrypt_frame_in_place(&key, &mut plain, &mut sink).unwrap();
        let direct = encrypt_frame(&key, msg);
        // 同一密钥下两帧 IV 不同，验证长度一致且都能解回原文
        assert_eq!(sink.len(), direct.len());
        let len = u32::from_be_bytes(sink[0..4].try_into().unwrap()) as usize;
        assert_eq!(
            decrypt_frame(&key, &sink[4..16], &sink[16..32], &sink[32..]).unwrap(),
            msg
        );
        assert_eq!(len, msg.len());
    }
}
