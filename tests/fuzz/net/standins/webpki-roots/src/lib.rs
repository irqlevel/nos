//! The roots the fuzzer's TLS client trusts: its own certificate authority,
//! and nothing else. `tls` reads them as it reads Mozilla's,
//! `webpki_roots::TLS_SERVER_ROOTS.iter()`, so they are a slice here too --
//! behind a `LazyLock`, since a trust anchor is taken out of the certificate
//! by webpki, which is not a `const fn`.

use std::sync::LazyLock;

use pki_types::{CertificateDer, TrustAnchor};

/// The fuzzer's CA: P-256, valid 2020 to 2120 (`certs/README` says how it
/// was made).
static CA: CertificateDer<'static> = CertificateDer::from_slice(include_bytes!("../../../certs/ca.der"));

pub static TLS_SERVER_ROOTS: LazyLock<Vec<TrustAnchor<'static>>> = LazyLock::new(|| {
    vec![webpki::anchor_from_trusted_cert(&CA).expect("the fuzzer's CA certificate is one")]
});
