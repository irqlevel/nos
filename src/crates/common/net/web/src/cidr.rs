//! IPv4 addresses and networks as an allowlist names them: `a.b.c.d`, or
//! `a.b.c.d/n`. Addresses are in host byte order, as kcore's TCP gives a
//! peer's.

/// A network: the addresses whose first `n` bits are its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cidr {
    net: u32,
    mask: u32,
}

impl Cidr {
    /// `a.b.c.d/n` (n 0 to 32), or an address alone (/32). Bits past the
    /// prefix are dropped: 10.1.2.3/8 is 10.0.0.0/8.
    pub fn parse(s: &str) -> Option<Cidr> {
        let (addr, bits) = match s.split_once('/') {
            Some((a, n)) => (a, decimal(n, 32)?),
            None => (s, 32),
        };
        let ip = parse_ipv4(addr)?;
        let mask = if bits == 0 { 0 } else { u32::MAX << (32 - bits) };
        Some(Cidr { net: ip & mask, mask })
    }

    pub fn contains(&self, ip: u32) -> bool {
        ip & self.mask == self.net
    }

    /// The network's address and prefix length.
    pub fn parts(&self) -> (u32, u32) {
        (self.net, self.mask.count_ones())
    }
}

/// A dotted quad: four numbers 0 to 255, digits only, none with a leading
/// zero (which some read as octal).
pub fn parse_ipv4(s: &str) -> Option<u32> {
    let mut ip = 0u32;
    let mut parts = 0;
    for part in s.split('.') {
        parts += 1;
        if parts > 4 {
            return None;
        }
        ip = (ip << 8) | decimal(part, 255)?;
    }
    if parts == 4 { Some(ip) } else { None }
}

/// A decimal number from 0 to `max`, 1 to 3 digits, no leading zero.
fn decimal(s: &str, max: u32) -> Option<u32> {
    let b = s.as_bytes();
    if b.is_empty() || b.len() > 3 || (b.len() > 1 && b[0] == b'0') || !b.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let n = b.iter().fold(0u32, |n, d| n * 10 + u32::from(d - b'0'));
    if n <= max { Some(n) } else { None }
}
