use anyhow::Context;
use base64::{engine::general_purpose, Engine};
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use rand::rngs::OsRng;
use rand::RngCore;
use sha2::{Digest, Sha256};

pub fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    OsRng.fill_bytes(&mut buf);
    hex::encode(buf)
}

pub fn random_base64(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    OsRng.fill_bytes(&mut buf);
    general_purpose::STANDARD_NO_PAD.encode(buf)
}

pub fn room_id() -> String {
    random_hex(8)
}

pub fn derive_key(master: &str, context: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(master.as_bytes());
    hasher.update(b":");
    hasher.update(context.as_bytes());
    hasher.finalize().into()
}

pub fn encrypt_to_base64(master: &str, context: &str, plaintext: &str) -> anyhow::Result<String> {
    let (ciphertext, nonce) = encrypt_parts(master, context, plaintext)?;
    Ok(format!("{}:{}", nonce, ciphertext))
}

pub fn encrypt_parts(
    master: &str,
    context: &str,
    plaintext: &str,
) -> anyhow::Result<(String, String)> {
    let key = derive_key(master, context);
    let cipher = ChaCha20Poly1305::new(Key::from_slice(&key));
    let mut nonce = [0u8; 12];
    OsRng.fill_bytes(&mut nonce);
    let ciphertext = cipher
        .encrypt(Nonce::from_slice(&nonce), plaintext.as_bytes())
        .map_err(|_| anyhow::anyhow!("encrypt failed"))?;
    Ok((
        general_purpose::STANDARD.encode(ciphertext),
        general_purpose::STANDARD.encode(nonce),
    ))
}

pub fn decrypt_from_base64(master: &str, context: &str, value: &str) -> anyhow::Result<String> {
    if let Some((nonce, ciphertext)) = value.split_once(':') {
        return decrypt_parts(master, context, ciphertext, nonce);
    }
    anyhow::bail!("invalid encrypted value format");
}

pub fn decrypt_parts(
    master: &str,
    context: &str,
    ciphertext: &str,
    nonce: &str,
) -> anyhow::Result<String> {
    let ciphertext = general_purpose::STANDARD
        .decode(ciphertext)
        .context("invalid ciphertext base64")?;
    let nonce = general_purpose::STANDARD
        .decode(nonce)
        .context("invalid nonce base64")?;
    if nonce.len() != 12 {
        anyhow::bail!("invalid nonce length");
    }
    let key = derive_key(master, context);
    let cipher = ChaCha20Poly1305::new(Key::from_slice(&key));
    let plaintext = cipher
        .decrypt(Nonce::from_slice(&nonce), ciphertext.as_ref())
        .map_err(|_| anyhow::anyhow!("decrypt failed"))?;
    Ok(String::from_utf8(plaintext)?)
}

pub fn master_key() -> String {
    std::env::var("CONSOLE_MASTER_KEY").unwrap_or_else(|_| "vnt-hub-default-dev-master-key".into())
}

pub fn device_push_key_material(device_id: &str, device_token: &str) -> String {
    format!("{}:{}", device_id, device_token)
}

pub fn device_push_context(device_id: &str, version: u32) -> String {
    format!("push:{}:{}", device_id, version)
}
