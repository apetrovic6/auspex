//! Encrypted cookies for the BFF.
//!
//! Two cookies, both AES-256-GCM with a random 12-byte nonce per write:
//!
//! - **Flow cookie** `__Host-login-flow`: `State` + PKCE verifier. `SameSite=Lax`,
//!   `Path=/`, `Max-Age=600`, encrypted. The `__Host-` prefix requires `Secure` and no
//!   `Domain` (RFC 6265bis §5.5.8).
//! - **Session cookie** `__Host-session`: the refresh token. `SameSite=Strict`, `Path=/`,
//!   encrypted. No max-age, so it lasts until the browser closes or logout clears it.
//!
//! # Key
//!
//! `auth.cookie_key` is standard padded base64 encoding of 32 random bytes.
//! Generate it with `openssl rand -base64 32`. Missing or invalid keys are rejected
//! as `settings::Error::Invalid` at configuration load.

use std::fmt;

use aws_lc_rs::aead::{AES_256_GCM, Aad, LessSafeKey, Nonce, UnboundKey};
use base64::Engine;
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};

/// 32 bytes for AES-256-GCM.
const KEY_LEN: usize = 32;
/// 12 bytes for AES-GCM nonce.
const NONCE_LEN: usize = 12;

/// Secret configuration value containing a standard padded base64 cookie key.
#[derive(Clone, Default, Deserialize)]
#[serde(transparent)]
pub struct CookieKeySetting(String);

impl CookieKeySetting {
    /// Decode a configured AES-256-GCM key.
    ///
    /// # Errors
    ///
    /// Rejects missing values, noncanonical standard padded base64, and decoded
    /// values that are not exactly 32 bytes. Errors never include the secret.
    pub fn decode(&self) -> Result<CookieKey, KeyError> {
        if self.0.is_empty() {
            return Err(KeyError::Missing);
        }
        let mut bytes = [0_u8; KEY_LEN + 1];
        let len = base64::engine::general_purpose::STANDARD
            .decode_slice(self.0.as_bytes(), &mut bytes)
            .map_err(|_| KeyError::InvalidEncoding)?;
        if len != KEY_LEN {
            return Err(KeyError::WrongLength);
        }
        let mut key = [0_u8; KEY_LEN];
        key.copy_from_slice(&bytes[..KEY_LEN]);
        Ok(CookieKey(key))
    }
}

impl fmt::Debug for CookieKeySetting {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CookieKeySetting([REDACTED])")
    }
}

impl fmt::Display for CookieKeySetting {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}

/// A 32-byte cookie encryption key.
#[derive(Clone)]
pub struct CookieKey([u8; KEY_LEN]);

impl CookieKey {
    /// For tests: a fixed key derived from a seed byte.
    #[cfg(feature = "testing")]
    #[must_use]
    pub fn fixed(seed: u8) -> Self {
        let mut key = [seed; KEY_LEN];
        // Derive a non-trivial key from the seed.
        for (factor, byte) in (1_u8..).zip(key.iter_mut()) {
            *byte = seed.wrapping_mul(factor);
        }
        Self(key)
    }

    fn seal(&self, plaintext: &[u8]) -> Vec<u8> {
        let cipher = LessSafeKey::new(
            UnboundKey::new(&AES_256_GCM, &self.0).expect("a 32-byte key is valid"),
        );
        let mut nonce = [0_u8; NONCE_LEN];
        OsRng.fill_bytes(&mut nonce);
        let mut output = Vec::with_capacity(NONCE_LEN + plaintext.len() + 16);
        output.extend_from_slice(&nonce);
        output.extend_from_slice(plaintext);
        let tag = cipher
            .seal_in_place_separate_tag(
                Nonce::assume_unique_for_key(nonce),
                Aad::empty(),
                &mut output[NONCE_LEN..],
            )
            .expect("AES-256-GCM encryption is infallible");
        output.extend_from_slice(tag.as_ref());
        output
    }

    fn open(&self, sealed: &[u8]) -> Result<Vec<u8>, OpenError> {
        if sealed.len() < NONCE_LEN + 16 {
            // 16 is the GCM tag size. Anything shorter is not a valid AEAD ciphertext.
            return Err(OpenError::TooShort);
        }
        let (nonce, ciphertext) = sealed.split_at(NONCE_LEN);
        let cipher = LessSafeKey::new(
            UnboundKey::new(&AES_256_GCM, &self.0).expect("a 32-byte key is valid"),
        );
        let mut plaintext = ciphertext.to_vec();
        let opened = cipher
            .open_in_place(
                Nonce::try_assume_unique_for_key(nonce).map_err(|_| OpenError::Decrypt)?,
                Aad::empty(),
                &mut plaintext,
            )
            .map_err(|_| OpenError::Decrypt)?;
        let len = opened.len();
        plaintext.truncate(len);
        Ok(plaintext)
    }
}

impl fmt::Debug for CookieKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CookieKey([REDACTED])")
    }
}

impl fmt::Display for CookieKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}

/// Why the configured cookie key cannot be decoded.
#[derive(Debug, thiserror::Error)]
pub enum KeyError {
    #[error("missing cookie key; generate one with `openssl rand -base64 32`")]
    Missing,
    #[error("cookie key must be standard padded base64 encoding of exactly 32 bytes")]
    InvalidEncoding,
    #[error("cookie key must decode to exactly 32 bytes")]
    WrongLength,
}

/// Why a sealed cookie cannot be opened.
#[derive(Debug)]
pub(crate) enum OpenError {
    TooShort,
    Decrypt,
}

/// A cookie is malformed, cannot be authenticated, or contains an invalid value.
#[derive(Debug, thiserror::Error)]
#[error("invalid encrypted cookie")]
pub struct InvalidCookie;

// -------------------------------------------------------------------------------------------------
// Cookie value types
// -------------------------------------------------------------------------------------------------

/// The encrypted value stored in `__Host-session`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionValue {
    /// The OIDC refresh token.
    pub refresh_token: String,
}

impl SessionValue {
    #[must_use]
    pub const fn new(refresh_token: String) -> Self {
        Self { refresh_token }
    }

    /// Encrypt and encode the session value.
    ///
    /// # Panics
    ///
    /// Panics if JSON serialization fails; this value contains only a serializable string.
    #[must_use]
    pub fn to_cookie_value(&self, key: &CookieKey) -> String {
        let json = serde_json::to_vec(self).expect("a SessionValue serialises");
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(key.seal(&json))
    }

    /// Decode and authenticate a session cookie.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidCookie`] for invalid encoding, failed authentication, or invalid JSON.
    pub fn from_cookie_value(value: &str, key: &CookieKey) -> Result<Self, InvalidCookie> {
        let sealed = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(value)
            .map_err(|_| InvalidCookie)?;
        let json = key.open(&sealed).map_err(|_| InvalidCookie)?;
        serde_json::from_slice(&json).map_err(|_| InvalidCookie)
    }
}

/// The encrypted value stored in `__Host-login-flow`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlowValue {
    /// The `OAuth2` `state` parameter — a random string checked on callback.
    pub state: String,
    /// The PKCE code verifier.
    pub code_verifier: String,
    /// The PKCE code challenge method (S256).
    pub code_challenge_method: String,
}

impl FlowValue {
    #[must_use]
    pub fn new(state: String, code_verifier: String) -> Self {
        Self {
            state,
            code_verifier,
            code_challenge_method: "S256".to_owned(),
        }
    }

    /// Encrypt and encode the login flow value.
    ///
    /// # Panics
    ///
    /// Panics if JSON serialization fails; this value contains only serializable strings.
    #[must_use]
    pub fn to_cookie_value(&self, key: &CookieKey) -> String {
        let json = serde_json::to_vec(self).expect("a FlowValue serialises");
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(key.seal(&json))
    }

    /// Decode and authenticate a login flow cookie.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidCookie`] for invalid encoding, failed authentication, or invalid JSON.
    pub fn from_cookie_value(value: &str, key: &CookieKey) -> Result<Self, InvalidCookie> {
        let sealed = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(value)
            .map_err(|_| InvalidCookie)?;
        let json = key.open(&sealed).map_err(|_| InvalidCookie)?;
        serde_json::from_slice(&json).map_err(|_| InvalidCookie)
    }
}

// -------------------------------------------------------------------------------------------------
// Cookie setting/clearing helpers
// -------------------------------------------------------------------------------------------------

/// Build a `Set-Cookie` header value for the flow cookie.
pub(crate) fn set_flow_cookie(value: &str, max_age_secs: u64) -> String {
    format!(
        "__Host-login-flow={value}; HttpOnly; Secure; SameSite=Lax; Path=/; Max-Age={max_age_secs}"
    )
}

/// Build a `Set-Cookie` header value to clear the flow cookie.
pub(crate) fn clear_flow_cookie() -> String {
    "__Host-login-flow=; HttpOnly; Secure; SameSite=Lax; Path=/; Max-Age=0".to_owned()
}

/// Build a `Set-Cookie` header value for the session cookie.
pub(crate) fn set_session_cookie(value: &str) -> String {
    format!("__Host-session={value}; HttpOnly; Secure; SameSite=Strict; Path=/")
}

/// Build a `Set-Cookie` header value to clear the session cookie.
pub(crate) fn clear_session_cookie() -> String {
    "__Host-session=; HttpOnly; Secure; SameSite=Strict; Path=/; Max-Age=0".to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setting_decodes_standard_padded_key() {
        let bytes = [0xff_u8; KEY_LEN];
        let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
        let setting: CookieKeySetting =
            serde_json::from_value(serde_json::Value::String(encoded)).unwrap();
        assert_eq!(setting.decode().unwrap().0, bytes);
    }

    #[test]
    fn setting_rejects_missing_key() {
        assert!(matches!(
            CookieKeySetting::default().decode(),
            Err(KeyError::Missing)
        ));
    }

    #[test]
    fn setting_rejects_invalid_or_noncanonical_base64() {
        let padded = base64::engine::general_purpose::STANDARD.encode([0xff_u8; KEY_LEN]);
        let unpadded = padded.trim_end_matches('=').to_owned();
        let url_safe = base64::engine::general_purpose::URL_SAFE.encode([0xff_u8; KEY_LEN]);
        let mut noncanonical = padded.clone().into_bytes();
        noncanonical[padded.len() - 2] = b'9';
        let noncanonical = String::from_utf8(noncanonical).unwrap();
        for secret in [
            "not a base64 secret".to_owned(),
            unpadded,
            url_safe,
            noncanonical,
        ] {
            let error = CookieKeySetting(secret.clone()).decode().unwrap_err();
            assert!(matches!(error, KeyError::InvalidEncoding));
            assert!(!error.to_string().contains(&secret));
            assert!(!format!("{error:?}").contains(&secret));
        }
    }

    #[test]
    fn setting_rejects_wrong_decoded_lengths() {
        for len in [0, 1, KEY_LEN - 1, KEY_LEN + 1, KEY_LEN * 2] {
            let encoded = base64::engine::general_purpose::STANDARD.encode(vec![42_u8; len]);
            assert!(
                CookieKeySetting(encoded).decode().is_err(),
                "decoded length {len}"
            );
        }
    }

    #[test]
    fn key_formatting_redacts_secrets() {
        let bytes = [0xff_u8; KEY_LEN];
        let secret = base64::engine::general_purpose::STANDARD.encode(bytes);
        let setting = CookieKeySetting(secret.clone());
        let key = setting.decode().unwrap();
        for formatted in [
            format!("{setting}"),
            format!("{setting:?}"),
            format!("{setting:#?}"),
            format!("{key}"),
            format!("{key:?}"),
            format!("{key:#?}"),
            format!("{:?}", CookieKeySetting::default()),
            format!("{}", CookieKeySetting::default()),
        ] {
            assert!(formatted.contains("[REDACTED]"));
            assert!(!formatted.contains(&secret));
            assert!(!formatted.contains(&format!("{bytes:?}")));
        }
    }

    #[test]
    fn round_trip_session_value() {
        let key = CookieKey::fixed(7);
        let value = SessionValue::new("rt_test_token_123".to_owned());
        let encoded = value.to_cookie_value(&key);
        let decoded = SessionValue::from_cookie_value(&encoded, &key).expect("round trip");
        assert_eq!(decoded.refresh_token, "rt_test_token_123");
    }

    #[test]
    fn round_trip_flow_value() {
        let key = CookieKey::fixed(7);
        let value = FlowValue::new("state_abc".to_owned(), "verifier_xyz".to_owned());
        let encoded = value.to_cookie_value(&key);
        let decoded = FlowValue::from_cookie_value(&encoded, &key).expect("round trip");
        assert_eq!(decoded.state, "state_abc");
        assert_eq!(decoded.code_verifier, "verifier_xyz");
    }

    #[test]
    fn tampered_cookie_fails() {
        let key = CookieKey::fixed(7);
        let value = SessionValue::new("rt_test".to_owned());
        let encoded = value.to_cookie_value(&key);
        let sealed = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(encoded)
            .unwrap();
        for index in [0, NONCE_LEN, sealed.len() - 1] {
            let mut tampered = sealed.clone();
            tampered[index] ^= 1;
            assert!(key.open(&tampered).is_err(), "tamper at byte {index}");
            let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(tampered);
            assert!(SessionValue::from_cookie_value(&encoded, &key).is_err());
        }
    }

    #[test]
    fn wrong_key_fails() {
        let key_a = CookieKey::fixed(7);
        let key_b = CookieKey::fixed(42);
        let value = SessionValue::new("rt_test".to_owned());
        let encoded = value.to_cookie_value(&key_a);
        assert!(SessionValue::from_cookie_value(&encoded, &key_b).is_err());
    }

    #[test]
    fn flow_cookie_header_has_correct_attributes() {
        let header = set_flow_cookie("val", 600);
        assert!(header.contains("__Host-login-flow=val"));
        assert!(header.contains("HttpOnly"));
        assert!(header.contains("Secure"));
        assert!(header.contains("SameSite=Lax"));
        assert!(header.contains("Path=/"));
        assert!(header.contains("Max-Age=600"));
    }

    #[test]
    fn session_cookie_header_has_correct_attributes() {
        let header = set_session_cookie("val");
        assert!(header.contains("__Host-session=val"));
        assert!(header.contains("HttpOnly"));
        assert!(header.contains("Secure"));
        assert!(header.contains("SameSite=Strict"));
        assert!(header.contains("Path=/"));
        assert!(!header.contains("Max-Age"), "session cookie has no Max-Age");
    }

    #[test]
    fn clear_flow_cookie() {
        let header = super::clear_flow_cookie();
        assert!(header.contains("Max-Age=0"));
        assert!(header.contains("__Host-login-flow="));
    }

    #[test]
    fn clear_session_cookie() {
        let header = super::clear_session_cookie();
        assert!(header.contains("Max-Age=0"));
        assert!(header.contains("__Host-session="));
    }
}
