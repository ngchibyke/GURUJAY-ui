use aes_gcm::{
    Aes256Gcm, Key, Nonce,
    aead::{Aead, AeadCore, KeyInit, OsRng},
};
use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::error::AppError;

const NONCE_LEN: usize = 12;

/// AES-256-GCM field encryption. Output layout: 12-byte random nonce || ciphertext+tag.
#[derive(Clone)]
pub struct FieldCipher {
    cipher: Aes256Gcm,
}

impl std::fmt::Debug for FieldCipher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FieldCipher(<redacted>)")
    }
}

impl FieldCipher {
    pub fn new(key: [u8; 32]) -> Self {
        Self {
            cipher: Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key)),
        }
    }

    pub fn encrypt(&self, plain: &str) -> Vec<u8> {
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let ct = self
            .cipher
            .encrypt(&nonce, plain.as_bytes())
            .expect("AES-GCM encryption of in-memory data cannot fail");
        let mut out = Vec::with_capacity(NONCE_LEN + ct.len());
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ct);
        out
    }

    pub fn decrypt(&self, blob: &[u8]) -> Result<String, AppError> {
        if blob.len() < NONCE_LEN {
            return Err(anyhow::anyhow!("encrypted field is malformed").into());
        }
        let (nonce, ct) = blob.split_at(NONCE_LEN);
        let plain = self
            .cipher
            .decrypt(Nonce::from_slice(nonce), ct)
            .map_err(|_| anyhow::anyhow!("encrypted field failed authentication"))?;
        String::from_utf8(plain)
            .map_err(|_| anyhow::anyhow!("decrypted field is not valid UTF-8").into())
    }
}

/// Deterministic HMAC-SHA256 blind index for equality lookups on encrypted fields.
pub fn blind_index(key: &[u8; 32], value: &str) -> Vec<u8> {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(value.as_bytes());
    mac.finalize().into_bytes().to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let c = FieldCipher::new([7; 32]);
        let b = c.encrypt("+2348031234567");
        assert_eq!(c.decrypt(&b).unwrap(), "+2348031234567");
    }
    #[test]
    fn same_plain_different_ciphertext() {
        let c = FieldCipher::new([7; 32]);
        assert_ne!(c.encrypt("x"), c.encrypt("x"));
    }
    #[test]
    fn tampered_blob_fails() {
        let c = FieldCipher::new([7; 32]);
        let mut b = c.encrypt("x");
        *b.last_mut().unwrap() ^= 1;
        assert!(c.decrypt(&b).is_err());
    }
    #[test]
    fn short_blob_fails() {
        let c = FieldCipher::new([7; 32]);
        assert!(c.decrypt(&[1, 2, 3]).is_err());
    }
    #[test]
    fn wrong_key_fails() {
        let b = FieldCipher::new([7; 32]).encrypt("x");
        assert!(FieldCipher::new([8; 32]).decrypt(&b).is_err());
    }
    #[test]
    fn debug_does_not_leak_key() {
        let s = format!("{:?}", FieldCipher::new([7; 32]));
        assert!(s.contains("redacted"));
    }
    #[test]
    fn index_is_deterministic() {
        assert_eq!(blind_index(&[1; 32], "a"), blind_index(&[1; 32], "a"));
        assert_ne!(blind_index(&[1; 32], "a"), blind_index(&[2; 32], "a"));
    }
}
