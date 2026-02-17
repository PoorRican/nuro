use std::sync::Arc;

use aho_corasick::AhoCorasick;
use arc_swap::ArcSwap;
use base64::Engine;

/// Multi-pattern secret scanner for exfiltration prevention.
///
/// Uses Aho-Corasick for efficient simultaneous matching of all known
/// secret values (raw, base64-encoded, and URL-encoded variants).
pub struct SecretScanner {
    automaton: AhoCorasick,
    /// Maps pattern index → secret_id.
    pattern_to_secret: Vec<String>,
    /// Maps pattern index → replacement string `[REDACTED:<id>]`.
    replacement_strings: Vec<String>,
}

impl SecretScanner {
    /// Build scanner from secret (id, value) pairs.
    /// For each value >= 8 chars: adds raw, base64-encoded, and URL-encoded patterns.
    pub fn build(secrets: &[(String, String)]) -> Self {
        let mut patterns: Vec<String> = Vec::new();
        let mut pattern_to_secret: Vec<String> = Vec::new();
        let mut replacement_strings: Vec<String> = Vec::new();

        for (id, value) in secrets {
            if value.len() < 8 {
                continue;
            }

            let redacted = format!("[REDACTED:{id}]");

            // Raw pattern
            patterns.push(value.clone());
            pattern_to_secret.push(id.clone());
            replacement_strings.push(redacted.clone());

            // Base64-encoded pattern
            let b64 = base64::engine::general_purpose::STANDARD.encode(value.as_bytes());
            patterns.push(b64);
            pattern_to_secret.push(id.clone());
            replacement_strings.push(redacted.clone());

            // URL-encoded pattern
            let url_encoded = urlencoding::encode(value).into_owned();
            if url_encoded != *value {
                patterns.push(url_encoded);
                pattern_to_secret.push(id.clone());
                replacement_strings.push(redacted);
            }
        }

        let automaton = AhoCorasick::new(&patterns).unwrap();

        Self {
            automaton,
            pattern_to_secret,
            replacement_strings,
        }
    }

    /// Scan text for leaked secrets. Returns deduplicated matched secret IDs.
    pub fn scan(&self, text: &str) -> Vec<String> {
        let mut found = Vec::new();
        for mat in self.automaton.find_iter(text) {
            let id = &self.pattern_to_secret[mat.pattern().as_usize()];
            if !found.contains(id) {
                found.push(id.clone());
            }
        }
        found
    }

    /// Redact all matched secrets, replacing with `[REDACTED:<secret_id>]`.
    pub fn redact(&self, text: &str) -> String {
        self.automaton.replace_all(text, &self.replacement_strings)
    }

    /// Empty scanner that matches nothing.
    pub fn empty() -> Self {
        Self {
            automaton: AhoCorasick::new([""; 0]).unwrap(),
            pattern_to_secret: Vec::new(),
            replacement_strings: Vec::new(),
        }
    }
}

/// Thread-safe, atomically swappable scanner handle.
pub type SharedScanner = Arc<ArcSwap<SecretScanner>>;

/// Create a new shared scanner initialized to empty (matches nothing).
pub fn new_shared_scanner() -> SharedScanner {
    Arc::new(ArcSwap::from_pointee(SecretScanner::empty()))
}

/// Rebuild the scanner atomically with new secret values.
pub fn rebuild_scanner(scanner: &SharedScanner, secrets: &[(String, String)]) {
    scanner.store(Arc::new(SecretScanner::build(secrets)));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_detects_raw_secret() {
        let secrets = vec![(
            "api_key".to_string(),
            "my-super-secret-token-12345".to_string(),
        )];
        let scanner = SecretScanner::build(&secrets);

        let found = scanner.scan("The token is my-super-secret-token-12345 in the response");
        assert_eq!(found, vec!["api_key"]);
    }

    #[test]
    fn scan_detects_base64_encoded() {
        let value = "my-super-secret-token-12345";
        let b64 = base64::engine::general_purpose::STANDARD.encode(value.as_bytes());
        let secrets = vec![("api_key".to_string(), value.to_string())];
        let scanner = SecretScanner::build(&secrets);

        let found = scanner.scan(&format!("encoded: {b64}"));
        assert_eq!(found, vec!["api_key"]);
    }

    #[test]
    fn scan_detects_url_encoded() {
        let value = "secret with spaces&chars";
        let url_enc = urlencoding::encode(value);
        let secrets = vec![("api_key".to_string(), value.to_string())];
        let scanner = SecretScanner::build(&secrets);

        let found = scanner.scan(&format!("param={url_enc}"));
        assert_eq!(found, vec!["api_key"]);
    }

    #[test]
    fn scan_no_false_positives() {
        let secrets = vec![("api_key".to_string(), "unique-secret-value-xyz".to_string())];
        let scanner = SecretScanner::build(&secrets);

        let found = scanner.scan("this text contains no secrets at all");
        assert!(found.is_empty());
    }

    #[test]
    fn short_secrets_excluded() {
        let secrets = vec![("short".to_string(), "abc".to_string())];
        let scanner = SecretScanner::build(&secrets);

        let found = scanner.scan("contains abc in text");
        assert!(found.is_empty());
    }

    #[test]
    fn redact_replaces_with_marker() {
        let secrets = vec![(
            "db_pass".to_string(),
            "super-secret-password-123".to_string(),
        )];
        let scanner = SecretScanner::build(&secrets);

        let redacted = scanner.redact("connection: user:super-secret-password-123@host");
        assert_eq!(redacted, "connection: user:[REDACTED:db_pass]@host");
        assert!(!redacted.contains("super-secret-password-123"));
    }

    #[test]
    fn empty_scanner_matches_nothing() {
        let scanner = SecretScanner::empty();
        assert!(scanner.scan("any text").is_empty());
        assert_eq!(scanner.redact("any text"), "any text");
    }

    #[test]
    fn shared_scanner_rebuild() {
        let shared = new_shared_scanner();

        // Initially empty
        assert!(shared.load().scan("my-super-secret-token-12345").is_empty());

        // After rebuild
        let secrets = vec![("tok".to_string(), "my-super-secret-token-12345".to_string())];
        rebuild_scanner(&shared, &secrets);
        assert_eq!(
            shared.load().scan("has my-super-secret-token-12345"),
            vec!["tok"]
        );
    }

    #[test]
    fn scan_deduplicates_matches() {
        let secrets = vec![(
            "api_key".to_string(),
            "repeated-secret-value-xyz".to_string(),
        )];
        let scanner = SecretScanner::build(&secrets);

        let found = scanner.scan("repeated-secret-value-xyz and repeated-secret-value-xyz again");
        assert_eq!(found, vec!["api_key"]);
    }
}
