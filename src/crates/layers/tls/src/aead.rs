//! TLS's ChaCha20-Poly1305 suites on the crypto crate -- CRYPTOGAMS'
//! assembly, in the kernel's SIMD sections -- where rustls-rustcrypto's run
//! on RustCrypto's portable code: rustls's AEAD traits over
//! `crypto::ChaCha20Poly1305`, and the three suites -- TLS 1.3's and TLS
//! 1.2's two -- as rustls-rustcrypto has them but for the AEAD: the same
//! hash, HKDF, PRF, key exchange and signatures, which [`provider`] swaps
//! in for its own.
//!
//! [`provider`]: crate::provider

use alloc::boxed::Box;

use crypto::{ChaCha20Poly1305 as Key, KEY_LEN, NONCE_LEN, TAG_LEN};
use rustls::crypto::cipher::{
    self, AeadKey, InboundOpaqueMessage, InboundPlainMessage, Iv, KeyBlockShape, MessageDecrypter,
    MessageEncrypter, OutboundOpaqueMessage, OutboundPlainMessage, PrefixedPayload, Tls12AeadAlgorithm,
    Tls13AeadAlgorithm, UnsupportedOperationError,
};
use rustls::{
    CipherSuite, CipherSuiteCommon, ConnectionTrafficSecrets, ContentType, ProtocolVersion, SupportedCipherSuite,
    Tls12CipherSuite, Tls13CipherSuite,
};

/// rustls-rustcrypto's suites, whose every part but the AEAD these keep.
const RUSTCRYPTO_TLS13: &Tls13CipherSuite = match rustls_rustcrypto::TLS13_CHACHA20_POLY1305_SHA256 {
    SupportedCipherSuite::Tls13(suite) => suite,
    SupportedCipherSuite::Tls12(_) => panic!("rustls-rustcrypto's TLS 1.3 ChaCha20-Poly1305 suite is not TLS 1.3's"),
};
const RUSTCRYPTO_ECDSA: &Tls12CipherSuite = match rustls_rustcrypto::TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256 {
    SupportedCipherSuite::Tls12(suite) => suite,
    SupportedCipherSuite::Tls13(_) => panic!("rustls-rustcrypto's ECDSA ChaCha20-Poly1305 suite is not TLS 1.2's"),
};
const RUSTCRYPTO_RSA: &Tls12CipherSuite = match rustls_rustcrypto::TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256 {
    SupportedCipherSuite::Tls12(suite) => suite,
    SupportedCipherSuite::Tls13(_) => panic!("rustls-rustcrypto's RSA ChaCha20-Poly1305 suite is not TLS 1.2's"),
};

static TLS13_SUITE: Tls13CipherSuite = Tls13CipherSuite {
    common: CipherSuiteCommon {
        suite: CipherSuite::TLS13_CHACHA20_POLY1305_SHA256,
        hash_provider: RUSTCRYPTO_TLS13.common.hash_provider,
        confidentiality_limit: RUSTCRYPTO_TLS13.common.confidentiality_limit,
    },
    hkdf_provider: RUSTCRYPTO_TLS13.hkdf_provider,
    aead_alg: &ChaCha20Poly1305,
    quic: RUSTCRYPTO_TLS13.quic,
};

static ECDSA_SUITE: Tls12CipherSuite = Tls12CipherSuite {
    common: CipherSuiteCommon {
        suite: CipherSuite::TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256,
        hash_provider: RUSTCRYPTO_ECDSA.common.hash_provider,
        confidentiality_limit: RUSTCRYPTO_ECDSA.common.confidentiality_limit,
    },
    prf_provider: RUSTCRYPTO_ECDSA.prf_provider,
    kx: RUSTCRYPTO_ECDSA.kx,
    sign: RUSTCRYPTO_ECDSA.sign,
    aead_alg: &ChaCha20Poly1305,
};

static RSA_SUITE: Tls12CipherSuite = Tls12CipherSuite {
    common: CipherSuiteCommon {
        suite: CipherSuite::TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256,
        hash_provider: RUSTCRYPTO_RSA.common.hash_provider,
        confidentiality_limit: RUSTCRYPTO_RSA.common.confidentiality_limit,
    },
    prf_provider: RUSTCRYPTO_RSA.prf_provider,
    kx: RUSTCRYPTO_RSA.kx,
    sign: RUSTCRYPTO_RSA.sign,
    aead_alg: &ChaCha20Poly1305,
};

/// The suite of the crypto crate's AEAD that takes `suite`'s place, for
/// one of ChaCha20-Poly1305's; None for any other.
pub fn suite_for(suite: CipherSuite) -> Option<SupportedCipherSuite> {
    match suite {
        CipherSuite::TLS13_CHACHA20_POLY1305_SHA256 => Some(SupportedCipherSuite::Tls13(&TLS13_SUITE)),
        CipherSuite::TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256 => Some(SupportedCipherSuite::Tls12(&ECDSA_SUITE)),
        CipherSuite::TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256 => Some(SupportedCipherSuite::Tls12(&RSA_SUITE)),
        _ => None,
    }
}

/// ChaCha20-Poly1305 as rustls asks an AEAD for its records' keys.
struct ChaCha20Poly1305;

/// A record key, or none where rustls handed one of another length than
/// the 32 bytes `key_len` and `key_block_shape` ask for -- then every
/// record is refused, rather than a panic in the kernel.
fn key_of(key: &AeadKey) -> Option<Key> {
    <&[u8; KEY_LEN]>::try_from(key.as_ref()).ok().map(Key::new)
}

impl Tls13AeadAlgorithm for ChaCha20Poly1305 {
    fn encrypter(&self, key: AeadKey, iv: Iv) -> Box<dyn MessageEncrypter> {
        Box::new(Tls13Records { key: key_of(&key), iv })
    }

    fn decrypter(&self, key: AeadKey, iv: Iv) -> Box<dyn MessageDecrypter> {
        Box::new(Tls13Records { key: key_of(&key), iv })
    }

    fn key_len(&self) -> usize {
        KEY_LEN
    }

    fn extract_keys(&self, key: AeadKey, iv: Iv) -> Result<ConnectionTrafficSecrets, UnsupportedOperationError> {
        Ok(ConnectionTrafficSecrets::Chacha20Poly1305 { key, iv })
    }
}

impl Tls12AeadAlgorithm for ChaCha20Poly1305 {
    fn encrypter(&self, key: AeadKey, iv: &[u8], _explicit: &[u8]) -> Box<dyn MessageEncrypter> {
        Box::new(Tls12Records { key: key_of(&key), iv: iv_of(iv) })
    }

    fn decrypter(&self, key: AeadKey, iv: &[u8]) -> Box<dyn MessageDecrypter> {
        Box::new(Tls12Records { key: key_of(&key), iv: iv_of(iv) })
    }

    /// RFC 7905: a 32-byte key and a 12-byte IV, and no explicit nonce --
    /// the record's sequence number XORed into the IV, as TLS 1.3's.
    fn key_block_shape(&self) -> KeyBlockShape {
        KeyBlockShape { enc_key_len: KEY_LEN, fixed_iv_len: NONCE_LEN, explicit_nonce_len: 0 }
    }

    fn extract_keys(
        &self,
        key: AeadKey,
        iv: &[u8],
        _explicit: &[u8],
    ) -> Result<ConnectionTrafficSecrets, UnsupportedOperationError> {
        let iv: [u8; NONCE_LEN] = iv.try_into().map_err(|_| UnsupportedOperationError)?;
        Ok(ConnectionTrafficSecrets::Chacha20Poly1305 { key, iv: Iv::new(iv) })
    }
}

/// The IV of a TLS 1.2 key block -- `fixed_iv_len` bytes, as
/// `key_block_shape` asks -- or zeros where it is not, its key refused
/// then too (`key_of`'s length is the block's own check).
fn iv_of(iv: &[u8]) -> Iv {
    let mut bytes = [0u8; NONCE_LEN];
    if iv.len() == NONCE_LEN {
        bytes.copy_from_slice(iv);
    }
    Iv::new(bytes)
}

/// TLS 1.3's records: the content type inside, the record header the
/// associated data.
struct Tls13Records {
    key: Option<Key>,
    iv: Iv,
}

impl MessageEncrypter for Tls13Records {
    fn encrypt(&mut self, m: OutboundPlainMessage<'_>, seq: u64) -> Result<OutboundOpaqueMessage, rustls::Error> {
        let key = self.key.as_ref().ok_or(rustls::Error::EncryptError)?;
        let total_len = self.encrypted_payload_len(m.payload.len());
        let mut payload = PrefixedPayload::with_capacity(total_len);
        payload.extend_from_chunks(&m.payload);
        payload.extend_from_slice(&m.typ.to_array());
        let nonce = cipher::Nonce::new(&self.iv, seq).0;
        let aad = cipher::make_tls13_aad(total_len);
        let tag = key.seal_in_place(&nonce, &aad, payload.as_mut());
        payload.extend_from_slice(&tag);
        Ok(OutboundOpaqueMessage::new(ContentType::ApplicationData, ProtocolVersion::TLSv1_2, payload))
    }

    fn encrypted_payload_len(&self, payload_len: usize) -> usize {
        payload_len + 1 + TAG_LEN
    }
}

impl MessageDecrypter for Tls13Records {
    fn decrypt<'a>(&mut self, mut m: InboundOpaqueMessage<'a>, seq: u64) -> Result<InboundPlainMessage<'a>, rustls::Error> {
        let key = self.key.as_ref().ok_or(rustls::Error::DecryptError)?;
        let aad = cipher::make_tls13_aad(m.payload.len());
        let nonce = cipher::Nonce::new(&self.iv, seq).0;
        let text_len = open(key, &nonce, &aad, &mut m.payload)?;
        m.payload.truncate(text_len);
        m.into_tls13_unpadded_message()
    }
}

/// TLS 1.2's records: the sequence number, type, version and length in the
/// associated data.
struct Tls12Records {
    key: Option<Key>,
    iv: Iv,
}

impl MessageEncrypter for Tls12Records {
    fn encrypt(&mut self, m: OutboundPlainMessage<'_>, seq: u64) -> Result<OutboundOpaqueMessage, rustls::Error> {
        let key = self.key.as_ref().ok_or(rustls::Error::EncryptError)?;
        let total_len = self.encrypted_payload_len(m.payload.len());
        let mut payload = PrefixedPayload::with_capacity(total_len);
        payload.extend_from_chunks(&m.payload);
        let nonce = cipher::Nonce::new(&self.iv, seq).0;
        let aad = cipher::make_tls12_aad(seq, m.typ, m.version, m.payload.len());
        let tag = key.seal_in_place(&nonce, &aad, payload.as_mut());
        payload.extend_from_slice(&tag);
        Ok(OutboundOpaqueMessage::new(m.typ, m.version, payload))
    }

    fn encrypted_payload_len(&self, payload_len: usize) -> usize {
        payload_len + TAG_LEN
    }
}

impl MessageDecrypter for Tls12Records {
    fn decrypt<'a>(&mut self, mut m: InboundOpaqueMessage<'a>, seq: u64) -> Result<InboundPlainMessage<'a>, rustls::Error> {
        let key = self.key.as_ref().ok_or(rustls::Error::DecryptError)?;
        let text_len = m.payload.len().checked_sub(TAG_LEN).ok_or(rustls::Error::DecryptError)?;
        let aad = cipher::make_tls12_aad(seq, m.typ, m.version, text_len);
        let nonce = cipher::Nonce::new(&self.iv, seq).0;
        let text_len = open(key, &nonce, &aad, &mut m.payload)?;
        m.payload.truncate(text_len);
        Ok(m.into_plain_message())
    }
}

/// A record's payload -- its ciphertext, then the tag -- opened in place:
/// how much of it is the plaintext.
fn open(key: &Key, nonce: &[u8; NONCE_LEN], aad: &[u8], payload: &mut [u8]) -> Result<usize, rustls::Error> {
    let text_len = payload.len().checked_sub(TAG_LEN).ok_or(rustls::Error::DecryptError)?;
    let (text, tag) = payload.split_at_mut(text_len);
    let tag: &[u8; TAG_LEN] = (&*tag).try_into().map_err(|_| rustls::Error::DecryptError)?;
    key.open_in_place(nonce, aad, text, tag).map_err(|_| rustls::Error::DecryptError)?;
    Ok(text_len)
}
