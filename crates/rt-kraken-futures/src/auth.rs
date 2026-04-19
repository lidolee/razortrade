//! Kraken Futures request signing.
//!
//! Kraken Futures uses a two-stage hash scheme:
//!
//! 1. `SHA-256(postData || nonce || endpointPath)` — the message hash.
//! 2. `HMAC-SHA-512` of that, using `base64_decode(api_secret)` as the key.
//! 3. Base64-encode the HMAC output → `Authent` header value.
//!
//! The WebSocket challenge flow is a slight variant: the "message" is just
//! the challenge UUID string, not a concatenation.
//!
//! ## Common gotchas (from Kraken's own FAQ)
//!
//! - The secret MUST be base64-decoded before being used as the HMAC key.
//!   Using the base64 string directly is the most common failure mode.
//! - `endpointPath` is the path suffix (e.g. `/api/v3/sendorder`), NOT the
//!   full URL. It must exactly match what the HTTP request hits.
//! - For GET requests `postData` is the URL query string (without the
//!   leading `?`). For POST requests with a JSON body it is the raw JSON.
//!   For endpoints with no params, `postData` is an empty string.
//! - Nonces must be strictly monotonic per API key. A duplicate or
//!   lower-than-previous nonce triggers `INVALID_SIGNATURE` on the server.

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256, Sha512};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;

type HmacSha512 = Hmac<Sha512>;

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("base64 decode of api_secret failed: {0}")]
    Base64Decode(#[from] base64::DecodeError),

    #[error("base64 key has invalid length for HMAC-SHA-512")]
    InvalidKeyLength,

    #[error("system clock is before UNIX epoch")]
    SystemClockBeforeEpoch,
}

/// Sign a Kraken Futures REST request. Returns the value that goes into
/// the `Authent` HTTP header.
///
/// # Arguments
///
/// * `api_secret_b64` — the raw secret from Kraken account management, as-is.
/// * `post_data` — the request payload. For GET: the query string without `?`.
///   For POST with JSON body: the raw JSON string. For empty bodies: `""`.
/// * `nonce` — monotonically increasing nonce string.
/// * `endpoint_path` — path suffix, e.g. `/api/v3/sendorder`. No host, no scheme.
pub fn sign_rest_request(
    api_secret_b64: &str,
    post_data: &str,
    nonce: &str,
    endpoint_path: &str,
) -> Result<String, AuthError> {
    // Step 1: SHA-256 of (postData || nonce || endpointPath).
    let mut hasher = Sha256::new();
    hasher.update(post_data.as_bytes());
    hasher.update(nonce.as_bytes());
    hasher.update(endpoint_path.as_bytes());
    let sha256_digest = hasher.finalize();

    // Step 2: Base64-decode the secret.
    let secret_bytes = BASE64.decode(api_secret_b64.as_bytes())?;

    // Step 3: HMAC-SHA-512 of the SHA-256 digest using the decoded secret.
    let mut mac =
        HmacSha512::new_from_slice(&secret_bytes).map_err(|_| AuthError::InvalidKeyLength)?;
    mac.update(&sha256_digest);
    let hmac_bytes = mac.finalize().into_bytes();

    // Step 4: Base64-encode the HMAC result.
    Ok(BASE64.encode(hmac_bytes))
}

/// Sign a WebSocket challenge. Kraken delivers a UUID string; we hash it
/// with SHA-256, then HMAC-SHA-512 with the decoded secret, then base64.
pub fn sign_ws_challenge(
    api_secret_b64: &str,
    challenge: &str,
) -> Result<String, AuthError> {
    let mut hasher = Sha256::new();
    hasher.update(challenge.as_bytes());
    let sha256_digest = hasher.finalize();

    let secret_bytes = BASE64.decode(api_secret_b64.as_bytes())?;
    let mut mac =
        HmacSha512::new_from_slice(&secret_bytes).map_err(|_| AuthError::InvalidKeyLength)?;
    mac.update(&sha256_digest);
    let hmac_bytes = mac.finalize().into_bytes();

    Ok(BASE64.encode(hmac_bytes))
}

/// Monotonic nonce generator. Kraken requires nonces to be strictly
/// increasing per API key; collisions or regressions cause immediate
/// authentication failure.
///
/// We use millisecond UNIX timestamps, with atomic max-tracking to ensure
/// that bursts faster than 1ms (rare in practice but possible) still produce
/// unique values.
#[derive(Debug)]
pub struct NonceGenerator {
    last: AtomicU64,
}

impl NonceGenerator {
    pub fn new() -> Self {
        Self {
            last: AtomicU64::new(0),
        }
    }

    pub fn next(&self) -> Result<String, AuthError> {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| AuthError::SystemClockBeforeEpoch)?
            .as_millis() as u64;

        // Ensure strict monotonicity: always return max(now, last + 1).
        let mut last = self.last.load(Ordering::Acquire);
        loop {
            let candidate = now_ms.max(last + 1);
            match self.last.compare_exchange_weak(
                last,
                candidate,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Ok(candidate.to_string()),
                Err(observed) => last = observed,
            }
        }
    }
}

impl Default for NonceGenerator {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test that our signer is deterministic and produces base64 output
    /// of the expected length (HMAC-SHA-512 digest is 64 bytes → 88 chars
    /// base64 with padding). This does not verify against a Kraken-provided
    /// test vector (none published with expected output), but catches
    /// regressions and confirms the plumbing is intact.
    #[test]
    fn rest_signature_is_deterministic() {
        let secret = "kQH5HW/8p1uGOVjbgWA7FunAmGO8lsSUXNsu3eow76sz84Q18fWxnyRzBHCd3pd5nE9qa99HAZtuZuj6F1huXg==";
        let post_data = r#"{"orderType":"lmt","symbol":"PI_XBTUSD","side":"buy","size":1,"limitPrice":40000}"#;
        let nonce = "1616492376594";
        let path = "/api/v3/sendorder";

        let sig_a = sign_rest_request(secret, post_data, nonce, path).unwrap();
        let sig_b = sign_rest_request(secret, post_data, nonce, path).unwrap();
        assert_eq!(sig_a, sig_b, "signature must be deterministic");
        assert_eq!(sig_a.len(), 88, "HMAC-SHA-512 base64 length is 88 chars");
    }

    #[test]
    fn rest_signature_changes_with_nonce() {
        let secret = "kQH5HW/8p1uGOVjbgWA7FunAmGO8lsSUXNsu3eow76sz84Q18fWxnyRzBHCd3pd5nE9qa99HAZtuZuj6F1huXg==";
        let path = "/api/v3/sendorder";
        let a = sign_rest_request(secret, "", "1000", path).unwrap();
        let b = sign_rest_request(secret, "", "1001", path).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn rest_signature_changes_with_path() {
        let secret = "kQH5HW/8p1uGOVjbgWA7FunAmGO8lsSUXNsu3eow76sz84Q18fWxnyRzBHCd3pd5nE9qa99HAZtuZuj6F1huXg==";
        let a = sign_rest_request(secret, "", "1000", "/api/v3/sendorder").unwrap();
        let b = sign_rest_request(secret, "", "1000", "/api/v3/cancelorder").unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn invalid_base64_secret_returns_error() {
        let bad = "@@@not base64@@@";
        let err = sign_rest_request(bad, "", "1", "/api/v3/sendorder");
        assert!(err.is_err());
    }

    #[test]
    fn ws_challenge_signature_length() {
        let secret = "kQH5HW/8p1uGOVjbgWA7FunAmGO8lsSUXNsu3eow76sz84Q18fWxnyRzBHCd3pd5nE9qa99HAZtuZuj6F1huXg==";
        let challenge = "c100b894-1729-464d-ace1-52dbce11db42";
        let sig = sign_ws_challenge(secret, challenge).unwrap();
        assert_eq!(sig.len(), 88);
    }

    #[test]
    fn nonce_is_strictly_monotonic() {
        let gen = NonceGenerator::new();
        let mut prev: u64 = 0;
        for _ in 0..10_000 {
            let n: u64 = gen.next().unwrap().parse().unwrap();
            assert!(n > prev, "nonce not monotonic: {n} after {prev}");
            prev = n;
        }
    }
}
