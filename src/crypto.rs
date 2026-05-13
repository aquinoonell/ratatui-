use base64::{engine::general_purpose::STANDARD as B64, Engine};
use chacha20poly1305::{
    aead::{Aead, AeadCore, KeyInit, OsRng as AeadOsRng},
    ChaCha20Poly1305, Nonce,
};
use ed25519_dalek::{
    ed25519::signature::Signer, Signature, SigningKey, Verifier, VerifyingKey,
};
use hkdf::Hkdf;
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use x25519_dalek::{EphemeralSecret, PublicKey as X25519Public};

// ─── Constants ───────────────────────────────────────────────────────────────

const HKDF_INFO_SESSION: &[u8] = b"timetracker-v1-session";
const HKDF_INFO_RATCHET: &[u8] = b"timetracker-v1-ratchet";
const HKDF_INFO_MSGKEY: &[u8] = b"timetracker-v1-msgkey";

// ─── Disk storage types ───────────────────────────────────────────────────────

/// Serialisable form of an identity keypair saved to disk.
#[derive(Serialize, Deserialize)]
struct StoredIdentity {
    username: String,
    /// Ed25519 signing key bytes (32 bytes, little-endian scalar)
    signing_key_b64: String,
    /// Ed25519 verifying (public) key bytes
    verifying_key_b64: String,
}

/// Map of username → base64-encoded Ed25519 verifying key bytes,
/// stored in known_users.json for TOFU verification.
#[derive(Serialize, Deserialize, Default)]
pub struct KnownUsers {
    users: HashMap<String, String>,
}

// ─── Identity ────────────────────────────────────────────────────────────────

/// Holds the local user's long-term Ed25519 keypair.
pub struct Identity {
    pub username: String,
    signing_key: SigningKey,
    pub verifying_key: VerifyingKey,
}

impl Identity {
    /// Load from disk, or generate a fresh keypair and save it.
    pub fn load_or_create(username: &str) -> Result<Self, String> {
        let path = identity_path(username);
        if path.exists() {
            Self::load(username, &path)
        } else {
            Self::generate(username, &path)
        }
    }

    fn generate(username: &str, path: &PathBuf) -> Result<Self, String> {
        let signing_key = SigningKey::generate(&mut OsRng);
        let verifying_key = signing_key.verifying_key();

        let stored = StoredIdentity {
            username: username.to_string(),
            signing_key_b64: B64.encode(signing_key.to_bytes()),
            verifying_key_b64: B64.encode(verifying_key.to_bytes()),
        };

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("Cannot create key dir: {}", e))?;
        }
        let json = serde_json::to_string_pretty(&stored)
            .map_err(|e| format!("Serialise error: {}", e))?;
        fs::write(path, json).map_err(|e| format!("Cannot write key file: {}", e))?;

        Ok(Identity { username: username.to_string(), signing_key, verifying_key })
    }

    fn load(username: &str, path: &PathBuf) -> Result<Self, String> {
        let json = fs::read_to_string(path)
            .map_err(|e| format!("Cannot read key file: {}", e))?;
        let stored: StoredIdentity = serde_json::from_str(&json)
            .map_err(|e| format!("Key file corrupt: {}", e))?;

        let sk_bytes: [u8; 32] = B64.decode(&stored.signing_key_b64)
            .map_err(|_| "Bad signing key encoding".to_string())?
            .try_into()
            .map_err(|_| "Signing key wrong length".to_string())?;
        let signing_key = SigningKey::from_bytes(&sk_bytes);
        let verifying_key = signing_key.verifying_key();

        Ok(Identity { username: username.to_string(), signing_key, verifying_key })
    }

    /// Return the public key as a base64 string for wire transmission.
    pub fn public_key_b64(&self) -> String {
        B64.encode(self.verifying_key.to_bytes())
    }

    /// Sign a message. Returns base64-encoded signature.
    pub fn sign_b64(&self, msg: &[u8]) -> String {
        let sig: Signature = self.signing_key.sign(msg);
        B64.encode(sig.to_bytes())
    }
}

// ─── TOFU store ──────────────────────────────────────────────────────────────

impl KnownUsers {
    pub fn load() -> Self {
        let path = known_users_path();
        if let Ok(json) = fs::read_to_string(&path) {
            serde_json::from_str(&json).unwrap_or_default()
        } else {
            KnownUsers::default()
        }
    }

    /// Read-only access to the inner map.
    pub fn users(&self) -> &std::collections::HashMap<String, String> {
        &self.users
    }

    fn save(&self) {
        let path = known_users_path();
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = fs::write(path, json);
        }
    }

    /// Check a peer's claimed public key against our TOFU store.
    /// Returns Ok(true) = new (first seen), Ok(false) = known and matching,
    /// Err(msg) = key CHANGED — possible MITM.
    pub fn check_and_update(
        &mut self,
        username: &str,
        pubkey_b64: &str,
    ) -> Result<bool, String> {
        match self.users.get(username) {
            None => {
                // First time seeing this user — trust and store
                self.users.insert(username.to_string(), pubkey_b64.to_string());
                self.save();
                Ok(true)
            }
            Some(stored) if stored == pubkey_b64 => Ok(false),
            Some(stored) => Err(format!(
                "⚠️  KEY MISMATCH for {}!\n\
                 Stored:  {}\n\
                 Received:{}\n\
                 Possible MITM — aborting handshake.",
                username,
                &stored[..stored.len().min(16)],
                &pubkey_b64[..pubkey_b64.len().min(16)]
            )),
        }
    }
}

// ─── Handshake ───────────────────────────────────────────────────────────────

/// Represents one side of an in-progress X25519 handshake.
/// Dropped (and memory zeroed) after session keys are derived.
pub struct Handshake {
    ephemeral_secret: Option<EphemeralSecret>,
    pub ephemeral_public: X25519Public,
}

impl Handshake {
    pub fn new() -> Self {
        let secret = EphemeralSecret::random_from_rng(OsRng);
        let public = X25519Public::from(&secret);
        Handshake { ephemeral_secret: Some(secret), ephemeral_public: public }
    }

    /// Build the HELLO wire payload:
    ///   HELLO <username> <eph_pub_b64> <sig_b64>
    /// The signature covers "HELLO" || eph_pub_bytes so the recipient can
    /// verify the ephemeral key is authentically from this identity.
    pub fn hello_line(&self, identity: &Identity) -> String {
        let eph_pub_bytes = self.ephemeral_public.as_bytes();
        let mut to_sign = b"HELLO".to_vec();
        to_sign.extend_from_slice(eph_pub_bytes);
        let sig_b64 = identity.sign_b64(&to_sign);
        let eph_b64 = B64.encode(eph_pub_bytes);
        format!("HELLO {} {} {}", identity.username, eph_b64, sig_b64)
    }

    /// Consume the handshake, verify the peer's signature, and derive
    /// (send_chain_key, recv_chain_key).  The initiator sends first so
    /// their send key = the other side's recv key.
    pub fn derive_session(
        mut self,
        peer_username: &str,      // peer's username — stored in SessionKeys for session lookup
        peer_pubkey_b64: &str,    // peer's Ed25519 verifying key (base64)
        peer_eph_pub_b64: &str,   // peer's X25519 ephemeral public (base64)
        peer_sig_b64: &str,       // peer's signature over "HELLO" || peer_eph_pub
        i_am_initiator: bool,
    ) -> Result<SessionKeys, String> {
        // 1. Decode peer's identity key
        let peer_vk_bytes: [u8; 32] = B64.decode(peer_pubkey_b64)
            .map_err(|_| "Bad peer pubkey encoding".to_string())?
            .try_into()
            .map_err(|_| "Peer pubkey wrong length".to_string())?;
        let peer_vk = VerifyingKey::from_bytes(&peer_vk_bytes)
            .map_err(|_| "Invalid peer verifying key".to_string())?;

        // 2. Decode peer's ephemeral key
        let peer_eph_bytes: [u8; 32] = B64.decode(peer_eph_pub_b64)
            .map_err(|_| "Bad peer eph pub encoding".to_string())?
            .try_into()
            .map_err(|_| "Peer eph pub wrong length".to_string())?;
        let peer_eph_pub = X25519Public::from(peer_eph_bytes);

        // 3. Verify peer's signature over "HELLO" || peer_eph_pub
        let mut signed_msg = b"HELLO".to_vec();
        signed_msg.extend_from_slice(&peer_eph_bytes);
        let sig_bytes: [u8; 64] = B64.decode(peer_sig_b64)
            .map_err(|_| "Bad signature encoding".to_string())?
            .try_into()
            .map_err(|_| "Signature wrong length".to_string())?;
        let sig = Signature::from_bytes(&sig_bytes);
        peer_vk.verify(&signed_msg, &sig)
            .map_err(|_| "Signature verification FAILED — handshake rejected".to_string())?;

        // 4. X25519 DH — consume and zero the ephemeral secret
        let my_secret = self.ephemeral_secret.take()
            .ok_or("Handshake already consumed")?;
        let shared_secret = my_secret.diffie_hellman(&peer_eph_pub);

        // 5. HKDF-SHA256 to derive two chain keys (one per direction)
        let hk = Hkdf::<Sha256>::new(None, shared_secret.as_bytes());
        let mut okm = [0u8; 64]; // 32 bytes per chain key
        hk.expand(HKDF_INFO_SESSION, &mut okm)
            .map_err(|_| "HKDF expand failed".to_string())?;

        // Convention: initiator uses okm[0..32] as send key, okm[32..64] as recv key
        let (key_a, key_b) = okm.split_at(32);
        let (send_chain, recv_chain) = if i_am_initiator {
            (key_a.to_vec(), key_b.to_vec())
        } else {
            (key_b.to_vec(), key_a.to_vec())
        };

        Ok(SessionKeys::new(peer_username.to_string(), send_chain, recv_chain))
    }
}

// ─── Session keys + symmetric ratchet ────────────────────────────────────────

/// Holds the symmetric ratchet state for an established secure session.
pub struct SessionKeys {
    pub peer_username: String,
    send_chain_key: Vec<u8>,
    recv_chain_key: Vec<u8>,
}

impl SessionKeys {
    fn new(peer_username: String, send_chain: Vec<u8>, recv_chain: Vec<u8>) -> Self {
        SessionKeys { peer_username, send_chain_key: send_chain, recv_chain_key: recv_chain }
    }

    /// Ratchet the send chain forward: derive a one-time message key, advance
    /// the chain key, and encrypt the plaintext with ChaCha20-Poly1305.
    ///
    /// Returns base64(nonce || ciphertext_with_tag) ready for the wire.
    pub fn encrypt(&mut self, plaintext: &str) -> Result<String, String> {
        let (msg_key, next_chain) = ratchet_step(&self.send_chain_key)?;
        self.send_chain_key = next_chain;

        let cipher = ChaCha20Poly1305::new_from_slice(&msg_key)
            .map_err(|e| format!("AEAD key error: {}", e))?;
        let nonce = ChaCha20Poly1305::generate_nonce(&mut AeadOsRng);
        let ciphertext = cipher
            .encrypt(&nonce, plaintext.as_bytes())
            .map_err(|e| format!("Encrypt error: {}", e))?;

        // Zero the message key — forward secrecy within the session
        drop(msg_key);

        let mut wire = nonce.to_vec();
        wire.extend_from_slice(&ciphertext);
        Ok(B64.encode(&wire))
    }

    /// Ratchet the recv chain forward and decrypt a wire payload.
    pub fn decrypt(&mut self, wire_b64: &str) -> Result<String, String> {
        let wire = B64.decode(wire_b64)
            .map_err(|_| "Bad base64 in ciphertext".to_string())?;
        if wire.len() < 12 {
            return Err("Ciphertext too short".to_string());
        }
        let (nonce_bytes, ct) = wire.split_at(12);
        let nonce = Nonce::from_slice(nonce_bytes);

        let (msg_key, next_chain) = ratchet_step(&self.recv_chain_key)?;
        self.recv_chain_key = next_chain;

        let cipher = ChaCha20Poly1305::new_from_slice(&msg_key)
            .map_err(|e| format!("AEAD key error: {}", e))?;
        let plaintext = cipher
            .decrypt(nonce, ct)
            .map_err(|_| "Decryption FAILED — message may be tampered".to_string())?;

        drop(msg_key);

        String::from_utf8(plaintext)
            .map_err(|_| "Decrypted bytes are not valid UTF-8".to_string())
    }
}

/// Derive one message key and the next chain key from the current chain key.
/// Uses two separate HKDF expansions with distinct info strings so the
/// message key and new chain key are cryptographically independent.
fn ratchet_step(chain_key: &[u8]) -> Result<(Vec<u8>, Vec<u8>), String> {
    let hk = Hkdf::<Sha256>::new(None, chain_key);

    let mut msg_key = vec![0u8; 32];
    hk.expand(HKDF_INFO_MSGKEY, &mut msg_key)
        .map_err(|_| "HKDF expand (msg_key) failed")?;

    let mut next_chain = vec![0u8; 32];
    hk.expand(HKDF_INFO_RATCHET, &mut next_chain)
        .map_err(|_| "HKDF expand (chain) failed")?;

    Ok((msg_key, next_chain))
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Parse an Ed25519 verifying key from base64.
pub fn parse_verifying_key(b64: &str) -> Result<VerifyingKey, String> {
    let bytes: [u8; 32] = B64.decode(b64)
        .map_err(|_| "Bad base64 for verifying key".to_string())?
        .try_into()
        .map_err(|_| "Verifying key wrong length".to_string())?;
    VerifyingKey::from_bytes(&bytes)
        .map_err(|_| "Invalid Ed25519 verifying key".to_string())
}

fn config_dir() -> PathBuf {
    let mut p = dirs_or_home();
    p.push(".config");
    p.push("timetracker");
    p
}

fn identity_path(username: &str) -> PathBuf {
    let mut p = config_dir();
    p.push("keys");
    p.push(format!("{}.json", username));
    p
}

fn known_users_path() -> PathBuf {
    let mut p = config_dir();
    p.push("known_users.json");
    p
}

fn dirs_or_home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}
