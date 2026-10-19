//! Runtime configuration resolved from environment variables.

use std::env;

/// Redis connection URL and the AES-256-GCM key used to encrypt payloads.
#[derive(Debug, Clone)]
pub struct Config {
    /// Redis connection string (see `REDIS_URL`).
    pub redis_url: String,
    /// 32-byte AES-256-GCM cipher key (hex in the environment).
    pub key: [u8; 32],
}

const ENV_REDIS_URL: &str = "REDIS_URL";
const ENV_CRYPTO_KEY: &str = "CRYPTO_KEY";

impl Config {
    /// Build a `Config` from the `REDIS_URL` and `CRYPTO_KEY` environment variables.
    pub fn from_env() -> Result<Config, String> {
        let redis_url =
            env::var(ENV_REDIS_URL).map_err(|_| format!("{} is not set", ENV_REDIS_URL))?;
        let key_hex =
            env::var(ENV_CRYPTO_KEY).map_err(|_| format!("{} is not set", ENV_CRYPTO_KEY))?;
        let key = decode_key(&key_hex)?;
        Ok(Config { redis_url, key })
    }
}

/// Decode a hex string into a 32-byte AES-256-GCM key.
pub fn decode_key(hex_str: &str) -> Result<[u8; 32], String> {
    let bytes =
        hex::decode(hex_str.trim()).map_err(|e| format!("invalid hex in CRYPTO_KEY: {}", e))?;
    if bytes.len() != 32 {
        return Err(format!(
            "CRYPTO_KEY must be 32 bytes (64 hex chars), got {} bytes",
            bytes.len()
        ));
    }
    let mut key = [0u8; 32];
    key.copy_from_slice(&bytes);
    Ok(key)
}

/// Generate a random 32-byte AES-256-GCM key as a lowercase hex string.
pub fn generate_key() -> String {
    use rand::Rng;
    let mut key = [0u8; 32];
    rand::rng().fill_bytes(&mut key);
    hex::encode(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_key_valid() {
        assert_eq!(decode_key(&"a".repeat(64)).unwrap(), [0xaa; 32]);
    }

    #[test]
    fn decode_key_wrong_length_is_error() {
        assert!(decode_key("abcd").is_err());
    }

    #[test]
    fn decode_key_bad_hex_is_error() {
        assert!(decode_key(&"zz".repeat(32)).is_err());
    }

    #[test]
    fn generate_key_is_64_hex_chars_decoding_to_32_bytes() {
        let key = generate_key();
        assert_eq!(key.len(), 64);
        assert_eq!(hex::decode(&key).unwrap().len(), 32);
    }

    #[test]
    fn generate_key_is_random() {
        assert_ne!(generate_key(), generate_key());
    }
}
