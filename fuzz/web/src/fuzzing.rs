//! Every parser fed what a generator makes and then breaks, each held to:
//! no panic (an overflow included -- this build checks them); taking back
//! what its own writer wrote; answering a prefix of anything whole with
//! "not yet"; and keeping to the limits it was given.

use crate::vectors::{client_frame, written};
use crate::{Fails, Rng};
use mgmtwire::{json, Limits, Value, Writer};
use web::{cidr, http, sha1, ws, x509};

pub fn run(seed: u64, iters: u64) -> usize {
    let mut f = Fails::default();
    let mut rng = Rng(seed);
    let targets: [(&str, fn(&mut Rng, &mut Fails)); 7] = [
        ("json", json_one),
        ("http", http_one),
        ("ws", ws_one),
        ("cidr", cidr_one),
        ("sha1", sha1_one),
        ("pem", pem_one),
        ("x509", x509_one),
    ];
    for (name, one) in targets {
        let before = f.0;
        /* A certificate is slow to sign: fewer of them */
        let n = if name == "x509" { iters / 50 } else { iters };
        for i in 0..n {
            let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| one(&mut rng, &mut f)));
            if caught.is_err() {
                println!("FAIL: {} panicked at iteration {} (seed {})", name, i, seed);
                f.0 += 1;
            }
            if f.0 > before + 10 {
                break;
            }
        }
        println!("fuzz {:<5} {} inputs, {} failed", name, n, f.0 - before);
    }
    f.0
}

/// A value as a generator makes one: integers JavaScript holds, strings of
/// every kind of character.
fn gen_value(rng: &mut Rng, depth: usize) -> Value {
    match rng.below(if depth >= 4 { 4 } else { 6 }) {
        0 => Value::Null,
        1 => Value::Bool(rng.chance(2)),
        2 => {
            let n = (rng.next() % (2 * mgmtwire::SAFE_INT + 1)) as i128 - mgmtwire::SAFE_INT as i128;
            Value::Int(n)
        }
        3 => Value::Str(gen_string(rng)),
        4 => Value::List((0..rng.below(5)).map(|_| gen_value(rng, depth + 1)).collect()),
        _ => {
            let mut members: Vec<(String, Value)> = Vec::new();
            for _ in 0..rng.below(5) {
                let k = gen_string(rng);
                if !members.iter().any(|(m, _)| *m == k) {
                    members.push((k, gen_value(rng, depth + 1)));
                }
            }
            Value::Object(members)
        }
    }
}

fn gen_string(rng: &mut Rng) -> String {
    let pool = ['a', 'Z', '0', ' ', '"', '\\', '/', '\n', '\t', '\u{1}', '\u{1f}', '\u{7f}', '\u{e9}', '\u{2028}', '\u{1F600}', '\u{FFFD}'];
    (0..rng.below(12)).map(|_| pool[rng.below(pool.len() as u64) as usize]).collect()
}

fn write_value(w: &mut Writer, v: &Value) {
    match v {
        Value::Null => w.null(),
        Value::Bool(b) => w.bool(*b),
        /* Within what JavaScript holds: the callers see to it */
        Value::Int(n) => w.int(i64::try_from(*n).unwrap_or(i64::MAX)),
        Value::Str(s) => w.str(s),
        Value::List(items) => {
            w.begin_list();
            items.iter().for_each(|i| write_value(w, i));
            w.end_list();
        }
        Value::Object(members) => {
            w.begin_object();
            for (k, v) in members {
                w.key(k);
                write_value(w, v);
            }
            w.end_object();
        }
    }
}

/// Bytes changed the ways that break texts: one flipped, one dropped, one
/// put in, a run cut out, the end cut off.
fn mutate(rng: &mut Rng, b: &mut Vec<u8>, specials: &[u8]) {
    for _ in 0..1 + rng.below(3) {
        let at = rng.below(b.len() as u64 + 1) as usize;
        match rng.below(5) {
            0 if at < b.len() => b[at] ^= 1 << rng.below(8),
            1 if at < b.len() => {
                b.remove(at);
            }
            2 => b.insert(at, if rng.chance(2) { specials[rng.below(specials.len() as u64) as usize] } else { rng.byte() }),
            3 if at < b.len() => {
                let end = (at + rng.below(16) as usize).min(b.len());
                b.drain(at..end);
            }
            _ => b.truncate(at),
        }
    }
}

fn json_one(rng: &mut Rng, f: &mut Fails) {
    let v = gen_value(rng, 0);
    let (text, ok) = written(|w| write_value(w, &v));
    f.check("a generated value written", ok, &v);
    let back = json::parse(text.as_bytes(), &Limits::PARAMS);
    f.check("what was written reads back the same", back.as_ref() == Ok(&v), (&text, &back));
    let mut broken = text.into_bytes();
    mutate(rng, &mut broken, b"\"\\{}[],:0-.eu");
    /* What parses writes and reads back the same -- unless a number in it
     * is past what JavaScript holds, which the writer does not write as a
     * number at all. */
    if let Ok(again) = json::parse(&broken, &Limits::PARAMS) {
        if !contains_big(&again) {
            let (rewritten, ok) = written(|w| write_value(w, &again));
            f.check("what parses writes", ok, &again);
            let third = json::parse(rewritten.as_bytes(), &Limits::PARAMS);
            f.check("and reads back the same", third.as_ref() == Ok(&again), (&rewritten, &third));
        }
    }
    let tight = Limits { bytes: 1 + rng.below(64) as usize, depth: 1 + rng.below(4) as usize, values: 1 + rng.below(8) as usize,
                         members: 1 + rng.below(4) as usize, string: rng.below(8) as usize };
    if let Ok(v) = json::parse(&broken, &tight) {
        f.check("a value within its limits", within(&v, &tight, 0), (&v, tight));
    }
}

fn contains_big(v: &Value) -> bool {
    match v {
        Value::Int(n) => n.unsigned_abs() > u128::from(mgmtwire::SAFE_INT),
        Value::List(items) => items.iter().any(contains_big),
        Value::Object(m) => m.iter().any(|(_, v)| contains_big(v)),
        _ => false,
    }
}

fn within(v: &Value, l: &Limits, depth: usize) -> bool {
    match v {
        Value::Str(s) => s.len() <= l.string,
        Value::List(items) => depth < l.depth && items.len() < l.values && items.iter().all(|i| within(i, l, depth + 1)),
        Value::Object(m) => {
            depth < l.depth && m.len() <= l.members && m.iter().all(|(k, v)| k.len() <= l.string && within(v, l, depth + 1))
        }
        _ => true,
    }
}

fn gen_request(rng: &mut Rng) -> (Vec<u8>, usize) {
    let methods = ["GET", "POST", "PUT", "DELETE", "HEAD", "PATCH"];
    let paths = ["/", "/v1/host", "/v1/call/vm/list", "/v1/jobs?notes_since=3", "/v1/stream", "/a%20b"];
    let mut r = format!("{} {} HTTP/1.{}\r\n", methods[rng.below(6) as usize], paths[rng.below(6) as usize], rng.below(2));
    r.push_str("Host: 10.0.100.1:39234\r\n");
    for i in 0..rng.below(6) {
        let names = ["Accept", "X-Thing", "Connection", "User-Agent", "Sec-WebSocket-Key"];
        r.push_str(&format!("{}{}: {}\r\n", names[rng.below(5) as usize], i, "v".repeat(rng.below(40) as usize)));
    }
    let body_len = if rng.chance(2) { rng.below(300) as usize } else { 0 };
    if body_len > 0 || rng.chance(4) {
        r.push_str(&format!("Content-Length: {}\r\n", body_len));
    }
    r.push_str("\r\n");
    let mut b = r.into_bytes();
    let head = b.len();
    b.extend((0..body_len).map(|i| b'a' + (i % 26) as u8));
    (b, head + body_len)
}

fn http_one(rng: &mut Rng, f: &mut Fails) {
    let (req, whole) = gen_request(rng);
    match http::parse(&req, &http::Limits::API) {
        http::Parsed::Request(r, n) => {
            f.check("a generated request taken whole", n == whole, (n, whole));
            f.check("its target a path", r.target.starts_with('/'), r.target);
        }
        other => f.check("a generated request taken", false, (String::from_utf8_lossy(&req), other)),
    }
    let cut = rng.below(whole as u64) as usize;
    f.check("a request cut short is not there yet",
            matches!(http::parse(&req[..cut], &http::Limits::API), http::Parsed::Partial { .. }),
            (cut, String::from_utf8_lossy(&req[..cut])));
    let mut broken = req.clone();
    mutate(rng, &mut broken, b"\r\n :\t\0/?%");
    let limits = http::Limits { line: 16 + rng.below(64) as usize, headers: rng.below(128) as usize,
                                count: rng.below(6) as usize, body: rng.below(64) as usize };
    for l in [http::Limits::API, limits] {
        if let http::Parsed::Request(r, n) = http::parse(&broken, &l) {
            f.check("taken no more than it has", n <= broken.len(), ());
            f.check("a body within its limit", r.body.len() <= l.body, r.body.len());
            f.check("headers within their number", r.headers().len() <= l.count, r.headers().len());
            f.check("a target of visible ASCII", r.target.starts_with('/') && r.target.bytes().all(|c| (0x21..0x7F).contains(&c)), r.target);
            f.check("a request line within its limit", r.method_name.len() + r.target.len() + 10 <= l.line + 2, ());
            for (name, value) in r.headers() {
                f.check("a header's name a token", !name.is_empty() && name.bytes().all(|c| c.is_ascii_graphic() && c != b':'), name);
                f.check("a header's value without CR, LF or NUL", !value.iter().any(|c| matches!(c, b'\r' | b'\n' | 0)), value);
            }
        }
    }
}

fn ws_one(rng: &mut Rng, f: &mut Fails) {
    let max = 64 + rng.below(512) as usize;
    let mut stream = Vec::new();
    let mut messages: Vec<Vec<u8>> = Vec::new();
    for _ in 0..1 + rng.below(4) {
        let len = rng.below(max as u64 + 1) as usize;
        let msg: Vec<u8> = (0..len).map(|_| b'a' + rng.below(26) as u8).collect();
        let mut parts = 1 + rng.below(3) as usize;
        let mut at = 0;
        let mut first = true;
        while at < msg.len() || first {
            parts = parts.saturating_sub(1);
            let n = if parts == 0 { msg.len() - at } else { rng.below((msg.len() - at) as u64 + 1) as usize };
            let key = [rng.byte(), rng.byte(), rng.byte(), rng.byte()];
            let fin = at + n == msg.len();
            stream.extend(client_frame(fin, if first { 1 } else { 0 }, &msg[at..at + n], key));
            at += n;
            first = false;
            if rng.chance(3) {
                stream.extend(client_frame(true, 9, b"ping", key));
            }
            if fin {
                break;
            }
        }
        messages.push(msg);
    }
    let mut d = ws::Decoder::new(max);
    let mut buf = stream.clone();
    let mut at = 0;
    let mut got = Vec::new();
    loop {
        match d.feed(&mut buf[at..]) {
            Ok(Some((e, n))) => {
                at += n;
                if let Some(ws::Event::Message { data, .. }) = e {
                    got.push(data.to_vec());
                }
            }
            Ok(None) => break,
            Err(e) => {
                f.check("a generated stream read", false, e);
                break;
            }
        }
    }
    f.check("every message as it was sent", got == messages && at == stream.len(), (got.len(), messages.len()));
    let mut broken = stream;
    mutate(rng, &mut broken, &[0x80, 0x81, 0x7E, 0x7F, 0x88, 0x89, 0xFF, 0]);
    let mut d = ws::Decoder::new(max);
    let mut at = 0;
    let mut steps = 0;
    while steps < 64 {
        steps += 1;
        match d.feed(&mut broken[at..]) {
            Ok(Some((e, n))) => {
                f.check("a frame takes bytes", n > 0, ());
                at += n;
                match e {
                    Some(ws::Event::Message { data, text }) => {
                        f.check("a message within its limit", data.len() <= max, data.len());
                        f.check("a text message UTF-8", !text || std::str::from_utf8(data).is_ok(), ());
                    }
                    Some(ws::Event::Ping(p) | ws::Event::Pong(p)) => f.check("a control frame short", p.len() <= ws::CONTROL_MAX, ()),
                    _ => {}
                }
            }
            _ => break,
        }
    }
}

fn cidr_one(rng: &mut Rng, f: &mut Fails) {
    let ip = rng.next() as u32;
    let bits = rng.below(33) as u32;
    let text = format!("{}.{}.{}.{}/{}", ip >> 24, (ip >> 16) & 255, (ip >> 8) & 255, ip & 255, bits);
    let n = cidr::Cidr::parse(&text);
    f.check("a network written reads", n.is_some_and(|n| n.contains(ip) && n.parts().1 == bits), &text);
    if let (Some(n), 1..=32) = (n, bits) {
        /* The prefix's last bit flipped */
        let outside = ip ^ (1u32 << (32 - bits));
        f.check("an address off the prefix is outside", !n.contains(outside), (&text, outside));
    }
    let mut broken = text.into_bytes();
    mutate(rng, &mut broken, b"./0123456789 ");
    let _ = cidr::Cidr::parse(&String::from_utf8_lossy(&broken));
}

fn sha1_one(rng: &mut Rng, f: &mut Fails) {
    let data: Vec<u8> = (0..rng.below(300)).map(|_| rng.byte()).collect();
    let whole = sha1::sha1(&data);
    let mut s = sha1::Sha1::new();
    let mut at = 0;
    while at < data.len() {
        let n = (1 + rng.below(70) as usize).min(data.len() - at);
        s.update(&data[at..at + n]);
        at += n;
    }
    f.check("sha1 in pieces is sha1 whole", s.finish() == whole, data.len());
}

fn pem_one(rng: &mut Rng, f: &mut Fails) {
    let der: Vec<u8> = (0..rng.below(400)).map(|_| rng.byte()).collect();
    let text = x509::pem("CERTIFICATE", &der).unwrap_or_default();
    f.check("PEM reads back", x509::pem_decode("CERTIFICATE", &text).as_deref() == Some(&der[..]), der.len());
    let mut broken = text.into_bytes();
    mutate(rng, &mut broken, b"-=\n ABCabc+/");
    let _ = x509::pem_decode("CERTIFICATE", &String::from_utf8_lossy(&broken));
    let _ = x509::find_spki(&der);
}

fn x509_one(rng: &mut Rng, f: &mut Fails) {
    let mut secret = [0u8; 32];
    secret.iter_mut().for_each(|b| *b = rng.byte());
    let now = rng.below(253_402_300_799);
    let req = x509::Request {
        name: "nos",
        ips: &[[10, 0, 100, 1]],
        dns: &["nos"],
        serial: [rng.byte(); 16],
        not_before: now,
        not_after: now.saturating_add(rng.below(1 << 30)).min(253_402_300_799),
        usage: if rng.chance(2) { x509::Usage::Server } else { x509::Usage::Client },
    };
    match x509::self_signed(&secret, &req) {
        Ok(id) => {
            f.check("a certificate's SubjectPublicKeyInfo found", x509::find_spki(&id.cert).is_some(), ());
            let mut broken = id.cert.clone();
            mutate(rng, &mut broken, &[0x30, 0x80, 0x81, 0x82, 0xFF, 0]);
            let _ = x509::find_spki(&broken);
        }
        Err(x509::Error::BadKey) => {}
        Err(e) => f.check("a certificate made", false, e),
    }
}
