//! PKCE, and the random values the flow depends on being unguessable.
//!
//! PKCE (RFC 7636) exists because a public client cannot keep a secret. Without it, anyone who
//! intercepts the authorization code — another process watching the loopback port, a browser
//! extension, a logged URL — can redeem it, since redeeming needs only the code and a client id
//! that is published in the binary. PKCE closes that by having the client invent a random
//! `code_verifier`, send only its SHA-256 digest up front, and reveal the verifier when it
//! redeems the code. An intercepted code is then useless without a value that never travelled
//! over the same channel.

use secrecy::SecretString;
use sha2::{Digest, Sha256};

use crate::error::{Error, ErrorKind, Result};
use crate::http::base64;

/// Bytes drawn from the OS for a verifier or a state value.
///
/// 32 bytes encode to 43 unpadded base64url characters, which is exactly RFC 7636 §4.1's
/// minimum verifier length and comfortably past guessing.
const ENTROPY_BYTES: usize = 32;

/// A PKCE verifier and the challenge derived from it.
pub struct Pkce {
    verifier: SecretString,
    challenge: String,
}

impl Pkce {
    /// A fresh verifier from the operating system's random source.
    pub fn generate() -> Result<Self> {
        Ok(Self::from_verifier(&random_token()?))
    }

    /// Derive the challenge for a verifier that already exists.
    ///
    /// Exists for the RFC 7636 Appendix B test vector, which is the only way to know the S256
    /// derivation is right without a server to tell us it is wrong.
    pub fn from_verifier(verifier: &str) -> Self {
        Self {
            challenge: base64::encode_url_nopad(Sha256::digest(verifier.as_bytes())),
            verifier: SecretString::from(verifier.to_owned()),
        }
    }

    /// The `code_challenge` to send with the authorize request. Not secret: its whole purpose is
    /// to be published in a URL.
    pub fn challenge(&self) -> &str {
        &self.challenge
    }

    /// The `code_verifier` to send with the token request. Secret until then.
    pub fn verifier(&self) -> &SecretString {
        &self.verifier
    }
}

/// Hand-written, so that a `#[derive(Debug)]` anywhere upstream cannot print the verifier.
impl std::fmt::Debug for Pkce {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pkce")
            .field("challenge", &self.challenge)
            .field("verifier", &"<redacted>")
            .finish()
    }
}

/// A random `state` value, which is what ties a callback to the request that caused it.
///
/// Without it, anything that can reach the loopback port can hand gea an authorization code of
/// its choosing and have gea exchange it, filing an attacker's session under the user's name.
pub fn random_state() -> Result<String> {
    random_token()
}

fn random_token() -> Result<String> {
    let mut bytes = [0u8; ENTROPY_BYTES];
    getrandom::fill(&mut bytes)
        .map_err(|e| Error::new(ErrorKind::OauthEntropyUnavailable { cause: e.to_string() }))?;
    Ok(base64::encode_url_nopad(bytes))
}

#[cfg(test)]
mod tests {
    use secrecy::ExposeSecret;

    use super::*;

    /// RFC 7636 Appendix B's own vector. If this passes, the S256 derivation — digest, then
    /// unpadded base64url, in that order — is right. If it fails, every login fails with a
    /// server-side message about the code challenge that says nothing about which half is wrong.
    #[test]
    fn the_rfc_7636_test_vector_produces_the_documented_challenge() {
        let p = Pkce::from_verifier("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk");
        assert_eq!(p.challenge(), "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    /// RFC 7636 §4.1 fixes the length at 43 to 128 characters drawn from an unreserved set.
    /// A padded or standard-alphabet encoding would satisfy neither.
    #[test]
    fn a_generated_verifier_is_43_unreserved_characters() {
        let p = Pkce::generate().expect("the OS random source is available in a test");
        let v = p.verifier().expose_secret().to_owned();
        assert_eq!(v.len(), 43, "{v}");
        assert!(
            v.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~')),
            "{v} contains a character RFC 7636 does not allow in a verifier"
        );
    }

    /// Cheap smoke test that the CSPRNG is actually wired up. A constant would pass every other
    /// test in this file.
    #[test]
    fn two_generated_values_differ() {
        let a = random_state().expect("the OS random source is available in a test");
        let b = random_state().expect("the OS random source is available in a test");
        assert_ne!(a, b);
    }

    #[test]
    fn a_pkce_debug_never_prints_the_verifier() {
        let p = Pkce::from_verifier("supersecret-verifier-value");
        let d = format!("{p:?}");
        assert!(!d.contains("supersecret"), "{d}");
        assert!(d.contains(p.challenge()), "the challenge is not a secret and is useful: {d}");
    }
}
