use std::fmt;

use aes_gcm::aead::{Aead, KeyInit, OsRng};
use aes_gcm::{Aes256Gcm, Nonce};
use rand::RngCore;

#[derive(Debug)]
pub struct CryptoError(String);

impl fmt::Display for CryptoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for CryptoError {}

/// Derive a 32-byte key from raw bytes. If already 32 bytes, used as-is.
/// Otherwise SHA-256-style mixing to produce 32 bytes.
fn derive_key(raw: &[u8]) -> [u8; 32] {
    if raw.len() == 32 {
        let mut key = [0u8; 32];
        key.copy_from_slice(raw);
        return key;
    }
    // Simple key derivation: iterative mixing to 32 bytes.
    // For production, consider a proper KDF like HKDF, but this matches the plan spec.
    let mut key = [0u8; 32];
    for (i, &b) in raw.iter().enumerate() {
        key[i % 32] ^= b;
    }
    // Additional mixing pass to distribute entropy
    for i in 0..32 {
        key[i] = key[i]
            .wrapping_add(key[(i + 13) % 32])
            .wrapping_mul(137)
            .wrapping_add(key[(i + 7) % 32]);
    }
    key
}

/// Encrypt plaintext with a master key. Returns (ciphertext_with_tag, nonce).
pub fn encrypt(plaintext: &[u8], master_key: &[u8]) -> Result<(Vec<u8>, Vec<u8>), CryptoError> {
    let key = derive_key(master_key);
    let cipher = Aes256Gcm::new_from_slice(&key)
        .map_err(|e| CryptoError(format!("cipher init: {e}")))?;

    let mut nonce_bytes = [0u8; 12];
    OsRng.fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);

    let ciphertext = cipher
        .encrypt(nonce, plaintext)
        .map_err(|e| CryptoError(format!("encryption failed: {e}")))?;

    Ok((ciphertext, nonce_bytes.to_vec()))
}

/// Decrypt ciphertext with a master key and nonce.
pub fn decrypt(ciphertext: &[u8], nonce: &[u8], master_key: &[u8]) -> Result<Vec<u8>, CryptoError> {
    let key = derive_key(master_key);
    let cipher = Aes256Gcm::new_from_slice(&key)
        .map_err(|e| CryptoError(format!("cipher init: {e}")))?;

    let nonce = Nonce::from_slice(nonce);

    cipher
        .decrypt(nonce, ciphertext)
        .map_err(|e| CryptoError(format!("decryption failed: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let key = b"my-secret-master-key-1234567890!";
        let plaintext = b"hello, this is a secret value";

        let (ciphertext, nonce) = encrypt(plaintext, key).unwrap();
        assert_ne!(&ciphertext[..plaintext.len()], plaintext);

        let recovered = decrypt(&ciphertext, &nonce, key).unwrap();
        assert_eq!(recovered, plaintext);
    }

    #[test]
    fn different_nonces_produce_different_ciphertext() {
        let key = b"key-that-is-long-enough-for-test";
        let plaintext = b"same data";

        let (ct1, _) = encrypt(plaintext, key).unwrap();
        let (ct2, _) = encrypt(plaintext, key).unwrap();

        // With random nonces, ciphertexts should differ (extremely high probability)
        assert_ne!(ct1, ct2);
    }

    #[test]
    fn wrong_key_fails_with_error() {
        let key = b"correct-key-for-aes-gcm-testing!";
        let wrong_key = b"wrong-key-for-aes-gcm-testing!!";
        let plaintext = b"secret";

        let (ct, nonce) = encrypt(plaintext, key).unwrap();
        let result = decrypt(&ct, &nonce, wrong_key);
        assert!(result.is_err(), "wrong key should produce an error");
    }

    #[test]
    fn empty_plaintext() {
        let key = b"key-for-empty-test-32-bytes-long";
        let (ct, nonce) = encrypt(b"", key).unwrap();
        // AES-GCM produces a 16-byte auth tag even for empty plaintext
        assert_eq!(ct.len(), 16);
        let pt = decrypt(&ct, &nonce, key).unwrap();
        assert!(pt.is_empty());
    }

    #[test]
    fn derive_key_32_bytes_passthrough() {
        let raw = [42u8; 32];
        let key = derive_key(&raw);
        assert_eq!(key, raw);
    }

    #[test]
    fn derive_key_shorter_input() {
        let raw = b"short";
        let key = derive_key(raw);
        assert_eq!(key.len(), 32);
        // Should be deterministic
        assert_eq!(key, derive_key(raw));
    }
}
