//! v0.5 placeholder encryption.
//!
//! Uses XOR with a key derived from the master key + nonce.
//! This is NOT production-grade crypto — replace with AES-GCM or age
//! before any real deployment.

use std::fmt;

#[derive(Debug)]
pub struct CryptoError(String);

impl fmt::Display for CryptoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for CryptoError {}

/// Encrypt plaintext with a master key. Returns (ciphertext, nonce).
pub fn encrypt(plaintext: &[u8], master_key: &[u8]) -> Result<(Vec<u8>, Vec<u8>), CryptoError> {
    // Generate 16-byte random nonce
    let nonce: Vec<u8> = (0..16)
        .map(|_| rand_byte())
        .collect();

    let keystream = derive_keystream(master_key, &nonce, plaintext.len());
    let ciphertext: Vec<u8> = plaintext
        .iter()
        .zip(keystream.iter())
        .map(|(p, k)| p ^ k)
        .collect();

    Ok((ciphertext, nonce))
}

/// Decrypt ciphertext with a master key and nonce.
pub fn decrypt(
    ciphertext: &[u8],
    nonce: &[u8],
    master_key: &[u8],
) -> Result<Vec<u8>, CryptoError> {
    let keystream = derive_keystream(master_key, nonce, ciphertext.len());
    let plaintext: Vec<u8> = ciphertext
        .iter()
        .zip(keystream.iter())
        .map(|(c, k)| c ^ k)
        .collect();

    Ok(plaintext)
}

/// Derive a keystream of `len` bytes from master_key and nonce.
/// Uses iterated mixing — NOT cryptographically secure, v0.5 placeholder only.
fn derive_keystream(master_key: &[u8], nonce: &[u8], len: usize) -> Vec<u8> {
    let mut seed: Vec<u8> = Vec::with_capacity(master_key.len() + nonce.len());
    seed.extend_from_slice(master_key);
    seed.extend_from_slice(nonce);

    let mut keystream = Vec::with_capacity(len);
    let mut state: [u8; 32] = [0u8; 32];

    // Initialize state from seed
    for (i, &b) in seed.iter().enumerate() {
        state[i % 32] ^= b;
    }

    // Simple PRNG expansion
    while keystream.len() < len {
        // Mix state
        for i in 0..32 {
            state[i] = state[i]
                .wrapping_add(state[(i + 13) % 32])
                .wrapping_mul(137)
                .wrapping_add(state[(i + 7) % 32]);
        }
        keystream.extend_from_slice(&state);
    }

    keystream.truncate(len);
    keystream
}

/// Simple random byte using thread-local state seeded from system time.
/// NOT cryptographically secure — v0.5 placeholder.
fn rand_byte() -> u8 {
    use std::cell::Cell;
    use std::time::SystemTime;

    thread_local! {
        static STATE: Cell<u64> = Cell::new(
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64
        );
    }

    STATE.with(|s| {
        let mut x = s.get();
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        s.set(x);
        (x & 0xFF) as u8
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let key = b"my-secret-master-key-1234567890!";
        let plaintext = b"hello, this is a secret value";

        let (ciphertext, nonce) = encrypt(plaintext, key).unwrap();
        assert_ne!(&ciphertext, plaintext);

        let recovered = decrypt(&ciphertext, &nonce, key).unwrap();
        assert_eq!(recovered, plaintext);
    }

    #[test]
    fn different_nonces_produce_different_ciphertext() {
        let key = b"key";
        let plaintext = b"same data";

        let (ct1, _) = encrypt(plaintext, key).unwrap();
        let (ct2, _) = encrypt(plaintext, key).unwrap();

        // With random nonces, ciphertexts should differ (extremely high probability)
        assert_ne!(ct1, ct2);
    }

    #[test]
    fn wrong_key_decrypts_to_wrong_value() {
        let key = b"correct-key";
        let wrong_key = b"wrong-key!!";
        let plaintext = b"secret";

        let (ct, nonce) = encrypt(plaintext, key).unwrap();
        let result = decrypt(&ct, &nonce, wrong_key).unwrap();
        assert_ne!(result, plaintext);
    }

    #[test]
    fn empty_plaintext() {
        let key = b"key";
        let (ct, nonce) = encrypt(b"", key).unwrap();
        assert!(ct.is_empty());
        let pt = decrypt(&ct, &nonce, key).unwrap();
        assert!(pt.is_empty());
    }
}
