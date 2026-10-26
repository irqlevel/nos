//! Keys: the server's own, the ones allowed to log in, and the forms each
//! is written in -- the blobs on the wire (RFC 8709), authorized_keys lines,
//! OpenSSH's private key file, fingerprints.

use alloc::string::String;
use alloc::vec::Vec;
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use sha2::{Digest, Sha256};

use crate::wire::{put_string, put_u32, Reader};

/// The one type of key the server knows, by its SSH name.
pub const ED25519: &str = "ssh-ed25519";

const PUBLIC_LEN: usize = 32;
const SIGNATURE_LEN: usize = 64;

/// OpenSSH's private key file (PROTOCOL.key): its armour, its magic, the
/// block its private section is padded to and the column its base64 is
/// wrapped at -- as ssh-keygen writes them.
const OPENSSH_BEGIN: &str = "-----BEGIN OPENSSH PRIVATE KEY-----";
const OPENSSH_END: &str = "-----END OPENSSH PRIVATE KEY-----";
const OPENSSH_MAGIC: &[u8] = b"openssh-key-v1\0";
const OPENSSH_BLOCK: usize = 8;
const OPENSSH_WRAP: usize = 70;

/// The server's key: an Ed25519 key pair, made from its 32-byte seed.
pub struct HostKey {
    signing: SigningKey,
    public: PublicKey,
}

impl HostKey {
    pub fn from_seed(seed: &[u8; 32]) -> Self {
        let signing = SigningKey::from_bytes(seed);
        let public = PublicKey { key: signing.verifying_key().to_bytes() };
        Self { signing, public }
    }

    pub fn public(&self) -> &PublicKey {
        &self.public
    }

    /// A signature blob over `data`: string "ssh-ed25519", string the 64
    /// bytes of the signature (RFC 8709 6).
    pub fn sign(&self, data: &[u8]) -> Vec<u8> {
        let signature = self.signing.sign(data).to_bytes();
        let mut out = Vec::with_capacity(8 + ED25519.len() + SIGNATURE_LEN);
        put_string(&mut out, ED25519.as_bytes());
        put_string(&mut out, &signature);
        out
    }

    /// The key as an OpenSSH private key file ("openssh-key-v1"), with no
    /// passphrase: what ssh-keygen reads and writes, so a host key can move
    /// between this server and an OpenSSH one. `check` is the file's check
    /// word: any random number.
    pub fn to_openssh(&self, comment: &str, check: u32) -> String {
        let mut private = Vec::new();
        put_u32(&mut private, check);
        put_u32(&mut private, check);
        put_string(&mut private, ED25519.as_bytes());
        put_string(&mut private, &self.public.key);
        let mut pair = [0u8; 64];
        pair[..32].copy_from_slice(&self.signing.to_bytes());
        pair[32..].copy_from_slice(&self.public.key);
        put_string(&mut private, &pair);
        put_string(&mut private, comment.as_bytes());
        let mut pad = 1u8;
        while private.len() % OPENSSH_BLOCK != 0 {
            private.push(pad);
            pad += 1;
        }

        let mut blob = Vec::new();
        blob.extend_from_slice(OPENSSH_MAGIC);
        put_string(&mut blob, b"none"); /* cipher */
        put_string(&mut blob, b"none"); /* key derivation */
        put_string(&mut blob, b""); /* its options */
        put_u32(&mut blob, 1); /* keys */
        put_string(&mut blob, &self.public.blob());
        put_string(&mut blob, &private);

        let text = base64_encode(&blob, true);
        let mut out = String::new();
        out.push_str(OPENSSH_BEGIN);
        out.push('\n');
        for line in text.as_bytes().chunks(OPENSSH_WRAP) {
            /* base64 is ASCII */
            out.push_str(core::str::from_utf8(line).unwrap_or(""));
            out.push('\n');
        }
        out.push_str(OPENSSH_END);
        out.push('\n');
        out
    }

    /// The key in an OpenSSH private key file -- ssh-keygen -t ed25519's,
    /// made without a passphrase -- or what is wrong with the file.
    pub fn from_openssh(text: &[u8]) -> core::result::Result<Self, &'static str> {
        const TRUNCATED: &str = "truncated";

        let text = core::str::from_utf8(text).map_err(|_| "not text")?;
        let start = text.find(OPENSSH_BEGIN).ok_or("no BEGIN OPENSSH PRIVATE KEY line")? + OPENSSH_BEGIN.len();
        let end = start + text[start..].find(OPENSSH_END).ok_or("no END OPENSSH PRIVATE KEY line")?;
        let armoured: Vec<u8> = text[start..end].bytes().filter(|b| !b.is_ascii_whitespace()).collect();
        let blob = base64_decode(&armoured).ok_or("bad base64")?;
        let rest = blob.strip_prefix(OPENSSH_MAGIC).ok_or("not an openssh-key-v1 key")?;

        let mut r = Reader::new(rest);
        let cipher = r.string().ok_or(TRUNCATED)?;
        let kdf = r.string().ok_or(TRUNCATED)?;
        r.string().ok_or(TRUNCATED)?;
        if cipher != b"none" || kdf != b"none" {
            return Err("encrypted with a passphrase");
        }
        if r.u32().ok_or(TRUNCATED)? != 1 {
            return Err("more than one key in it");
        }
        r.string().ok_or(TRUNCATED)?;
        let private = r.string().ok_or(TRUNCATED)?;

        let mut p = Reader::new(private);
        if p.u32().ok_or(TRUNCATED)? != p.u32().ok_or(TRUNCATED)? {
            return Err("its check words differ: damaged");
        }
        if p.string().ok_or(TRUNCATED)? != ED25519.as_bytes() {
            return Err("not an ssh-ed25519 key");
        }
        let public = p.string().ok_or(TRUNCATED)?;
        let pair = p.string().ok_or(TRUNCATED)?;
        if public.len() != PUBLIC_LEN || pair.len() != 2 * PUBLIC_LEN || &pair[PUBLIC_LEN..] != public {
            return Err("damaged");
        }

        let mut seed = [0u8; 32];
        seed.copy_from_slice(&pair[..32]);
        let key = HostKey::from_seed(&seed);
        if key.public.key[..] != *public {
            return Err("its public key is not the private key's");
        }
        Ok(key)
    }
}

/// An ssh-ed25519 public key.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct PublicKey {
    pub key: [u8; PUBLIC_LEN],
}

impl PublicKey {
    /// The key blob: string "ssh-ed25519", string the 32 bytes of the key.
    pub fn blob(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(8 + ED25519.len() + PUBLIC_LEN);
        put_string(&mut out, ED25519.as_bytes());
        put_string(&mut out, &self.key);
        out
    }

    pub fn from_blob(blob: &[u8]) -> Option<Self> {
        let mut r = Reader::new(blob);
        if r.string()? != ED25519.as_bytes() {
            return None;
        }
        let key = r.string()?;
        if key.len() != PUBLIC_LEN || !r.done() {
            return None;
        }
        let mut out = Self { key: [0u8; PUBLIC_LEN] };
        out.key.copy_from_slice(key);
        Some(out)
    }

    /// Whether `sig_blob` is this key's signature over `data`. Strict
    /// verification: no small-order key, no signature with a second form.
    pub fn verify(&self, data: &[u8], sig_blob: &[u8]) -> bool {
        let mut r = Reader::new(sig_blob);
        let signature = match (r.string(), r.string()) {
            (Some(alg), Some(sig)) if alg == ED25519.as_bytes() && sig.len() == SIGNATURE_LEN && r.done() => sig,
            _ => return false,
        };
        let key = match VerifyingKey::from_bytes(&self.key) {
            Ok(key) => key,
            Err(_) => return false,
        };
        let mut bytes = [0u8; SIGNATURE_LEN];
        bytes.copy_from_slice(signature);
        key.verify_strict(data, &Signature::from_bytes(&bytes)).is_ok()
    }

    /// SHA256:<base64>, as `ssh-keygen -l` shows it and as a client asks
    /// about it the first time it connects.
    pub fn fingerprint(&self) -> String {
        let digest = Sha256::digest(self.blob());
        let mut out = String::from("SHA256:");
        out.push_str(&base64_encode(&digest, false));
        out
    }

    /// "ssh-ed25519 AAAA...": the key as an authorized_keys line has it.
    pub fn to_line(&self) -> String {
        let mut out = String::from(ED25519);
        out.push(' ');
        out.push_str(&base64_encode(&self.blob(), true));
        out
    }
}

/// A key allowed to log in: a line of an authorized_keys file.
#[derive(Clone)]
pub struct AuthorizedKey {
    pub key: PublicKey,
    pub comment: String,
}

impl AuthorizedKey {
    /// One line of an authorized_keys file: a key, None for a blank line or
    /// a comment, and an error for anything else -- another type of key, or
    /// options in front of one, which this server would not honour and so
    /// will not pretend to take.
    pub fn parse(line: &str) -> core::result::Result<Option<Self>, &'static str> {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            return Ok(None);
        }

        let mut words = line.split_ascii_whitespace();
        let kind = words.next().unwrap_or("");
        if kind != ED25519 {
            if kind.starts_with("ssh-") || kind.starts_with("ecdsa-") || kind.starts_with("sk-") {
                return Err("not an ssh-ed25519 key");
            }
            return Err("options in front of the key, or no key at all -- options are not supported");
        }
        let encoded = words.next().ok_or("no key after ssh-ed25519")?;
        let blob = base64_decode(encoded.as_bytes()).ok_or("bad base64")?;
        let key = PublicKey::from_blob(&blob).ok_or("not an ssh-ed25519 key blob")?;

        let mut comment = String::new();
        for word in words {
            if !comment.is_empty() {
                comment.push(' ');
            }
            comment.push_str(word);
        }
        Ok(Some(Self { key, comment }))
    }

    /// The key as a line of an authorized_keys file.
    pub fn to_line(&self) -> String {
        let mut out = self.key.to_line();
        if !self.comment.is_empty() {
            out.push(' ');
            out.push_str(&self.comment);
        }
        out
    }
}

const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Base64 (RFC 4648), padded with '=' or not: a fingerprint is not.
pub fn base64_encode(data: &[u8], pad: bool) -> String {
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    for chunk in data.chunks(3) {
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        let n = (chunk[0] as u32) << 16 | (b1 as u32) << 8 | b2 as u32;
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(BASE64[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else if pad {
                out.push('=');
            }
        }
    }
    out
}

pub fn base64_decode(text: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let mut acc = 0u32;
    let mut bits = 0u32;
    let mut padding = 0;
    for &c in text {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => {
                padding += 1;
                continue;
            }
            _ => return None,
        };
        if padding != 0 {
            return None;
        }
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    if padding > 2 || bits >= 6 {
        return None;
    }
    Some(out)
}
