//! A host enrolled into a panel by one command run on it. The panel's key is
//! let in as an admin, its address alone is allowed on the API's interface,
//! and a one-time token is taken back to the panel over TLS pinned by its key.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;

use kcore::cmd::Output;
use kcore::net::Nic;
use kcore::tcp::{TcpStream, RECV_TIMEOUT};
use tls::pinned::{Client as TlsClient, Flush, Read, Recv, Socket};

use super::files::{self, Role};
use super::{config, State};

const USAGE: &str = "usage: api enroll <panel-ip:port> <panel-key> <token> <name> [nic=eth0]";
const TIMEOUT_MS: u64 = 15_000;
const RESPONSE_MAX: usize = 4096;
const WRITE_PENDING_MAX: usize = 16 * 1024;

struct Sock(TcpStream);

impl Socket for Sock {
    fn recv(&mut self, buf: &mut [u8], wait_ms: u64) -> Recv {
        match self.0.recv(buf, wait_ms) {
            n if n > 0 => Recv::Data(n as usize),
            0 => Recv::End,
            RECV_TIMEOUT => Recv::Idle,
            _ => Recv::Failed,
        }
    }

    fn send(&mut self, buf: &[u8], wait_ms: u64) -> Option<usize> {
        let n = self.0.send(buf, wait_ms);
        if n >= 0 {
            Some(n as usize)
        } else {
            None
        }
    }
}

fn valid_token(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|b| b.is_ascii_hexdigit())
}

fn valid_name(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 63
        && text
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn panel_addr(text: &str) -> Option<(u32, u16)> {
    let (ip, port) = text.rsplit_once(':')?;
    let ip = web::cidr::parse_ipv4(ip)?;
    let port = port.parse::<u16>().ok().filter(|&p| p != 0)?;
    Some((ip, port))
}

fn let_panel_in(state: &State, key: [u8; 32]) -> Result<(), String> {
    let hex = files::hex(&key);
    let name = alloc::format!("panel-{}", &hex[..8]);
    state.files.with(|_| -> Result<(), String> {
        let mut list = files::read_clients(&state.dir)?;
        if let Some(c) = list.iter().find(|c| c.key == key) {
            if c.role != Role::Admin {
                return Err(String::from(
                    "the panel's key is let in read-only already -- api client del it first",
                ));
            }
            return Ok(());
        }
        if list.iter().any(|c| c.name == name) {
            return Err(alloc::format!(
                "{} is another client's name already -- api client del it first",
                name
            ));
        }
        if list.len() == files::CLIENTS_LIMIT {
            return Err(alloc::format!(
                "{} clients is all there may be",
                files::CLIENTS_LIMIT
            ));
        }
        let mut text = String::new();
        let _ = write!(text, "{} admin {}", hex, name);
        list.extend(files::parse_clients(&text)?);
        files::write_clients(&state.dir, &list)
    })
}

fn configure(state: &State, nic: &str, panel_ip: u32, panel_key: [u8; 32]) -> Result<(), String> {
    let_panel_in(state, panel_key)?;
    let path = state.dir.path("api.toml");
    let mut text = match kcore::fs::read(&path, config::MAX_BYTES) {
        Ok(bytes) => {
            String::from_utf8(bytes).map_err(|_| alloc::format!("{} is not UTF-8", path))?
        }
        Err(kcore::error::Error::NotFound) => String::new(),
        Err(_) => return Err(alloc::format!("cannot read {}", path)),
    };
    if let Some(new) =
        config::listening(&text, nic).map_err(|e| alloc::format!("{}: {}", path, e))?
    {
        text = new;
    }
    if let Some(new) =
        config::allowing(&text, nic, panel_ip).map_err(|e| alloc::format!("{}: {}", path, e))?
    {
        text = new;
    }
    kcore::fs::write(&path, text.as_bytes())
        .map_err(|_| alloc::format!("cannot write {}", path))?;
    Ok(())
}

fn call_panel(
    nic: &str,
    panel_ip: u32,
    panel_port: u16,
    panel_key: [u8; 32],
    token: &str,
    name: &str,
    api_url: &str,
    host_key: &str,
    identity: &files::Identity,
) -> Result<(), String> {
    let device = Nic::find(nic).ok_or_else(|| alloc::format!("no interface {}", nic))?;
    let socket = TcpStream::connect(&device, panel_ip, panel_port).ok_or_else(|| {
        alloc::format!(
            "cannot connect to the panel at {}:{}",
            super::http::Ip(panel_ip),
            panel_port
        )
    })?;
    let client = TlsClient::new(&identity.cert, &identity.key, &[panel_key], &[])
        .map_err(|e| alloc::format!("cannot make the panel's TLS client: {:?}", e))?;
    let mut session = client
        .connect(Sock(socket), panel_ip, TIMEOUT_MS)
        .map_err(|(_, e)| alloc::format!("the panel's TLS handshake was refused: {}", e.name()))?;
    let path = alloc::format!(
        "/api/enroll?token={}&name={}&url={}&pin={}",
        token,
        name,
        api_url,
        host_key
    );
    let request = alloc::format!(
        "GET {} HTTP/1.1\r\nHost: {}:{}\r\nConnection: close\r\n\r\n",
        path,
        super::http::Ip(panel_ip),
        panel_port
    );
    if !session.write(request.as_bytes(), WRITE_PENDING_MAX)
        || session.flush(TIMEOUT_MS) != Flush::Done
    {
        return Err(String::from(
            "the enrollment could not be sent to the panel",
        ));
    }
    let mut response = Vec::new();
    response
        .try_reserve_exact(RESPONSE_MAX)
        .map_err(|_| String::from("out of memory"))?;
    response.resize(RESPONSE_MAX, 0);
    let mut used = 0;
    let status = loop {
        if used == response.len() {
            return Err(String::from("the panel's answer is too long"));
        }
        match session.read(&mut response[used..], TIMEOUT_MS) {
            Read::Data(n) => {
                used += n;
                let text = core::str::from_utf8(&response[..used]).unwrap_or("");
                if let Some(line) = text.split("\r\n").next() {
                    if let Some(status) = line
                        .split_whitespace()
                        .nth(1)
                        .and_then(|s| s.parse::<u16>().ok())
                    {
                        break status;
                    }
                }
            }
            Read::End => return Err(String::from("the panel closed without an HTTP status")),
            Read::Idle => return Err(String::from("the panel did not answer in time")),
            Read::Failed => return Err(String::from("the panel's TLS session failed")),
        }
    };
    let _ = session.close(1_000);
    if !(200..300).contains(&status) {
        return Err(alloc::format!(
            "the panel refused the enrollment: HTTP {}",
            status
        ));
    }
    Ok(())
}

pub fn run<'a>(
    state: &State,
    mut words: impl Iterator<Item = &'a str>,
    out: &mut Output,
) -> Result<(), String> {
    let (Some(at), Some(key), Some(token), Some(name), extra) = (
        words.next(),
        words.next(),
        words.next(),
        words.next(),
        words.next(),
    ) else {
        return Err(String::from(USAGE));
    };
    let mut nic = "eth0";
    match (extra, words.next()) {
        (None, None) => {}
        (Some(word), None) => {
            nic = word
                .strip_prefix("nic=")
                .filter(|n| !n.is_empty())
                .ok_or_else(|| String::from(USAGE))?;
        }
        _ => return Err(String::from(USAGE)),
    }
    let (panel_ip, panel_port) = panel_addr(at).ok_or_else(|| String::from(USAGE))?;
    let panel_key =
        files::parse_hex(key).ok_or_else(|| String::from("the panel's key is 64 hex digits"))?;
    if !valid_token(token) {
        return Err(String::from(
            "the enrollment token is 64 hex digits, as the panel gives it",
        ));
    }
    if !valid_name(name) {
        return Err(String::from(
            "the host's name is lowercase letters, digits and -, up to 63 characters",
        ));
    }
    let device = Nic::find(nic).ok_or_else(|| alloc::format!("no interface {}", nic))?;
    let api_ip = device.ip();
    if api_ip == 0 {
        return Err(alloc::format!(
            "{} has no address for the panel to reach",
            nic
        ));
    }
    configure(state, nic, panel_ip, panel_key)?;
    if state.server.with(|s| s.is_some()) {
        super::stop(state, out)?;
    }
    super::start(state, out)?;
    let config = super::read_config(&state.dir)?;
    let api_url = alloc::format!("{}:{}", super::http::Ip(api_ip), config.port);
    let (identity, _) = state
        .files
        .with(|_| files::identity(&state.dir, &super::cert_ips()))?;
    let host_key = files::hex(&identity.key_fingerprint);
    call_panel(
        nic, panel_ip, panel_port, panel_key, token, name, &api_url, &host_key, &identity,
    )?;
    let _ = writeln!(
        out,
        "api: {} enrolled into the panel at {}:{} as {}",
        name,
        super::http::Ip(panel_ip),
        panel_port,
        api_url
    );
    Ok(())
}
