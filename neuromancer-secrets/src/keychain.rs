use keyring::Entry;
use secrecy::SecretString;

use neuromancer_core::error::{InfraError, NeuromancerError};

/// Get or create a master encryption key in the macOS Keychain.
///
/// Uses the `keyring` crate for platform-native credential storage.
/// If the entry already exists, returns the stored key. Otherwise,
/// generates 32 random bytes, base64-encodes them, stores in keychain,
/// and returns the encoded key.
pub fn get_or_create_master_key(service: &str) -> Result<SecretString, NeuromancerError> {
    let entry = Entry::new(service, "neuromancer-master-key").map_err(|e| {
        NeuromancerError::Infra(InfraError::Database(format!("keyring entry: {e}")))
    })?;

    match entry.get_password() {
        Ok(password) => Ok(SecretString::from(password)),
        Err(keyring::Error::NoEntry) => {
            use base64::Engine;
            use rand::RngCore;

            let mut key_bytes = [0u8; 32];
            rand::thread_rng().fill_bytes(&mut key_bytes);
            let encoded = base64::engine::general_purpose::STANDARD.encode(key_bytes);

            entry.set_password(&encoded).map_err(|e| {
                NeuromancerError::Infra(InfraError::Database(format!("keyring set: {e}")))
            })?;

            Ok(SecretString::from(encoded))
        }
        Err(e) => Err(NeuromancerError::Infra(InfraError::Database(format!(
            "keyring get: {e}"
        )))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore] // Requires macOS Keychain access — run manually
    fn create_read_delete_master_key() {
        let service = "neuromancer-test-keychain";

        // Create
        let key1 = get_or_create_master_key(service).unwrap();

        // Read back — should be same value
        let key2 = get_or_create_master_key(service).unwrap();
        assert_eq!(
            secrecy::ExposeSecret::expose_secret(&key1),
            secrecy::ExposeSecret::expose_secret(&key2),
        );

        // Cleanup
        let entry = Entry::new(service, "neuromancer-master-key").unwrap();
        entry.delete_credential().unwrap();
    }
}
