//! Certificates written out for OpenSSL and Python's ssl to judge: a
//! server's, as the host API makes its own; a client's, as a client of it
//! is given one; one valid into 2051, whose end is a GeneralizedTime. Each
//! with its key in PKCS#8 PEM, and a file of what this code says of each --
//! its fingerprint as OpenSSL prints one, its SubjectPublicKeyInfo's SHA-256
//! -- for the script to compare with what OpenSSL says.

use crate::vectors::server_request;
use std::fmt::Write as _;
use web::x509;

pub fn write(dir: &str) -> usize {
    let mut fails = 0;
    let mut said = String::new();
    /* Valid from a day ago: the TLS libraries that judge them check dates */
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(1_761_000_000)
        .saturating_sub(86_400);
    let client = x509::Request {
        name: "panel",
        ips: &[],
        dns: &[],
        serial: [0x3C; 16],
        not_before: now,
        not_after: now + 365 * 86_400,
        usage: x509::Usage::Client,
    };
    let made = [
        ("server", [11u8; 32], server_request(now, now + 825 * 86_400)),
        ("client", [12u8; 32], client),
        ("late", [13u8; 32], server_request(now, 2_556_143_999)),
    ];
    for (name, secret, req) in made {
        let id = match x509::self_signed(&secret, &req) {
            Ok(id) => id,
            Err(e) => {
                println!("FAIL: {}'s certificate: {:?}", name, e);
                fails += 1;
                continue;
            }
        };
        let (Ok(cert), Ok(key)) = (x509::pem("CERTIFICATE", &id.cert), x509::pem("PRIVATE KEY", &id.key)) else {
            println!("FAIL: {}'s PEM", name);
            fails += 1;
            continue;
        };
        let wrote = std::fs::write(format!("{}/{}.crt", dir, name), cert)
            .and_then(|_| std::fs::write(format!("{}/{}.key", dir, name), &*key));
        if let Err(e) = wrote {
            println!("FAIL: {}'s files: {}", name, e);
            fails += 1;
            continue;
        }
        let spki = x509::find_spki(&id.cert).map(x509::sha256).map(|d| crate::vectors::hex(&d)).unwrap_or_default();
        let _ = writeln!(said, "{} fingerprint {} spki {}", name, x509::fingerprint_hex(&x509::sha256(&id.cert)), spki);
    }
    if let Err(e) = std::fs::write(format!("{}/said.txt", dir), said) {
        println!("FAIL: said.txt: {}", e);
        fails += 1;
    }
    fails
}
