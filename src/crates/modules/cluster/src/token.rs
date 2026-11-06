//! An invitation's token: what `cluster invite` prints, once, and `cluster
//! join` takes on the new host -- the cluster's id, the inviting member's
//! name, address, port and key, the invited host's name, the invitation's
//! number and its secret. The joiner's ipcnet pins the member's key from
//! it, as the member's pinned the joiner's from the invitation, and the
//! joiner proves the secret over that connection; the registry keeps the
//! secret's hash, never the secret (plans/cluster.md, "Joining").
//!
//! Written `nos1-` and the bytes in hex: one word, for a shell to take.

use alloc::string::String;
use alloc::vec::Vec;

use sha2::{Digest, Sha256};

use crate::files::hex;

const PREFIX: &str = "nos1-";
/// The fixed part: cluster, address, port, key, invitation, secret.
const FIXED: usize = 16 + 4 + 2 + 32 + 8 + 32;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Token {
    pub cluster: [u8; 16],
    pub member: String,
    pub ip: u32,
    pub port: u16,
    pub key: [u8; 32],
    pub host: String,
    pub invitation: u64,
    pub secret: [u8; 32],
}

impl Token {
    pub fn encode(&self) -> String {
        let mut b = Vec::with_capacity(FIXED + 2 + self.member.len() + self.host.len());
        b.extend_from_slice(&self.cluster);
        b.extend_from_slice(&self.ip.to_be_bytes());
        b.extend_from_slice(&self.port.to_be_bytes());
        b.extend_from_slice(&self.key);
        b.extend_from_slice(&self.invitation.to_be_bytes());
        b.extend_from_slice(&self.secret);
        for name in [&self.member, &self.host] {
            b.push(name.len() as u8);
            b.extend_from_slice(name.as_bytes());
        }
        let mut s = String::from(PREFIX);
        s.push_str(&hex(&b));
        s
    }

    pub fn decode(text: &str) -> Option<Token> {
        let digits = text.strip_prefix(PREFIX)?.as_bytes();
        if digits.len() % 2 != 0 || digits.len() / 2 < FIXED + 2 {
            return None;
        }
        let mut b = Vec::with_capacity(digits.len() / 2);
        for pair in digits.chunks(2) {
            let digit = |x: u8| (x as char).to_digit(16);
            b.push((digit(pair[0])? * 16 + digit(pair[1])?) as u8);
        }
        let take = |at: usize, n: usize| b.get(at..at + n);
        let fixed = |at: usize| -> Option<[u8; 32]> { take(at, 32)?.try_into().ok() };
        let cluster: [u8; 16] = take(0, 16)?.try_into().ok()?;
        let ip = u32::from_be_bytes(take(16, 4)?.try_into().ok()?);
        let port = u16::from_be_bytes(take(20, 2)?.try_into().ok()?);
        let key = fixed(22)?;
        let invitation = u64::from_be_bytes(take(54, 8)?.try_into().ok()?);
        let secret = fixed(62)?;
        let mut at = FIXED;
        let mut names = [String::new(), String::new()];
        for name in names.iter_mut() {
            let n = usize::from(*b.get(at)?);
            let text = core::str::from_utf8(take(at + 1, n)?).ok()?;
            if !registry::valid_name(text) {
                return None;
            }
            *name = String::from(text);
            at += 1 + n;
        }
        if at != b.len() || ip == 0 {
            return None;
        }
        let [member, host] = names;
        Some(Token { cluster, member, ip, port, key, host, invitation, secret })
    }
}

/// What the registry keeps of a secret.
pub fn secret_hash(secret: &[u8; 32]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(b"nos cluster invitation");
    h.update(secret);
    h.finalize().into()
}
