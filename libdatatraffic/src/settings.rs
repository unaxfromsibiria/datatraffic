//! Runtime settings resolved from explicit values, environment variables, and defaults.

use std::env;

const ENV_REDIS_URL: &str = "REDIS_URL";
const ENV_CRYPTO_KEY: &str = "CRYPTO_KEY";
const ENV_CLIENT_NAME: &str = "CLIENT_NAME";
const ENV_REDIS_PASSWORD: &str = "REDIS_PASSWORD";

/// Resolved runtime configuration for a traffic client instance.
#[derive(Debug, Clone)]
pub struct Settings {
    pub redis_url: String,
    pub key: [u8; 32],
    pub client_name: String,
    /// Optional Redis password; `None` means no authentication.
    pub password: Option<String>,
}

impl Settings {
    /// Resolve settings with priority: explicit > env > default (client_name only).
    pub fn resolve(
        redis_url: Option<&str>,
        key_hex: Option<&str>,
        client_name: Option<&str>,
        password: Option<&str>,
    ) -> Result<Settings, String> {
        let redis_url = first_non_empty(redis_url).or_else(
            || env::var(ENV_REDIS_URL).ok()
        ).filter(|s| !s.is_empty()).ok_or_else(
            || format!("{} is not set (env) and no explicit value given", ENV_REDIS_URL)
        )?;

        let key_hex = first_non_empty(key_hex).or_else(
            || env::var(ENV_CRYPTO_KEY).ok()
        ).filter(|s| !s.is_empty()).ok_or_else(
            || format!("{} is not set (env) and no explicit value given", ENV_CRYPTO_KEY)
        )?;
        let key = super::config::decode_key(&key_hex)?;

        let client_name = first_non_empty(client_name).or_else(
            || env::var(ENV_CLIENT_NAME).ok()
        ).filter(|s| !s.is_empty()).unwrap_or_else(generate_client_name);

        // Optional: an empty/absent password means no authentication.
        let password = first_non_empty(password).or_else(
            || env::var(ENV_REDIS_PASSWORD).ok()
        ).filter(|s| !s.is_empty());

        Ok(Settings { redis_url, key, client_name, password })
    }
}

/// Inject `:<pass>@` right after the scheme when the URL has no credentials yet; otherwise unchanged.
/// The leading colon matters: redis-rs parses a bare `pass@host` as a username, not a password.
pub fn authed_url(url: &str, password: Option<&str>) -> String {
    let Some(pass) = password.map(str::trim).filter(|s| !s.is_empty()) else {
        return url.to_string();
    };
    if url.contains('@') {
        return url.to_string();
    }
    match url.find("://") {
        Some(idx) => format!("{}:{}@{}", &url[..idx + 3], pass, &url[idx + 3..]),
        None => url.to_string(),
    }
}

/// Generate a random 8-character hex string (4 bytes) as a client name.
pub fn generate_client_name() -> String {
    use rand::Rng;
    let mut buf = [0u8; 4];
    rand::rng().fill_bytes(&mut buf);
    hex::encode(buf)
}

fn first_non_empty(value: Option<&str>) -> Option<String> {
    value.map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)
}

#[cfg(test)]
mod tests {
    use std::sync::{Mutex, MutexGuard};

    use super::*;

    // Env-mutating tests must run serially: test threads share the process environment.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn lock_env() -> MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn generate_client_name_is_8_hex_chars() {
        let name = generate_client_name();
        assert_eq!(name.len(), 8);
        assert!(name.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn generate_client_name_is_random() {
        assert_ne!(generate_client_name(), generate_client_name());
    }

    #[test]
    fn resolve_explicit_overrides_env() {
        let _env = lock_env();
        unsafe {
            std::env::set_var(ENV_REDIS_URL, "redis://from-env");
            std::env::set_var(ENV_CRYPTO_KEY, &"a".repeat(64));
            std::env::set_var(ENV_CLIENT_NAME, "env-client");
        }

        let s = Settings::resolve(
            Some("redis://explicit"),
            Some(&"b".repeat(64)),
            Some("my-client"),
            Some("pw-explicit"),
        ).unwrap();
        assert_eq!(s.redis_url, "redis://explicit");
        assert_eq!(s.key, [0xbb; 32]);
        assert_eq!(s.client_name, "my-client");
        assert_eq!(s.password.as_deref(), Some("pw-explicit"));

        unsafe {
            std::env::remove_var(ENV_REDIS_URL);
            std::env::remove_var(ENV_CRYPTO_KEY);
            std::env::remove_var(ENV_CLIENT_NAME);
            std::env::remove_var(ENV_REDIS_PASSWORD);
        }
    }

    #[test]
    fn resolve_falls_back_to_env() {
        let _env = lock_env();
        unsafe {
            std::env::set_var(ENV_REDIS_URL, "redis://from-env");
            std::env::set_var(ENV_CRYPTO_KEY, &"c".repeat(64));
            std::env::remove_var(ENV_CLIENT_NAME);
        }

        let s = Settings::resolve(None, None, None, Some("pw-cli")).unwrap();
        assert_eq!(s.redis_url, "redis://from-env");
        assert_eq!(s.key, [0xcc; 32]);
        // client_name should be auto-generated (8 hex chars)
        assert_eq!(s.client_name.len(), 8);
        assert_eq!(s.password.as_deref(), Some("pw-cli"));

        unsafe {
            std::env::remove_var(ENV_REDIS_URL);
            std::env::remove_var(ENV_CRYPTO_KEY);
            std::env::remove_var(ENV_REDIS_PASSWORD);
        }
    }

    #[test]
    fn resolve_password_falls_back_to_env() {
        let _env = lock_env();
        unsafe {
            std::env::set_var(ENV_REDIS_URL, "redis://from-env");
            std::env::set_var(ENV_CRYPTO_KEY, &"c".repeat(64));
            std::env::set_var(ENV_REDIS_PASSWORD, "pw-from-env");
        }

        let s = Settings::resolve(None, None, None, None).unwrap();
        assert_eq!(s.password.as_deref(), Some("pw-from-env"));

        unsafe {
            std::env::remove_var(ENV_REDIS_URL);
            std::env::remove_var(ENV_CRYPTO_KEY);
            std::env::remove_var(ENV_REDIS_PASSWORD);
        }
    }

    #[test]
    fn resolve_password_none_when_absent() {
        let _env = lock_env();
        unsafe {
            std::env::set_var(ENV_REDIS_URL, "redis://from-env");
            std::env::set_var(ENV_CRYPTO_KEY, &"c".repeat(64));
            std::env::remove_var(ENV_REDIS_PASSWORD);
        }

        let s = Settings::resolve(None, None, None, None).unwrap();
        assert_eq!(s.password, None);

        unsafe {
            std::env::remove_var(ENV_REDIS_URL);
            std::env::remove_var(ENV_CRYPTO_KEY);
        }
    }

    #[test]
    fn resolve_missing_redis_url_is_error() {
        let _env = lock_env();
        unsafe { std::env::remove_var(ENV_REDIS_URL); }
        let err = Settings::resolve(None, Some(&"a".repeat(64)), None, None).unwrap_err();
        assert!(err.contains("REDIS_URL"));
    }

    #[test]
    fn resolve_missing_key_is_error() {
        let _env = lock_env();
        unsafe {
            std::env::set_var(ENV_REDIS_URL, "redis://localhost");
            std::env::remove_var(ENV_CRYPTO_KEY);
        }
        let err = Settings::resolve(None, None, None, None).unwrap_err();
        assert!(err.contains("CRYPTO_KEY"));
        unsafe { std::env::remove_var(ENV_REDIS_URL); }
    }

    #[test]
    fn resolve_bad_key_length_is_error() {
        let _env = lock_env();
        unsafe { std::env::set_var(ENV_REDIS_URL, "redis://localhost"); }
        let err = Settings::resolve(None, Some("abcd"), None, None).unwrap_err();
        assert!(err.contains("32 bytes"));
        unsafe { std::env::remove_var(ENV_REDIS_URL); }
    }

    // --- authed_url ----------------------------------------------------------

    #[test]
    fn authed_url_no_password_unchanged() {
        let url = "redis://localhost:6379";
        assert_eq!(authed_url(url, None), url);
        assert_eq!(authed_url(url, Some("")), url);
        assert_eq!(authed_url(url, Some("   ")), url);
    }

    #[test]
    fn authed_url_inserts_password() {
        assert_eq!(
            authed_url("redis://localhost:6379", Some("secret")),
            "redis://:secret@localhost:6379"
        );
    }

    #[test]
    fn authed_url_preserves_db_suffix() {
        assert_eq!(
            authed_url("redis://localhost:6379/2", Some("pw")),
            "redis://:pw@localhost:6379/2"
        );
    }

    #[test]
    fn authed_url_keeps_existing_credentials() {
        let url = "redis://user:oldpass@host:6380";
        assert_eq!(authed_url(url, Some("newpass")), url);
    }

    #[test]
    fn authed_url_handles_scheme_variants() {
        assert_eq!(
            authed_url("rediss://example.com", Some("pw")),
            "rediss://:pw@example.com"
        );
    }
}
