//! The host's device identity: an Ed25519 key that signs pass challenges, and an X25519 key that
//! is its Noise static key (`atomics_core::noise`). Both are made on first use and kept in the OS
//! credential store (`credentials::Store::device_item`), one item per ATOMICS home, so two homes
//! on one machine are two hosts. Only the public halves leave this module.

use std::path::Path;

use anyhow::{Result, anyhow};
use atomics_core::noise::{PublicKey, Secret};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::rand::SecureRandom;
use ring::signature::{Ed25519KeyPair, KeyPair};
use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, Zeroizing};

use crate::credentials::Store;

/// Never Debug: it holds the private keys.
pub struct DeviceKeys {
    signing: Ed25519KeyPair,
    /// The Noise static key: every channel's handshake answers with it.
    noise: Secret,
    noise_public: PublicKey,
}

/// The stored form: hex of the Ed25519 seed and of the X25519 secret. Never Debug.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Stored {
    v: u32,
    signing: String,
    noise: String,
}

impl Drop for Stored {
    fn drop(&mut self) {
        self.signing.zeroize();
        self.noise.zeroize();
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut hex, b| {
        let _ = write!(hex, "{b:02x}");
        hex
    })
}

fn unhex32(text: &str) -> Option<Zeroizing<[u8; 32]>> {
    let mut out = Zeroizing::new([0u8; 32]);
    if text.len() != 64 {
        return None;
    }
    for (slot, pair) in out.iter_mut().zip(text.as_bytes().chunks(2)) {
        let digit = |b: u8| (b as char).to_digit(16);
        *slot = (digit(pair[0])? * 16 + digit(pair[1])?) as u8;
    }
    Some(out)
}

impl DeviceKeys {
    /// The keys of the ATOMICS home `home`, made and stored the first time. The credential store's
    /// tool may wait for the keychain to unlock: never call this on the runtime thread.
    pub fn load_or_create(store: &Store, home: &Path) -> Result<Self> {
        let text = Zeroizing::new(store.device_item(&slot(home), fresh)?);
        Self::parse(&text)
    }

    fn parse(text: &str) -> Result<Self> {
        let invalid =
            || anyhow!("the stored device keys are malformed (contents hidden); remove them to make new ones");
        let stored: Stored = serde_json::from_str(text).map_err(|_| invalid())?;
        if stored.v != 1 {
            return Err(invalid());
        }
        let seed = unhex32(&stored.signing).ok_or_else(invalid)?;
        let noise = unhex32(&stored.noise).ok_or_else(invalid)?;
        let signing = Ed25519KeyPair::from_seed_unchecked(&seed[..]).map_err(|_| invalid())?;
        let noise = Secret::from_bytes(&noise);
        let noise_public = noise.public();
        Ok(Self { signing, noise, noise_public })
    }

    /// The Ed25519 public key, base64url without padding.
    pub fn signing_public(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.signing.public_key().as_ref())
    }

    /// The X25519 public key, base64url without padding.
    pub fn noise_public(&self) -> String {
        self.noise_public.text()
    }

    pub fn noise_key(&self) -> PublicKey {
        self.noise_public
    }

    /// The Noise static private key, for a channel's handshake.
    pub fn noise_secret(&self) -> &Secret {
        &self.noise
    }

    /// The signature (base64url) that buys a pass: over `atomics_core::tunnel::pass_message`.
    pub fn sign_challenge(&self, host_id: &str, challenge: &str) -> String {
        let message = atomics_core::tunnel::pass_message(host_id, challenge);
        URL_SAFE_NO_PAD.encode(self.signing.sign(message.as_bytes()).as_ref())
    }
}

/// New keys, as stored.
fn fresh() -> Result<String> {
    let random = ring::rand::SystemRandom::new();
    let (mut seed, mut noise) = (Zeroizing::new([0u8; 32]), Zeroizing::new([0u8; 32]));
    random
        .fill(&mut seed[..])
        .and_then(|()| random.fill(&mut noise[..]))
        .map_err(|_| anyhow!("no system randomness"))?;
    Ok(serde_json::to_string(&Stored { v: 1, signing: hex(&seed[..]), noise: hex(&noise[..]) })?)
}

/// The credential-store slot of `home`'s keys: a digest of the path, so the item names no folder.
pub fn slot(home: &Path) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, home.as_os_str().as_encoded_bytes());
    format!("device-{}", hex(&digest.as_ref()[..8]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::signature::{ED25519, UnparsedPublicKey};

    fn stored(signing: &str, noise: &str) -> String {
        serde_json::to_string(&Stored { v: 1, signing: signing.into(), noise: noise.into() }).unwrap()
    }

    fn b64(text: &str) -> Vec<u8> {
        URL_SAFE_NO_PAD.decode(text).unwrap()
    }

    #[test]
    fn public_keys_match_the_rfc_vectors() {
        // RFC 8032 7.1 test 1 (Ed25519), RFC 7748 6.1 Alice (X25519).
        let keys = DeviceKeys::parse(&stored(
            "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60",
            "77076d0a7318a57d3c16c17251b26645df4c2f87ebc0992ab177fba51db92c2a",
        ))
        .unwrap();
        assert_eq!(
            hex(&b64(&keys.signing_public())),
            "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a"
        );
        assert_eq!(hex(&b64(&keys.noise_public())), "8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a");
        assert!(!keys.signing_public().contains(['=', '+', '/']), "base64url without padding");
        let signature = b64(&keys.sign_challenge("h-1", "c-1"));
        assert_eq!(signature.len(), 64);
        UnparsedPublicKey::new(&ED25519, b64(&keys.signing_public()))
            .verify(b"atomics-tunnel-pass-v1\nh-1\nc-1", &signature)
            .expect("the signature covers context, host id and challenge on three lines");
    }

    #[test]
    fn keys_are_made_once_per_home_and_kept_in_the_credential_store() {
        let (dir, _, store) = crate::credentials::tests::machine(false);
        let (home, other) = (dir.path().join("home"), dir.path().join("other"));
        let first = DeviceKeys::load_or_create(&store, &home).unwrap();
        let again = DeviceKeys::load_or_create(&store, &home).unwrap();
        assert_eq!(first.signing_public(), again.signing_public(), "read back, not made again");
        assert_eq!(first.noise_public(), again.noise_public());
        let elsewhere = DeviceKeys::load_or_create(&store, &other).unwrap();
        assert_ne!(first.signing_public(), elsewhere.signing_public(), "another home is another host");
        assert_ne!(first.noise_public(), first.signing_public());
        // The item is hex in the store, named by a digest: no key or folder in clear.
        let item = std::fs::read_to_string(dir.path().join(format!("keychain-device-{}", slot(&home)))).unwrap();
        assert!(item.trim().bytes().all(|b| b.is_ascii_hexdigit()));
        assert!(!slot(&home).contains("home") && slot(&home).starts_with("device-"));
        let argv = std::fs::read_to_string(dir.path().join("keychain-argv")).unwrap();
        assert!(!argv.contains(item.trim()), "values reach the keychain tool on stdin, never argv");
        // Removing it makes new keys.
        store.remove_device_item(&slot(&home)).unwrap();
        let rotated = DeviceKeys::load_or_create(&store, &home).unwrap();
        assert_ne!(rotated.signing_public(), first.signing_public());
    }

    #[test]
    fn a_malformed_item_is_refused_without_quoting_it() {
        for text in [
            "not json".to_string(),
            stored("00", "11"),
            stored(&"zz".repeat(32), &"11".repeat(32)),
            r#"{"v":2,"signing":"","noise":""}"#.to_string(),
        ] {
            let error = DeviceKeys::parse(&text).err().expect("refused").to_string();
            assert!(error.contains("malformed") && !error.contains("zz") && !error.contains("json"), "{error}");
        }
    }
}
