//! A self-signed X.509 certificate (RFC 5280) for a P-256 key: what nos's
//! host API presents, and what a client of it can be given. Its key in
//! PKCS#8 (RFC 5958, an ECPrivateKey of RFC 5915 inside), PEM both ways,
//! the SHA-256 fingerprints a client pins -- the whole certificate's, and
//! its SubjectPublicKeyInfo's -- and that SubjectPublicKeyInfo found in a
//! certificate someone else made.
//!
//! The DER is written here, a TLV at a time, rather than through a general
//! encoder: a certificate of one shape, every byte of it in this file.

use alloc::string::String;
use alloc::vec::Vec;
use base64ct::{Base64, Encoding};
use p256::ecdsa::signature::Signer;
use p256::ecdsa::{Signature, SigningKey};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

/* Object identifiers, as DER has them (tag and length included) */
/// ecdsa-with-SHA256, 1.2.840.10045.4.3.2
const ECDSA_SHA256: &[u8] = &[0x06, 0x08, 0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x04, 0x03, 0x02];
/// id-ecPublicKey, 1.2.840.10045.2.1
const EC_PUBLIC_KEY: &[u8] = &[0x06, 0x07, 0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x02, 0x01];
/// prime256v1, 1.2.840.10045.3.1.7
const PRIME256V1: &[u8] = &[0x06, 0x08, 0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x03, 0x01, 0x07];
/// commonName, 2.5.4.3
const COMMON_NAME: &[u8] = &[0x06, 0x03, 0x55, 0x04, 0x03];
/// basicConstraints, keyUsage, extKeyUsage, subjectAltName: 2.5.29.19, 15, 37, 17
const BASIC_CONSTRAINTS: &[u8] = &[0x06, 0x03, 0x55, 0x1D, 0x13];
const KEY_USAGE: &[u8] = &[0x06, 0x03, 0x55, 0x1D, 0x0F];
const EXT_KEY_USAGE: &[u8] = &[0x06, 0x03, 0x55, 0x1D, 0x25];
const SUBJECT_ALT_NAME: &[u8] = &[0x06, 0x03, 0x55, 0x1D, 0x11];
/// id-kp-serverAuth and id-kp-clientAuth, 1.3.6.1.5.5.7.3.1 and .2
const SERVER_AUTH: &[u8] = &[0x06, 0x08, 0x2B, 0x06, 0x01, 0x05, 0x05, 0x07, 0x03, 0x01];
const CLIENT_AUTH: &[u8] = &[0x06, 0x08, 0x2B, 0x06, 0x01, 0x05, 0x05, 0x07, 0x03, 0x02];

const SEQUENCE: u8 = 0x30;
const SET: u8 = 0x31;
const INTEGER: u8 = 0x02;
const BIT_STRING: u8 = 0x03;
const OCTET_STRING: u8 = 0x04;
const BOOLEAN: u8 = 0x01;
const UTF8_STRING: u8 = 0x0C;
const UTC_TIME: u8 = 0x17;
const GENERALIZED_TIME: u8 = 0x18;

/// What the certificate is for: a server's (the host API's) or a client's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Usage {
    Server,
    Client,
}

/// What to make a certificate of.
pub struct Request<'a> {
    /// Its subject's and its issuer's common name.
    pub name: &'a str,
    /// The addresses and names it is for (a server's subjectAltName).
    pub ips: &'a [[u8; 4]],
    pub dns: &'a [&'a str],
    /// Random bytes for its serial number.
    pub serial: [u8; 16],
    /// When it is valid from and until, in seconds since 1970.
    pub not_before: u64,
    pub not_after: u64,
    pub usage: Usage,
}

/// A key and the certificate made for it, both DER.
pub struct Identity {
    pub key: Zeroizing<Vec<u8>>,
    pub cert: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The 32 bytes are no P-256 key -- zero, or past the group's order:
    /// draw others.
    BadKey,
    /// A date before 1970 or after 9999, or `not_after` before
    /// `not_before`.
    BadTime,
    /// A name too long for the certificate, or empty.
    BadName,
    NoMemory,
}

/// A certificate for the key `secret` (a P-256 scalar, big-endian), signed
/// by that key, and the key in PKCS#8.
pub fn self_signed(secret: &[u8; 32], req: &Request) -> Result<Identity, Error> {
    let signer = SigningKey::from_bytes(secret.into()).map_err(|_| Error::BadKey)?;
    let point = signer.verifying_key().to_encoded_point(false);
    let point = point.as_bytes();
    if req.name.is_empty() || req.name.len() > 64 || req.dns.iter().any(|d| d.is_empty() || d.len() > 253) {
        return Err(Error::BadName);
    }
    if req.not_after < req.not_before {
        return Err(Error::BadTime);
    }

    let mut tbs = Vec::new();
    {
        let mut body = Vec::new();
        /* version: v3 */
        tlv(&mut body, 0xA0, &[INTEGER, 1, 2])?;
        let mut serial = req.serial;
        /* Positive, and no leading zero to strip: 16 bytes as given */
        serial[0] = (serial[0] & 0x7F) | 0x40;
        tlv(&mut body, INTEGER, &serial)?;
        tlv(&mut body, SEQUENCE, ECDSA_SHA256)?;
        let name = name(req.name)?;
        put(&mut body, &name)?;
        let mut validity = Vec::new();
        time(&mut validity, req.not_before)?;
        time(&mut validity, req.not_after)?;
        tlv(&mut body, SEQUENCE, &validity)?;
        put(&mut body, &name)?;
        let spki = spki(point)?;
        put(&mut body, &spki)?;
        let extensions = extensions(req)?;
        let mut wrapped = Vec::new();
        tlv(&mut wrapped, SEQUENCE, &extensions)?;
        tlv(&mut body, 0xA3, &wrapped)?;
        tlv(&mut tbs, SEQUENCE, &body)?;
    }

    /* RFC 6979: no randomness in the signature, past the key's */
    let signature: Signature = signer.sign(&tbs);
    let (r, s) = signature.split_bytes();
    let mut ints = Vec::new();
    integer(&mut ints, &r)?;
    integer(&mut ints, &s)?;
    let mut sig = Vec::new();
    tlv(&mut sig, SEQUENCE, &ints)?;

    let mut cert_body = Vec::new();
    put(&mut cert_body, &tbs)?;
    tlv(&mut cert_body, SEQUENCE, ECDSA_SHA256)?;
    bit_string(&mut cert_body, &sig)?;
    let mut cert = Vec::new();
    tlv(&mut cert, SEQUENCE, &cert_body)?;

    Ok(Identity { key: pkcs8(secret, point)?, cert })
}

/// The extensions: a CA no more; signing only; for a server or a client;
/// and a server's addresses and names.
fn extensions(req: &Request) -> Result<Vec<u8>, Error> {
    let mut all = Vec::new();
    /* basicConstraints, critical: cA false (the default, so empty) */
    extension(&mut all, BASIC_CONSTRAINTS, true, &[SEQUENCE, 0])?;
    /* keyUsage, critical: digitalSignature, bit 0 */
    extension(&mut all, KEY_USAGE, true, &[BIT_STRING, 2, 7, 0x80])?;
    let mut eku = Vec::new();
    tlv(&mut eku, SEQUENCE, if req.usage == Usage::Server { SERVER_AUTH } else { CLIENT_AUTH })?;
    extension(&mut all, EXT_KEY_USAGE, false, &eku)?;
    if !req.ips.is_empty() || !req.dns.is_empty() {
        let mut names = Vec::new();
        for ip in req.ips {
            /* [7] iPAddress */
            tlv(&mut names, 0x87, ip)?;
        }
        for d in req.dns {
            /* [2] dNSName, IA5String */
            if !d.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'.') {
                return Err(Error::BadName);
            }
            tlv(&mut names, 0x82, d.as_bytes())?;
        }
        let mut san = Vec::new();
        tlv(&mut san, SEQUENCE, &names)?;
        extension(&mut all, SUBJECT_ALT_NAME, false, &san)?;
    }
    Ok(all)
}

fn extension(out: &mut Vec<u8>, oid: &[u8], critical: bool, value: &[u8]) -> Result<(), Error> {
    let mut e = Vec::new();
    put(&mut e, oid)?;
    if critical {
        put(&mut e, &[BOOLEAN, 1, 0xFF])?;
    }
    tlv(&mut e, OCTET_STRING, value)?;
    tlv(out, SEQUENCE, &e)
}

/// A Name of one common name.
fn name(cn: &str) -> Result<Vec<u8>, Error> {
    let mut atv = Vec::new();
    put(&mut atv, COMMON_NAME)?;
    tlv(&mut atv, UTF8_STRING, cn.as_bytes())?;
    let mut seq = Vec::new();
    tlv(&mut seq, SEQUENCE, &atv)?;
    let mut set = Vec::new();
    tlv(&mut set, SET, &seq)?;
    let mut name = Vec::new();
    tlv(&mut name, SEQUENCE, &set)?;
    Ok(name)
}

/// The SubjectPublicKeyInfo of a P-256 key's uncompressed point.
pub fn spki(point: &[u8]) -> Result<Vec<u8>, Error> {
    let mut alg = Vec::new();
    put(&mut alg, EC_PUBLIC_KEY)?;
    put(&mut alg, PRIME256V1)?;
    let mut body = Vec::new();
    tlv(&mut body, SEQUENCE, &alg)?;
    bit_string(&mut body, point)?;
    let mut out = Vec::new();
    tlv(&mut out, SEQUENCE, &body)?;
    Ok(out)
}

/// PKCS#8's PrivateKeyInfo of an ECPrivateKey, the public point in it.
fn pkcs8(secret: &[u8; 32], point: &[u8]) -> Result<Zeroizing<Vec<u8>>, Error> {
    let mut ec = Zeroizing::new(Vec::new());
    put(&mut ec, &[INTEGER, 1, 1])?;
    tlv(&mut ec, OCTET_STRING, secret)?;
    let mut public = Vec::new();
    bit_string(&mut public, point)?;
    tlv(&mut ec, 0xA1, &public)?;
    let mut ec_key = Zeroizing::new(Vec::new());
    tlv(&mut ec_key, SEQUENCE, &ec)?;

    let mut alg = Vec::new();
    put(&mut alg, EC_PUBLIC_KEY)?;
    put(&mut alg, PRIME256V1)?;
    let mut info = Zeroizing::new(Vec::new());
    put(&mut info, &[INTEGER, 1, 0])?;
    tlv(&mut info, SEQUENCE, &alg)?;
    tlv(&mut info, OCTET_STRING, &ec_key)?;
    let mut out = Zeroizing::new(Vec::new());
    tlv(&mut out, SEQUENCE, &info)?;
    Ok(out)
}

/// A time as RFC 5280 4.1.2.5 has it: UTCTime through 2049, GeneralizedTime
/// from 2050, to the second, in UTC.
fn time(out: &mut Vec<u8>, secs: u64) -> Result<(), Error> {
    const MAX_SECS: u64 = 253_402_300_799; /* 9999-12-31 23:59:59 */
    if secs > MAX_SECS {
        return Err(Error::BadTime);
    }
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (y, m, d) = civil(days);
    let (hh, mm, ss) = (rem / 3600, rem / 60 % 60, rem % 60);
    let mut text = [0u8; 15];
    let mut n = 0;
    let mut two = |v: u64, text: &mut [u8; 15]| {
        text[n] = b'0' + (v / 10 % 10) as u8;
        text[n + 1] = b'0' + (v % 10) as u8;
        n += 2;
    };
    let y = y as u64;
    let tag = if y < 2050 {
        two(y % 100, &mut text);
        UTC_TIME
    } else {
        two(y / 100, &mut text);
        two(y % 100, &mut text);
        GENERALIZED_TIME
    };
    for v in [u64::from(m), u64::from(d), hh, mm, ss] {
        two(v, &mut text);
    }
    text[n] = b'Z';
    tlv(out, tag, &text[..n + 1])
}

/// The calendar date `days` after 1970-01-01 (H. Hinnant's
/// civil_from_days): year, month 1-12, day 1-31.
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

/// An INTEGER of an unsigned big-endian number: leading zeros dropped, one
/// put back where the top bit would make it negative.
fn integer(out: &mut Vec<u8>, be: &[u8]) -> Result<(), Error> {
    let start = be.iter().position(|&b| b != 0).unwrap_or(be.len().saturating_sub(1));
    let digits = &be[start..];
    let mut v = Vec::new();
    if digits.first().is_some_and(|&b| b & 0x80 != 0) {
        put(&mut v, &[0])?;
    }
    put(&mut v, digits)?;
    tlv(out, INTEGER, &v)
}

fn bit_string(out: &mut Vec<u8>, bytes: &[u8]) -> Result<(), Error> {
    let mut v = Vec::new();
    v.try_reserve(bytes.len() + 1).map_err(|_| Error::NoMemory)?;
    v.push(0);
    v.extend_from_slice(bytes);
    tlv(out, BIT_STRING, &v)
}

/// A tag, its content's length as DER has it -- short below 128, else
/// long, in the fewest bytes -- and the content.
fn tlv(out: &mut Vec<u8>, tag: u8, content: &[u8]) -> Result<(), Error> {
    let n = content.len();
    let mut len = [0u8; 5];
    let len_len = if n < 0x80 {
        len[0] = n as u8;
        1
    } else {
        let bytes = (n as u32).to_be_bytes();
        let skip = bytes.iter().take_while(|&&b| b == 0).count();
        len[0] = 0x80 | (4 - skip) as u8;
        len[1..1 + 4 - skip].copy_from_slice(&bytes[skip..]);
        1 + 4 - skip
    };
    out.try_reserve(1 + len_len + n).map_err(|_| Error::NoMemory)?;
    out.push(tag);
    out.extend_from_slice(&len[..len_len]);
    out.extend_from_slice(content);
    Ok(())
}

fn put(out: &mut Vec<u8>, b: &[u8]) -> Result<(), Error> {
    out.try_reserve(b.len()).map_err(|_| Error::NoMemory)?;
    out.extend_from_slice(b);
    Ok(())
}

/// `der` as PEM: its label's lines around its base64 in lines of 64.
pub fn pem(label: &str, der: &[u8]) -> Result<String, Error> {
    let encoded_len = Base64::encoded_len(der);
    let lines = encoded_len.div_ceil(64);
    let mut out = String::new();
    out.try_reserve(2 * (label.len() + 17) + encoded_len + lines).map_err(|_| Error::NoMemory)?;
    out.push_str("-----BEGIN ");
    out.push_str(label);
    out.push_str("-----\n");
    let mut buf = Vec::new();
    buf.try_reserve(encoded_len).map_err(|_| Error::NoMemory)?;
    buf.resize(encoded_len, 0);
    let text = Base64::encode(der, &mut buf).map_err(|_| Error::NoMemory)?;
    let mut rest = text;
    while !rest.is_empty() {
        let n = rest.len().min(64);
        out.push_str(&rest[..n]);
        out.push('\n');
        rest = &rest[n..];
    }
    out.push_str("-----END ");
    out.push_str(label);
    out.push_str("-----\n");
    Ok(out)
}

/// The DER of the first PEM block labelled `label` in `text`.
pub fn pem_decode(label: &str, text: &str) -> Option<Vec<u8>> {
    let mut begin = String::new();
    begin.try_reserve(label.len() + 16).ok()?;
    begin.push_str("-----BEGIN ");
    begin.push_str(label);
    begin.push_str("-----");
    let mut end = String::new();
    end.try_reserve(label.len() + 14).ok()?;
    end.push_str("-----END ");
    end.push_str(label);
    end.push_str("-----");
    let start = text.find(&begin)? + begin.len();
    let stop = start + text[start..].find(&end)?;
    let mut b64 = Vec::new();
    b64.try_reserve(stop - start).ok()?;
    b64.extend(text[start..stop].bytes().filter(|c| !c.is_ascii_whitespace()));
    let mut der = Vec::new();
    der.try_reserve(b64.len()).ok()?;
    der.resize(b64.len(), 0);
    let n = Base64::decode(&b64, &mut der).ok()?.len();
    der.truncate(n);
    Some(der)
}

/// SHA-256 of `der`: a certificate's fingerprint, or a key's pin of its
/// SubjectPublicKeyInfo.
pub fn sha256(der: &[u8]) -> [u8; 32] {
    Sha256::digest(der).into()
}

/// The SubjectPublicKeyInfo of a certificate, as it lies in it -- found by
/// walking the DER of whatever made it: the TBSCertificate, past its
/// version, serial, signature algorithm, issuer, validity and subject.
pub fn find_spki(cert: &[u8]) -> Option<&[u8]> {
    let (tag, cert_body, _) = read_tlv(cert)?;
    if tag != SEQUENCE {
        return None;
    }
    let (tag, tbs, _) = read_tlv(cert_body)?;
    if tag != SEQUENCE {
        return None;
    }
    let mut rest = tbs;
    /* version, if it is there */
    let (tag, _, after) = read_tlv(rest)?;
    if tag == 0xA0 {
        rest = after;
    }
    /* serial, signature, issuer, validity, subject */
    let expected = [INTEGER, SEQUENCE, SEQUENCE, SEQUENCE, SEQUENCE];
    for want in expected {
        let (tag, _, after) = read_tlv(rest)?;
        if tag != want {
            return None;
        }
        rest = after;
    }
    let (tag, _, after) = read_tlv(rest)?;
    if tag != SEQUENCE {
        return None;
    }
    Some(&rest[..rest.len() - after.len()])
}

/// The TLV at the head of `b`: its tag, its content, and what follows it.
/// Single-byte tags, definite lengths in their shortest form only.
fn read_tlv(b: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let (&tag, rest) = b.split_first()?;
    if tag & 0x1F == 0x1F {
        return None;
    }
    let (&first, rest) = rest.split_first()?;
    let (len, rest) = if first < 0x80 {
        (usize::from(first), rest)
    } else {
        let n = usize::from(first & 0x7F);
        if n == 0 || n > 4 || rest.len() < n || rest[0] == 0 {
            return None;
        }
        let len = rest[..n].iter().fold(0usize, |l, &d| (l << 8) | usize::from(d));
        if len < 0x80 {
            return None;
        }
        (len, &rest[n..])
    };
    if rest.len() < len {
        return None;
    }
    Some((tag, &rest[..len], &rest[len..]))
}

/// A fingerprint as OpenSSL prints one: hex pairs, upper case, colons
/// between.
pub fn fingerprint_hex(digest: &[u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut s = String::new();
    if s.try_reserve(32 * 3).is_err() {
        return s;
    }
    for (i, b) in digest.iter().enumerate() {
        if i > 0 {
            s.push(':');
        }
        s.push(char::from(HEX[usize::from(b >> 4)]));
        s.push(char::from(HEX[usize::from(b & 15)]));
    }
    s
}
