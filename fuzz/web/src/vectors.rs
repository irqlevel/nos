//! Known answers: each input with what it must make, or why it must be
//! refused.

use crate::Fails;
use mgmtwire::{json, Limits, Params, Status, Value, Writer};
use p256::ecdsa::signature::Verifier;
use p256::ecdsa::{Signature, SigningKey};
use p256::pkcs8::DecodePrivateKey;
use web::{cidr, http, sha1, ws, x509};

pub fn run() -> usize {
    let mut f = Fails::default();
    sha1_vectors(&mut f);
    ws_vectors(&mut f);
    json_vectors(&mut f);
    writer_vectors(&mut f);
    params_vectors(&mut f);
    status_vectors(&mut f);
    http_vectors(&mut f);
    cidr_vectors(&mut f);
    x509_vectors(&mut f);
    crate::descriptors::vectors(&mut f);
    println!("vectors: {} failed", f.0);
    f.0
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{:02x}", x)).collect()
}

fn sha1_vectors(f: &mut Fails) {
    let cases: [(&[u8], &str); 3] = [
        (b"", "da39a3ee5e6b4b0d3255bfef95601890afd80709"),
        (b"abc", "a9993e364706816aba3e25717850c26c9cd0d89d"),
        (b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq", "84983e441c3bd26ebaae4aa1f95129e5e54670f1"),
    ];
    for (input, want) in cases {
        let got = hex(&sha1::sha1(input));
        f.check("sha1 of a FIPS 180 vector", got == want, (String::from_utf8_lossy(input), got));
    }
    let million = vec![b'a'; 1_000_000];
    let got = hex(&sha1::sha1(&million));
    f.check("sha1 of a million a's", got == "34aa973cd4c4daa4f61eeb2bdbad27316534016f", got);
}

/// A client's frame: masked with `key`.
pub fn client_frame(fin: bool, opcode: u8, payload: &[u8], key: [u8; 4]) -> Vec<u8> {
    let mut v = vec![(if fin { 0x80 } else { 0 }) | opcode];
    match payload.len() {
        n @ 0..=125 => v.push(0x80 | n as u8),
        n if n <= 0xFFFF => {
            v.push(0x80 | 126);
            v.extend_from_slice(&(n as u16).to_be_bytes());
        }
        n => {
            v.push(0x80 | 127);
            v.extend_from_slice(&(n as u64).to_be_bytes());
        }
    }
    v.extend_from_slice(&key);
    v.extend(payload.iter().enumerate().map(|(i, b)| b ^ key[i % 4]));
    v
}

fn feed_one(max: usize, frame: &[u8]) -> Result<Option<(Option<(String, Vec<u8>)>, usize)>, ws::WsError> {
    let mut d = ws::Decoder::new(max);
    let mut buf = frame.to_vec();
    Ok(d.feed(&mut buf)?.map(|(e, n)| {
        let e = e.map(|e| match e {
            ws::Event::Message { text, data } => (String::from(if text { "text" } else { "binary" }), data.to_vec()),
            ws::Event::Ping(p) => (String::from("ping"), p.to_vec()),
            ws::Event::Pong(p) => (String::from("pong"), p.to_vec()),
            ws::Event::Close { code, reason } => {
                (format!("close {:?}", code), reason.as_bytes().to_vec())
            }
        });
        (e, n)
    }))
}

fn ws_vectors(f: &mut Fails) {
    let accept = ws::accept_key(b"dGhlIHNhbXBsZSBub25jZQ==");
    f.check("the accept key of RFC 6455's example", accept.as_ref().map(|a| &a[..]) == Some(&b"s3pPLMBiTxaQ9kYGzzhZRbK+xOo="[..]),
            accept.map(|a| String::from_utf8_lossy(&a).into_owned()));
    f.check("a key not 16 bytes has no accept key", ws::accept_key(b"dGhlIHNhbXBsZQ==").is_none(), ());
    f.check("a key not base64 has no accept key", ws::accept_key(b"!!!!!!!!!!!!!!!!!!!!!!==").is_none(), ());

    /* RFC 6455 5.7: a masked "Hello" */
    let hello = [0x81, 0x85, 0x37, 0xfa, 0x21, 0x3d, 0x7f, 0x9f, 0x4d, 0x51, 0x58];
    let got = feed_one(1024, &hello);
    f.check("RFC 6455's masked Hello", matches!(&got, Ok(Some((Some((k, d)), 11))) if k == "text" && d == b"Hello"), &got);
    for cut in 0..hello.len() {
        let got = feed_one(1024, &hello[..cut]);
        f.check("a frame cut short is not there yet", matches!(got, Ok(None)), (cut, &got));
    }
    let ping = client_frame(true, 9, b"Hello", [1, 2, 3, 4]);
    let got = feed_one(1024, &ping);
    f.check("a ping", matches!(&got, Ok(Some((Some((k, d)), _))) if k == "ping" && d == b"Hello"), &got);
    let close = client_frame(true, 8, &[0x03, 0xE8, b'b', b'y', b'e'], [9, 9, 9, 9]);
    let got = feed_one(1024, &close);
    f.check("a close of 1000", matches!(&got, Ok(Some((Some((k, d)), _))) if k == "close Some(1000)" && d == b"bye"), &got);
    let long = vec![0x41u8; 300];
    let got = feed_one(1024, &client_frame(true, 2, &long, [5, 6, 7, 8]));
    f.check("a frame of 300 bytes", matches!(&got, Ok(Some((Some((k, d)), _))) if k == "binary" && d.len() == 300), ());

    /* A message in three fragments, a ping between them */
    let mut d = ws::Decoder::new(1024);
    let mut stream = client_frame(false, 1, b"Hel", [1, 1, 1, 1]);
    stream.extend(client_frame(true, 9, b"p", [2, 2, 2, 2]));
    stream.extend(client_frame(false, 0, b"l", [3, 3, 3, 3]));
    stream.extend(client_frame(true, 0, b"o", [4, 4, 4, 4]));
    let mut events = Vec::new();
    let mut at = 0;
    loop {
        match d.feed(&mut stream[at..]) {
            Ok(Some((e, n))) => {
                at += n;
                if let Some(e) = e {
                    events.push(format!("{:?}", e));
                }
            }
            Ok(None) => break,
            Err(e) => {
                events.push(format!("{:?}", e));
                break;
            }
        }
    }
    f.check("fragments put together, a ping between them",
            events == ["Ping([112])", "Message { text: true, data: [72, 101, 108, 108, 111] }"] && at == stream.len(),
            &events);

    let refusals: Vec<(&str, Vec<u8>, usize, u16)> = vec![
        ("a client's frame not masked", vec![0x81, 0x05, b'H', b'e', b'l', b'l', b'o'], 1024, 1002),
        ("a reserved bit", client_frame(true, 1 | 0x40, b"x", [1, 2, 3, 4]), 1024, 1002),
        ("an unknown opcode", client_frame(true, 3, b"x", [1, 2, 3, 4]), 1024, 1002),
        ("a control frame of 126 bytes", client_frame(true, 9, &[0; 126], [1, 2, 3, 4]), 1024, 1002),
        ("a ping fragmented", client_frame(false, 9, b"x", [1, 2, 3, 4]), 1024, 1002),
        ("a length not in its shortest form", vec![0x82, 0x80 | 126, 0, 5, 1, 2, 3, 4, 0, 0, 0, 0, 0], 1024, 1002),
        ("a 64-bit length with its top bit", vec![0x82, 0x80 | 127, 0x80, 0, 0, 0, 0, 0, 0, 0, 1, 2, 3, 4], 1024, 1002),
        ("a frame past the limit", client_frame(true, 2, &[0; 17], [1, 2, 3, 4]), 16, 1009),
        ("a continuation of no message", client_frame(true, 0, b"x", [1, 2, 3, 4]), 1024, 1002),
        ("text not UTF-8", client_frame(true, 1, &[0xFF, 0xFE], [1, 2, 3, 4]), 1024, 1007),
        ("a close code nobody may send", client_frame(true, 8, &[0x03, 0xED], [1, 2, 3, 4]), 1024, 1002),
        ("a close of one byte", client_frame(true, 8, &[0x03], [1, 2, 3, 4]), 1024, 1002),
        ("a close's reason not UTF-8", client_frame(true, 8, &[0x03, 0xE8, 0xFF], [1, 2, 3, 4]), 1024, 1007),
    ];
    for (what, frame, max, code) in refusals {
        let got = feed_one(max, &frame);
        f.check(what, matches!(&got, Err(e) if e.close_code() == code), &got);
    }
    let mut d = ws::Decoder::new(1024);
    let mut two = client_frame(false, 1, b"a", [1, 2, 3, 4]);
    two.extend(client_frame(true, 1, b"b", [1, 2, 3, 4]));
    let first = d.feed(&mut two).map(|r| r.map(|(e, n)| (e.is_none(), n)));
    let n = first.as_ref().ok().and_then(|r| r.map(|(_, n)| n)).unwrap_or(0);
    let second = d.feed(&mut two[n..]).map(|r| r.map(|(_, n)| n));
    f.check("a message begun inside another", matches!(second, Err(ws::WsError::Protocol(_))), (&first, &second));

    /* Fragments each within the limit, the message past it */
    let mut d = ws::Decoder::new(16);
    let mut two = client_frame(false, 2, &[1; 10], [1, 2, 3, 4]);
    two.extend(client_frame(true, 0, &[2; 10], [1, 2, 3, 4]));
    let n = d.feed(&mut two).ok().flatten().map(|(_, n)| n).unwrap_or(0);
    let second = d.feed(&mut two[n..]).map(|r| r.map(|(_, n)| n));
    f.check("a message past its limit in fragments within it", matches!(second, Err(ws::WsError::TooBig)), (n, &second));

    let mut out = Vec::new();
    let w = ws::write_frame(&mut out, ws::Opcode::Text, b"Hello");
    f.check("a server's Hello", w.is_ok() && out == [0x81, 0x05, b'H', b'e', b'l', b'l', b'o'], &out);
    let mut out = Vec::new();
    let _ = ws::write_frame(&mut out, ws::Opcode::Binary, &[0; 200]);
    f.check("a server's frame of 200", out[..4] == [0x82, 126, 0, 200] && out.len() == 204, &out[..4]);
    let mut out = Vec::new();
    let _ = ws::write_frame(&mut out, ws::Opcode::Binary, &[0; 70_000]);
    f.check("a server's frame of 70000", out[..10] == [0x82, 127, 0, 0, 0, 0, 0, 1, 0x11, 0x70] && out.len() == 70_010,
            &out[..10]);
    let mut out = Vec::new();
    f.check("a server's ping of 126 refused", ws::write_frame(&mut out, ws::Opcode::Ping, &[0; 126]).is_err() && out.is_empty(), ());
    let mut out = Vec::new();
    let _ = ws::write_close(&mut out, 1000, "bye");
    f.check("a server's close", out == [0x88, 5, 0x03, 0xE8, b'b', b'y', b'e'], &out);
}

fn parse(text: &str) -> Result<Value, json::ParseError> {
    json::parse(text.as_bytes(), &Limits::PARAMS)
}

fn json_vectors(f: &mut Fails) {
    let v = parse(r#" {"a" : 1, "b":[true,false,null], "c":"x\"y\\z\n\u00e9\/", "d":-9223372036854775808,
                     "e":18446744073709551615, "f":{}, "g":[]} "#);
    let ok = match &v {
        Ok(v) => v.get("a") == Some(&Value::Int(1))
            && v.get("b").and_then(Value::as_list).map(|l| l.len()) == Some(3)
            && v.get("c").and_then(Value::as_str) == Some("x\"y\\z\n\u{e9}/")
            && v.get("d").and_then(Value::as_i64) == Some(i64::MIN)
            && v.get("e").and_then(Value::as_u64) == Some(u64::MAX),
        Err(_) => false,
    };
    f.check("an object of every kind", ok, &v);
    f.check("a surrogate pair", parse(r#""\ud83d\ude00""#) == Ok(Value::Str(String::from("\u{1F600}"))), parse(r#""\ud83d\ude00""#));
    f.check("-0 is 0", parse("-0") == Ok(Value::Int(0)), parse("-0"));

    let refused = [
        ("1.5", "a fraction"),
        ("1e3", "an exponent"),
        ("01", "a leading zero"),
        ("-", "a minus alone"),
        (r#"{"a":1,"a":2}"#, "a key twice"),
        (r#""\ud83d""#, "a high surrogate alone"),
        (r#""\ude00""#, "a low surrogate alone"),
        (r#""\ud83dx""#, "a high surrogate before a character"),
        ("[1,]", "a trailing comma"),
        (r#"{"a":}"#, "a key without its value"),
        ("18446744073709551616", "2^64"),
        ("-9223372036854775809", "below i64::MIN"),
        ("\"a", "a string not ended"),
        ("\"a\u{1}\"", "a control character in a string"),
        ("1 2", "two values"),
        ("", "nothing"),
        ("tru", "a word cut short"),
        ("[1 2]", "no comma"),
        (r#"{1:2}"#, "a key that is not a string"),
        (r#""\x""#, "an unknown escape"),
        (r#""\u12""#, "a short \\u"),
    ];
    for (text, what) in refused {
        f.check(what, parse(text).is_err(), (text, parse(text)));
    }
    f.check("bytes not UTF-8 in a string", json::parse(b"\"\xff\"", &Limits::PARAMS).is_err(), ());
    let deep16 = format!("{}{}", "[".repeat(16), "]".repeat(16));
    let deep17 = format!("{}{}", "[".repeat(17), "]".repeat(17));
    f.check("16 lists within each other", parse(&deep16).is_ok(), ());
    f.check("17 lists within each other refused", parse(&deep17).is_err(), ());
    let tight = Limits { bytes: 20, depth: 4, values: 5, members: 2, string: 4 };
    f.check("a text past its length", json::parse(b"[1,2,3,4,5,6,7,8,9,10]", &tight).is_err(), ());
    f.check("values past their number", json::parse(b"[1,2,3,4,5]", &tight).is_err(), ());
    f.check("members past their number", json::parse(br#"{"a":1,"b":2,"c":3}"#, &tight).is_err(), ());
    f.check("a string past its length", json::parse(br#""abcde""#, &tight).is_err(), ());
    f.check("a string at its length", json::parse(br#""ab\u00e9""#, &tight).is_ok(), ());
    f.check("digits in a string are a count", parse(r#""18446744073709551615""#).ok().and_then(|v| v.as_u64()) == Some(u64::MAX), ());
    f.check("digits with a leading zero are none", parse(r#""012""#).ok().and_then(|v| v.as_u64()).is_none(), ());
}

/// What a writer wrote, and whether it says it is whole.
pub fn written(f: impl FnOnce(&mut Writer)) -> (String, bool) {
    let mut out = Vec::new();
    let mut sink = |b: &[u8]| {
        out.extend_from_slice(b);
        true
    };
    let ok = {
        let mut w = Writer::new(&mut sink);
        f(&mut w);
        w.ok()
    };
    (String::from_utf8(out).unwrap_or_default(), ok)
}

fn writer_vectors(f: &mut Fails) {
    let (text, ok) = written(|w| {
        w.begin_object();
        w.field_str("s", "a\"b\\c\n\u{1}\u{e9}");
        w.field_uint("n", mgmtwire::SAFE_INT);
        w.field_uint("big", mgmtwire::SAFE_INT + 1);
        w.key("neg");
        w.int(-5);
        w.key("l");
        w.begin_list();
        w.uint(1);
        w.bool(true);
        w.null();
        w.begin_object();
        w.end_object();
        w.end_list();
        w.end_object();
    });
    let want = "{\"s\":\"a\\\"b\\\\c\\n\\u0001\u{e9}\",\"n\":9007199254740991,\"big\":\"9007199254740992\",\"neg\":-5,\"l\":[1,true,null,{}]}";
    f.check("an object written", ok && text == want, &text);
    f.check("what was written reads back", parse(&text).is_ok(), parse(&text));

    let misuses: [(&str, fn(&mut Writer)); 6] = [
        ("a key outside an object", |w| w.key("a")),
        ("a value in an object without its key", |w| {
            w.begin_object();
            w.uint(1);
            w.end_object();
        }),
        ("an object left open", |w| w.begin_object()),
        ("two values", |w| {
            w.uint(1);
            w.uint(2);
        }),
        ("a signed number JavaScript does not hold", |w| w.int(1 << 53)),
        ("a list closed as an object", |w| {
            w.begin_list();
            w.end_object();
        }),
    ];
    for (what, misuse) in misuses {
        let (_, ok) = written(misuse);
        f.check(what, !ok, ());
    }
    let (_, ok) = written(|_| {});
    f.check("nothing written is not a text", !ok, ());

    let mut taken = 0;
    let mut sink = |b: &[u8]| {
        taken += b.len();
        taken <= 8
    };
    let mut w = Writer::new(&mut sink);
    w.begin_object();
    w.field_str("key", "a value longer than the sink takes");
    w.end_object();
    f.check("a sink that takes no more", !w.ok(), ());

    f.check("provider names", mgmtwire::valid_provider("vm") && mgmtwire::valid_provider("storage_2")
            && !mgmtwire::valid_provider("Vm") && !mgmtwire::valid_provider("2vm") && !mgmtwire::valid_provider("")
            && !mgmtwire::valid_provider("a.b") && !mgmtwire::valid_provider(&"a".repeat(25)), ());
    f.check("method names", mgmtwire::valid_method("console.read") && !mgmtwire::valid_method(".a")
            && !mgmtwire::valid_method("a-b") && !mgmtwire::valid_method(&"a".repeat(33)), ());
}

fn params_vectors(f: &mut Fails) {
    let v = parse(r#"{"name":"vm1","cpus":2,"big":"18446744073709551615","on":true,"text":"a\nb","gone":null,"extra":1}"#)
        .unwrap_or(Value::Null);
    let mut p = match Params::new(&v) {
        Ok(p) => p,
        Err(e) => {
            f.check("parameters of an object", false, e);
            return;
        }
    };
    f.check("a string", p.str("name", 32) == Ok("vm1"), ());
    f.check("a count", p.uint("cpus", 1, 16) == Ok(2), ());
    f.check("a count past JavaScript's", p.uint("big", 0, u64::MAX) == Ok(u64::MAX), ());
    f.check("a truth", p.bool("on") == Ok(true), ());
    f.check("text with a newline", p.text("text", 16) == Ok("a\nb"), ());
    f.check("a null is not there", p.opt_str("gone", 8) == Ok(None), ());
    f.check("one not taken refuses the rest", matches!(p.finish(), Err(e) if e.field == "extra"), ());

    let v = parse(r#"{"name":"a\u0001b","n":17,"s":"","l":[1,2,3]}"#).unwrap_or(Value::Null);
    let mut p = Params::new(&v).expect("an object");
    f.check("a control character in a name", p.str("name", 32).is_err(), ());
    f.check("a count out of its range", p.uint("n", 1, 16).is_err(), ());
    f.check("an empty string", p.str("s", 8).is_err(), ());
    f.check("a list too long", p.list("l", 2).is_err(), ());
    f.check("one needed and not there", matches!(p.uint("cpus", 1, 16), Err(e) if e.why == "is needed"), ());
    f.check("no parameters", Params::new(&Value::Null).map(|p| p.finish().is_ok()) == Ok(true), ());
    f.check("parameters not an object", Params::new(&Value::Int(1)).is_err(), ());
}

fn status_vectors(f: &mut Fails) {
    for code in -14..=1 {
        let s = Status::from_code(code);
        f.check("a status's code reads back", s.code() == code || (s == Status::Failed && !(-13..=0).contains(&code)),
                (code, s));
    }
    let (text, ok) = written(|w| Status::Conflict.write_error(w, "vm noble is running", Some("name")));
    f.check("an error reply", ok && text == r#"{"error":{"code":"conflict","message":"vm noble is running","field":"name"}}"#,
            &text);
}

fn req(text: &[u8]) -> Result<(String, usize), String> {
    match http::parse(text, &http::Limits::API) {
        http::Parsed::Request(r, n) => Ok((format!("{:?} {} {}", r.method, r.target, String::from_utf8_lossy(r.body)), n)),
        http::Parsed::Partial { continue_wanted } => Err(format!("partial {}", continue_wanted)),
        http::Parsed::Refused(r) => Err(format!("{}", r.status)),
    }
}

fn http_vectors(f: &mut Fails) {
    let get = b"GET /v1/host HTTP/1.1\r\nHost: 10.0.100.1:39234\r\nUser-Agent: nosctl\r\n\r\n";
    f.check("a GET", req(get) == Ok((String::from("Get /v1/host "), get.len())), req(get));
    for cut in 0..get.len() {
        f.check("a request cut short is not there yet", req(&get[..cut]) == Err(String::from("partial false")),
                (cut, req(&get[..cut])));
    }
    let post = b"POST /v1/call/vm/list HTTP/1.1\r\nHost: x\r\nContent-Length: 2\r\n\r\n{}";
    f.check("a POST with its body", req(post) == Ok((String::from("Post /v1/call/vm/list {}"), post.len())), req(post));
    let expect = b"POST /v1/call/vm/list HTTP/1.1\r\nHost: x\r\nExpect: 100-continue\r\nContent-Length: 2\r\n\r\n";
    f.check("one waiting for 100 Continue", req(expect) == Err(String::from("partial true")), req(expect));
    let mut two = get.to_vec();
    two.extend_from_slice(post);
    f.check("two in a row, the first", req(&two) == Ok((String::from("Get /v1/host "), get.len())), req(&two));
    f.check("two in a row, the second", req(&two[get.len()..]).map(|(_, n)| n) == Ok(post.len()), ());
    let one_oh = b"GET / HTTP/1.0\r\n\r\n";
    f.check("HTTP/1.0 without a Host", req(one_oh).is_ok(), req(one_oh));

    if let http::Parsed::Request(r, _) = http::parse(b"GET /v1/jobs?notes_since=12&x=y HTTP/1.1\r\nHOST: x\r\nConnection: Upgrade, close\r\n\r\n", &http::Limits::API) {
        f.check("a path", r.path() == "/v1/jobs", r.path());
        f.check("a query's value", r.query("notes_since") == Some("12") && r.query("x") == Some("y") && r.query("z").is_none(), ());
        f.check("a header of any case", r.header_str("host") == Some("x"), ());
        f.check("close said", !r.keep_alive(), ());
    } else {
        f.check("a request with a query", false, ());
    }

    let long_line = format!("GET /{} HTTP/1.1\r\nHost: x\r\n\r\n", "a".repeat(2100));
    let many: String = (0..33).map(|i| format!("X-{}: 1\r\n", i)).collect();
    let many = format!("GET / HTTP/1.1\r\nHost: x\r\n{}\r\n", many);
    let long_headers = format!("GET / HTTP/1.1\r\nHost: x\r\nX-Big: {}\r\n\r\n", "a".repeat(8200));
    let big = format!("POST / HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\n", 64 * 1024 + 1);
    let refusals: Vec<(&str, Vec<u8>, &str)> = vec![
        ("a line ended by LF alone", b"GET / HTTP/1.1\nHost: x\n\n".to_vec(), "400"),
        ("a CR inside a line", b"GET / HTTP/1.1\r\nHost: x\ry\r\n\r\n".to_vec(), "400"),
        ("no Host", b"GET / HTTP/1.1\r\n\r\n".to_vec(), "400"),
        ("two Hosts", b"GET / HTTP/1.1\r\nHost: a\r\nHost: b\r\n\r\n".to_vec(), "400"),
        ("a Transfer-Encoding", b"POST / HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec(), "501"),
        ("two Content-Lengths", b"POST / HTTP/1.1\r\nHost: x\r\nContent-Length: 1\r\nContent-Length: 1\r\n\r\nx".to_vec(), "400"),
        ("a Content-Length that is no number", b"POST / HTTP/1.1\r\nHost: x\r\nContent-Length: 1a\r\n\r\n".to_vec(), "400"),
        ("a Content-Length with a sign", b"POST / HTTP/1.1\r\nHost: x\r\nContent-Length: +1\r\n\r\n".to_vec(), "400"),
        ("a body past the limit", big.into_bytes(), "413"),
        ("a request line past the limit", long_line.into_bytes(), "414"),
        ("headers past the limit", long_headers.into_bytes(), "431"),
        ("33 headers", many.into_bytes(), "431"),
        ("a header folded", b"GET / HTTP/1.1\r\nHost: x\r\n y\r\n\r\n".to_vec(), "400"),
        ("a space before the colon", b"GET / HTTP/1.1\r\nHost : x\r\n\r\n".to_vec(), "400"),
        ("a control character in a value", b"GET / HTTP/1.1\r\nHost: x\x01y\r\n\r\n".to_vec(), "400"),
        ("an absolute target", b"GET http://x/ HTTP/1.1\r\nHost: x\r\n\r\n".to_vec(), "400"),
        ("a target with a space", b"GET /a b HTTP/1.1\r\nHost: x\r\n\r\n".to_vec(), "400"),
        ("HTTP/2.0", b"GET / HTTP/2.0\r\nHost: x\r\n\r\n".to_vec(), "505"),
        ("no version", b"GET /\r\nHost: x\r\n\r\n".to_vec(), "400"),
        ("two spaces", b"GET  / HTTP/1.1\r\nHost: x\r\n\r\n".to_vec(), "400"),
        ("an empty line first", b"\r\nGET / HTTP/1.1\r\nHost: x\r\n\r\n".to_vec(), "400"),
        ("a header without a colon", b"GET / HTTP/1.1\r\nHost: x\r\nOops\r\n\r\n".to_vec(), "400"),
    ];
    for (what, text, status) in refusals {
        f.check(what, req(&text) == Err(String::from(status)), (status, req(&text)));
    }

    let mut out = Vec::new();
    let r = http::write_head(&mut out, 200, &[("Content-Type", "application/json")], Some(2));
    f.check("a response's head", r.is_ok() && out == b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n",
            String::from_utf8_lossy(&out));
    let mut out = Vec::new();
    f.check("a value with a CRLF refused", http::write_head(&mut out, 200, &[("X", "a\r\nInjected: 1")], None).is_err() && out.is_empty(), ());
    f.check("a name with a space refused", http::write_head(&mut out, 200, &[("X Y", "a")], None).is_err() && out.is_empty(), ());
    f.check("a status that is none refused", http::write_head(&mut out, 99, &[], None).is_err(), ());
}

fn cidr_vectors(f: &mut Fails) {
    let ip = |s: &str| cidr::parse_ipv4(s).unwrap_or(0);
    let n = cidr::Cidr::parse("10.0.0.0/8");
    f.check("a /8", n.is_some_and(|n| n.contains(ip("10.1.2.3")) && !n.contains(ip("11.0.0.1"))), n);
    let one = cidr::Cidr::parse("1.2.3.4");
    f.check("an address alone", one.is_some_and(|n| n.contains(ip("1.2.3.4")) && !n.contains(ip("1.2.3.5"))), one);
    let all = cidr::Cidr::parse("0.0.0.0/0");
    f.check("every address", all.is_some_and(|n| n.contains(ip("255.255.255.255")) && n.contains(0)), all);
    let masked = cidr::Cidr::parse("10.1.2.3/8");
    f.check("bits past the prefix dropped", masked.map(|n| n.parts()) == Some((ip("10.0.0.0"), 8)), masked);
    for bad in ["1.2.3", "1.2.3.4.5", "256.1.1.1", "01.2.3.4", "1.2.3.4/33", "1.2.3.4/", "a.b.c.d", " 1.2.3.4", "1.2.3.4/08", "", "1..2.3"] {
        f.check("not a network", cidr::Cidr::parse(bad).is_none(), bad);
    }
}

/// The TBSCertificate and the signature's r and s, read out of a
/// certificate's DER the long way, as a client would.
pub fn tbs_and_signature(cert: &[u8]) -> Option<(Vec<u8>, [u8; 64])> {
    fn tlv(b: &[u8]) -> Option<(u8, &[u8], &[u8], usize)> {
        let tag = *b.first()?;
        let first = *b.get(1)?;
        let (len, at) = if first < 0x80 {
            (first as usize, 2)
        } else {
            let n = (first & 0x7F) as usize;
            let mut l = 0usize;
            for i in 0..n {
                l = (l << 8) | *b.get(2 + i)? as usize;
            }
            (l, 2 + n)
        };
        let content = b.get(at..at + len)?;
        Some((tag, content, b.get(at + len..)?, at + len))
    }
    let (_, body, _, _) = tlv(cert)?;
    let (_, _, rest, tbs_len) = tlv(body)?;
    let tbs = body[..tbs_len].to_vec();
    let (_, _, rest, _) = tlv(rest)?;
    let (_, bits, _, _) = tlv(rest)?;
    let (_, ints, _, _) = tlv(bits.get(1..)?)?;
    let (_, r, rest, _) = tlv(ints)?;
    let (_, s, _, _) = tlv(rest)?;
    let mut rs = [0u8; 64];
    let r = r.strip_prefix(&[0]).unwrap_or(r);
    let s = s.strip_prefix(&[0]).unwrap_or(s);
    rs[32 - r.len()..32].copy_from_slice(r);
    rs[64 - s.len()..].copy_from_slice(s);
    Some((tbs, rs))
}

pub fn server_request(not_before: u64, not_after: u64) -> x509::Request<'static> {
    x509::Request {
        name: "nos 10.0.100.1",
        ips: &[[10, 0, 100, 1], [65, 108, 38, 184]],
        dns: &["nos"],
        serial: [0xA5; 16],
        not_before,
        not_after,
        usage: x509::Usage::Server,
    }
}

fn x509_vectors(f: &mut Fails) {
    let secret = [7u8; 32];
    let id = match x509::self_signed(&secret, &server_request(1_761_000_000, 1_761_000_000 + 825 * 86_400)) {
        Ok(id) => id,
        Err(e) => {
            f.check("a server's certificate made", false, e);
            return;
        }
    };
    let signer = SigningKey::from_bytes((&secret).into()).expect("a key");
    let point = signer.verifying_key().to_encoded_point(false);
    let spki = x509::spki(point.as_bytes()).expect("its SubjectPublicKeyInfo");
    f.check("the SubjectPublicKeyInfo found where it was put", x509::find_spki(&id.cert) == Some(&spki[..]), ());
    match tbs_and_signature(&id.cert) {
        Some((tbs, rs)) => {
            let sig = Signature::from_slice(&rs);
            f.check("the signature checks against the key", sig.is_ok_and(|s| signer.verifying_key().verify(&tbs, &s).is_ok()), ());
        }
        None => f.check("the certificate read the long way", false, ()),
    }
    let back = p256::SecretKey::from_pkcs8_der(&id.key);
    f.check("the PKCS#8 key reads back as the key", back.as_ref().is_ok_and(|k| k.to_bytes()[..] == secret[..]), back.is_ok());
    let again = x509::self_signed(&secret, &server_request(1_761_000_000, 1_761_000_000 + 825 * 86_400));
    f.check("a certificate made again is the same (RFC 6979)", again.is_ok_and(|a| a.cert == id.cert), ());

    let pem = x509::pem("CERTIFICATE", &id.cert).expect("PEM");
    f.check("PEM lines of 64 at most", pem.lines().all(|l| l.len() <= 64), ());
    f.check("PEM reads back", x509::pem_decode("CERTIFICATE", &pem).as_deref() == Some(&id.cert[..]), ());
    f.check("PEM of another label is none", x509::pem_decode("PRIVATE KEY", &pem).is_none(), ());

    f.check("a key of zeros refused", matches!(x509::self_signed(&[0; 32], &server_request(0, 1)), Err(x509::Error::BadKey)), ());
    f.check("a key past the order refused", matches!(x509::self_signed(&[0xFF; 32], &server_request(0, 1)), Err(x509::Error::BadKey)), ());
    f.check("an end before the start refused", matches!(x509::self_signed(&secret, &server_request(10, 5)), Err(x509::Error::BadTime)), ());
    f.check("a date past 9999 refused", matches!(x509::self_signed(&secret, &server_request(0, 253_402_300_800)), Err(x509::Error::BadTime)), ());

    let has = |cert: &[u8], needle: &[u8]| cert.windows(needle.len()).any(|w| w == needle);
    let times = [
        (0u64, b"\x17\x0d700101000000Z".to_vec()),
        (951_782_400, b"\x17\x0d000229000000Z".to_vec()),
        (1_761_043_599, b"\x17\x0d251021104639Z".to_vec()),
        (2_524_607_999, b"\x17\x0d491231235959Z".to_vec()),
        (2_524_608_000, b"\x18\x0f20500101000000Z".to_vec()),
        (4_102_444_800, b"\x18\x0f21000101000000Z".to_vec()),
        (253_402_300_799, b"\x18\x0f99991231235959Z".to_vec()),
    ];
    for (secs, want) in times {
        let c = x509::self_signed(&secret, &server_request(secs, secs));
        f.check("a time as RFC 5280 has it", c.is_ok_and(|c| has(&c.cert, &want)), (secs, String::from_utf8_lossy(&want)));
    }
    /* RFC 5280 4.1.2.2: the serial a positive INTEGER of at most 20 bytes --
     * after the version, 16 bytes of it, the top bit of the first clear */
    let serial_at = id.cert.windows(7).position(|w| w == [0xA0, 0x03, 0x02, 0x01, 0x02, 0x02, 0x10]);
    f.check("the serial positive, 16 bytes", serial_at.is_some_and(|at| id.cert.get(at + 7).is_some_and(|&b| b & 0x80 == 0 && b != 0)),
            serial_at);
    let fp = x509::fingerprint_hex(&x509::sha256(&id.cert));
    f.check("a fingerprint as OpenSSL prints one", fp.len() == 95 && fp.as_bytes()[2] == b':', &fp);
    f.check("no SubjectPublicKeyInfo in what is no certificate", x509::find_spki(b"\x30\x03\x02\x01\x01").is_none(), ());
}
