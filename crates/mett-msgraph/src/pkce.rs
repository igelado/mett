//! Proof Key for Code Exchange (RFC 7636) and random state values.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};

/// A PKCE code verifier and its S256 challenge.
pub(crate) struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

impl Pkce {
    /// Generates a fresh verifier from 32 random bytes (43 characters).
    pub fn generate() -> Result<Self, getrandom::Error> {
        Ok(Self::from_verifier(random_token()?))
    }

    fn from_verifier(verifier: String) -> Self {
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        Self {
            verifier,
            challenge,
        }
    }
}

/// 32 random bytes, base64url-encoded. Used for verifiers and `state`.
pub(crate) fn random_token() -> Result<String, getrandom::Error> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes)?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_rfc7636_example() {
        // RFC 7636, appendix B.
        let pkce = Pkce::from_verifier("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk".into());
        assert_eq!(
            pkce.challenge,
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn generated_verifiers_are_long_enough_and_unique() {
        let a = Pkce::generate().unwrap();
        let b = Pkce::generate().unwrap();
        assert_eq!(a.verifier.len(), 43);
        assert_ne!(a.verifier, b.verifier);
    }
}
