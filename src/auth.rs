//! Wallet authentication for realtime sessions.
//!
//! Flow: on connect the server emits `auth nonce { nonce }`. The client signs
//! `ekza-space:auth:<nonce>` with its wallet (`signMessage`) and emits
//! `auth { pubkey, signature }`. Both are base58. The nonce is bound to the
//! socket and consumed on first successful verification, so a captured
//! signature cannot be replayed on another connection.

use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use rand::RngCore;
use solana_pubkey::Pubkey;

pub const AUTH_MESSAGE_PREFIX: &str = "ekza-space:auth:";

pub fn new_nonce() -> String {
    let mut bytes = [0u8; 24];
    rand::thread_rng().fill_bytes(&mut bytes);
    bs58::encode(bytes).into_string()
}

pub fn auth_message(nonce: &str) -> String {
    format!("{AUTH_MESSAGE_PREFIX}{nonce}")
}

#[derive(Debug, PartialEq, Eq)]
pub enum AuthError {
    BadPubkey,
    BadSignature,
    Mismatch,
}

/// Verify that `signature_b58` is a valid ed25519 signature over
/// `auth_message(nonce)` by `pubkey_b58`.
pub fn verify(pubkey_b58: &str, signature_b58: &str, nonce: &str) -> Result<Pubkey, AuthError> {
    let pubkey: Pubkey = pubkey_b58.parse().map_err(|_| AuthError::BadPubkey)?;
    let key = VerifyingKey::from_bytes(&pubkey.to_bytes()).map_err(|_| AuthError::BadPubkey)?;

    let sig_bytes = bs58::decode(signature_b58)
        .into_vec()
        .map_err(|_| AuthError::BadSignature)?;
    let sig_bytes: [u8; 64] = sig_bytes.try_into().map_err(|_| AuthError::BadSignature)?;
    let signature = Signature::from_bytes(&sig_bytes);

    key.verify(auth_message(nonce).as_bytes(), &signature)
        .map(|_| pubkey)
        .map_err(|_| AuthError::Mismatch)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    fn keypair() -> (SigningKey, String) {
        let mut seed = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut seed);
        let sk = SigningKey::from_bytes(&seed);
        let pk = bs58::encode(sk.verifying_key().to_bytes()).into_string();
        (sk, pk)
    }

    #[test]
    fn accepts_valid_signature() {
        let (sk, pk) = keypair();
        let nonce = new_nonce();
        let sig = sk.sign(auth_message(&nonce).as_bytes());
        let sig_b58 = bs58::encode(sig.to_bytes()).into_string();
        let got = verify(&pk, &sig_b58, &nonce).unwrap();
        assert_eq!(got.to_string(), pk);
    }

    #[test]
    fn rejects_wrong_nonce_and_wrong_key() {
        let (sk, pk) = keypair();
        let (_, other_pk) = keypair();
        let nonce = new_nonce();
        let sig = sk.sign(auth_message(&nonce).as_bytes());
        let sig_b58 = bs58::encode(sig.to_bytes()).into_string();
        assert_eq!(
            verify(&pk, &sig_b58, "other-nonce"),
            Err(AuthError::Mismatch)
        );
        assert_eq!(
            verify(&other_pk, &sig_b58, &nonce),
            Err(AuthError::Mismatch)
        );
        assert_eq!(
            verify("not-a-key", &sig_b58, &nonce),
            Err(AuthError::BadPubkey)
        );
        assert_eq!(verify(&pk, "zz", &nonce), Err(AuthError::BadSignature));
    }
}
