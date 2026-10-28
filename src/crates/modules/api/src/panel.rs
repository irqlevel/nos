//! `api panel`: the management panel -- a VM of hv's, made of a cloud's
//! image (Debian's, unless told), which cloud-init makes the panel from the
//! seed made here. The panel is a client of this API like any other.
//!
//!     api panel                   its VM as hv has it, and where it is
//!     api panel create [image=<url>] [bundle=<url> sha256=<hex>] [port=<n>]
//!                      [memory=<MiB>]
//!
//! `create` makes what the panel needs of the host, and has hv's `vm`
//! provider make the VM -- a job, which downloads the image, makes the VM's
//! disk of it, and starts it:
//!
//! - the panel's identity as this API's client: a key and a certificate for
//!   it, let in as `panel`, an admin (`clients`); the key goes nowhere but
//!   the seed;
//! - its certificate as a server, which a browser is shown: made here, so
//!   that what the browser is shown can be held to the fingerprint `api
//!   panel` prints (`panel.pem`, the certificate alone, is kept for that);
//! - the seed: those, the API's address on hv0 and its key, root's
//!   password's hash (`panel.pass`, which nos-install makes), and a script
//!   that fetches the panel's bundle, holds it to its SHA-256 -- the system
//!   image's `/lib/panel/bundle` says both, unless told -- and installs it;
//! - the VM: a port of the switch of its own, the highest free one unless
//!   told, so that its address is the same at every start -- `allow_hv0` in
//!   api.toml lets that address in, and nothing else where nothing was let
//!   in before -- nos's port 39233 relayed to its 443, `protected`, booted
//!   again when it resets itself.
//!
//! A panel there already is started where it is not running: /etc/rc runs
//! `api panel create` at every boot.

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt::Write;

use kcore::cmd::Output;
use kcore::mgmt::{self as bus, Status, Writer};
use kcore::net::Nic;
use mgmtwire::json::{self, Value};
use mgmtwire::Limits;

use super::config;
use super::files::{self, Client, Role};
use super::http::Ip;
use super::State;

/// The VM's name, and the client's in `clients`.
pub const NAME: &str = "panel";
/// nos's port a browser reaches the panel on, relayed to its HTTPS.
pub const PUBLIC_PORT: u16 = 39233;
const GUEST_PORT: u16 = 443;
/// The VM's memory, unless told: Node and Caddy, and Debian under them.
const MEMORY_MIB: u64 = 1024;
const MEMORY_MIN_MIB: u64 = 64;
/// The interface the relay listens on, and the switch's.
const PUBLIC_IF: &str = "eth0";
const SWITCH_IF: &str = "hv0";
/// The image the VM is made of, unless told: Debian's, as its cloud has it,
/// which boots under hv through hvboot.
const DEFAULT_IMAGE: &str = "https://cloud.debian.org/images/cloud/trixie/latest/debian-13-genericcloud-amd64.qcow2";
/// What the system image says of the panel released with it: its bundle's
/// URL and SHA-256, `url=` and `sha256=` lines.
const BUNDLE_FILE: &str = "/lib/panel/bundle";
const BUNDLE_FILE_MAX: usize = 4096;
/// root's password, as nos-install hashed it: `scrypt$N$r$p$salt$hash`.
const PASSWORD_FILE: &str = "panel.pass";
const PASSWORD_MAX: usize = 512;
/// The panel's certificate as a server, kept for `api panel` to show.
const CERT_FILE: &str = "panel.pem";
const CERT_FILE_MAX: usize = 16 * 1024;
/// The keys nos's sshd lets in, which the VM lets in too.
const AUTHORIZED_KEYS: &str = "/etc/ssh/authorized_keys";
const KEYS_MAX: usize = 16 * 1024;
const URL_MAX: usize = 1024;
/// The API lets a client in by its key alone and never looks at its
/// certificate's dates: ten years, so the panel's never ends.
const CLIENT_DAYS: u64 = 3650;
/// What a browser is shown: as long as the host's own certificate is good.
const SERVER_DAYS: u64 = 825;
/// The name the panel's certificate is for besides the host's address.
const SERVER_NAME: &str = "nos-panel";
/// The panel's name for this host, until it is told another.
const HOST_NAME: &str = "nos";
/// A provider's answer, as it is read here.
const ANSWER: Limits = Limits { bytes: 256 * 1024, depth: 16, values: 1 << 16, members: 4096, string: 64 * 1024 };

/// The script the seed has cloud-init run: the bundle fetched -- tried
/// again while the VM's network comes up -- held to the SHA-256 nos gave,
/// unpacked, and installed by its own install.sh.
const FETCH: &str = "#!/bin/sh
# nos's management panel: its bundle fetched, held to the SHA-256 nos gave,
# and installed by its own install.sh. cloud-init runs it once; run it again
# by hand to install it anew.
set -eu
url=$1
sum=$2
tgz=/var/tmp/nos-panel.tar.gz
dir=/opt/nos-panel
tries=0
until python3 -c 'import sys, urllib.request; urllib.request.urlretrieve(sys.argv[1], sys.argv[2])' \"$url\" \"$tgz\"; do
    tries=$((tries + 1))
    if [ \"$tries\" -ge 30 ]; then
        echo \"nos-panel: $url could not be fetched\" >&2
        exit 1
    fi
    sleep 10
done
echo \"$sum  $tgz\" | sha256sum -c -
rm -rf \"$dir.new\"
mkdir -p \"$dir.new\"
tar -xzf \"$tgz\" -C \"$dir.new\"
rm -f \"$tgz\"
rm -rf \"$dir.old\"
if [ -d \"$dir\" ]; then mv \"$dir\" \"$dir.old\"; fi
mv \"$dir.new\" \"$dir\"
exec \"$dir/install.sh\"
";

pub fn run<'a>(state: &State, mut words: impl Iterator<Item = &'a str>, out: &mut Output) -> Result<(), String> {
    match words.next() {
        None => show(state, out),
        Some("create") => create(state, words, out),
        Some(_) => Err(String::from(super::USAGE)),
    }
}

/// A call of the bus: its answer, or why not -- the status and what the
/// provider said of it.
fn ask(provider: &str, method: &str, params: &[u8]) -> Result<Value, (Status, String)> {
    let mut got = Vec::new();
    let status = bus::call(provider, method, params, ANSWER.bytes, &mut |b| {
        got.try_reserve(b.len()).is_ok() && {
            got.extend_from_slice(b);
            true
        }
    });
    let value = json::parse(&got, &ANSWER).ok();
    if status == Status::Ok {
        return value.ok_or((Status::Failed, String::from("an answer that is not JSON")));
    }
    let said = value.as_ref().and_then(|v| v.get("error")).and_then(|e| e.get("message")).and_then(Value::as_str);
    Err((status, said.unwrap_or(status.name()).to_string()))
}

/// JSON as the bus writes it.
fn to_json(write: impl FnOnce(&mut Writer)) -> Result<Vec<u8>, String> {
    let mut text = Vec::new();
    let mut sink = |b: &[u8]| {
        text.try_reserve(b.len()).is_ok() && {
            text.extend_from_slice(b);
            true
        }
    };
    let mut w = Writer::new(&mut sink);
    write(&mut w);
    if !w.ok() {
        return Err(String::from("out of memory"));
    }
    Ok(text)
}

/// The panel's VM as hv's provider has it -- `vm.get`'s `vm`: None where
/// there is none.
fn panel_vm() -> Result<Option<Value>, String> {
    let params = to_json(|w| {
        w.begin_object();
        w.field_str("name", NAME);
        w.end_object();
    })?;
    match ask("vm", "get", &params) {
        Ok(answer) => answer.get("vm").cloned().map(Some)
            .ok_or_else(|| alloc::format!("vm get {} says nothing of the VM", NAME)),
        Err((Status::NotFound, _)) => Ok(None),
        Err((Status::NoProvider, _)) => Err(String::from("no vm provider: the panel is a VM of hv's -- insmod \
                                                          /lib/modules/hv.ko, and hv on")),
        Err((_, said)) => Err(alloc::format!("vm get {}: {}", NAME, said)),
    }
}

/// An address the provider writes, dotted.
fn ipv4(text: &str) -> Option<u32> {
    let mut ip = 0u32;
    let mut parts = 0;
    for part in text.split('.') {
        if part.is_empty() || part.len() > 3 || !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        ip = (ip << 8) | u32::from(part.parse::<u8>().ok()?);
        parts += 1;
    }
    (parts == 4).then_some(ip)
}

fn nic_ip(name: &str) -> Option<u32> {
    Nic::find(name).map(|n| n.ip()).filter(|&ip| ip != 0)
}

/// The panel's certificate's fingerprint, as a browser shows it, if it is
/// kept.
fn cert_fingerprint(state: &State) -> Option<String> {
    let bytes = kcore::fs::read(&state.dir.path(CERT_FILE), CERT_FILE_MAX).ok()?;
    let der = web::x509::pem_decode("CERTIFICATE", core::str::from_utf8(&bytes).ok()?)?;
    Some(web::x509::fingerprint_hex(&web::x509::sha256(&der)))
}

fn show(state: &State, out: &mut Output) -> Result<(), String> {
    let Some(vm) = panel_vm()? else {
        let _ = writeln!(out, "api: no panel -- api panel create makes it");
        return Ok(());
    };
    let field = |k: &str| vm.get(k).and_then(Value::as_str).unwrap_or("");
    let _ = write!(out, "api: the panel's VM is {}", field("state"));
    if !field("being").is_empty() {
        let _ = write!(out, " -- being {}", field("being"));
    }
    if let Some(port) = vm.get("port").and_then(Value::as_u64) {
        let _ = write!(out, ", port {} of the switch, {}", port, field("ip"));
    }
    let _ = writeln!(out);
    match nic_ip(PUBLIC_IF) {
        Some(ip) => {
            let _ = writeln!(out, "api: https://{}:{}", Ip(ip), PUBLIC_PORT);
        }
        None => {
            let _ = writeln!(out, "api: on {}'s port {}, once it has an address", PUBLIC_IF, PUBLIC_PORT);
        }
    }
    if let Some(f) = cert_fingerprint(state) {
        let _ = writeln!(out, "api: its certificate, as a browser shows it: SHA-256 {}", f);
    }
    Ok(())
}

/// What `create` is told.
struct Asked<'a> {
    /// Whether anything was asked: a panel there already takes none of it.
    any: bool,
    image: &'a str,
    bundle: Option<&'a str>,
    sha256: Option<&'a str>,
    port: Option<u64>,
    memory_mib: u64,
}

fn asked<'a>(words: impl Iterator<Item = &'a str>) -> Result<Asked<'a>, String> {
    let mut a = Asked { any: false, image: DEFAULT_IMAGE, bundle: None, sha256: None, port: None,
                        memory_mib: MEMORY_MIB };
    for word in words {
        a.any = true;
        match word.split_once('=') {
            Some(("image", v)) => a.image = v,
            Some(("bundle", v)) => a.bundle = Some(v),
            Some(("sha256", v)) => a.sha256 = Some(v),
            Some(("port", v)) => a.port = Some(v.parse::<u64>().map_err(|_| String::from("port= is a number"))?),
            Some(("memory", v)) => {
                a.memory_mib = v.parse::<u64>().ok().filter(|&m| m >= MEMORY_MIN_MIB)
                    .ok_or_else(|| alloc::format!("memory= is MiB, {} at least", MEMORY_MIN_MIB))?;
            }
            _ => return Err(alloc::format!("what is '{}'?\n{}", word, super::USAGE)),
        }
    }
    if !url(a.image) {
        return Err(String::from("image= is an http:// or https:// URL"));
    }
    Ok(a)
}

/// A URL as the seed may carry it: http or https, and nothing YAML or a
/// shell would read as more than a word.
fn url(text: &str) -> bool {
    (text.starts_with("https://") || text.starts_with("http://")) && text.len() <= URL_MAX
        && text.bytes().all(|b| b.is_ascii_graphic() && !matches!(b, b'"' | b'\'' | b'\\' | b'`'))
}

fn sha256_hex(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The bundle: as told, or as the system image says -- both its URL and
/// its SHA-256, or neither.
fn bundle(asked: &Asked) -> Result<(String, String), String> {
    let (url_text, sum) = match (asked.bundle, asked.sha256) {
        (Some(u), Some(s)) => (String::from(u), String::from(s)),
        (None, None) => {
            let bytes = kcore::fs::read(BUNDLE_FILE, BUNDLE_FILE_MAX).map_err(|_| {
                alloc::format!("no {}: this system image was not released with a panel -- bundle=<url> \
                                sha256=<hex> say which", BUNDLE_FILE)
            })?;
            let text = core::str::from_utf8(&bytes).map_err(|_| alloc::format!("{} is not text", BUNDLE_FILE))?;
            let line = |key: &str| {
                text.lines().find_map(|l| l.trim().strip_prefix(key).and_then(|r| r.strip_prefix('='))).map(str::trim)
            };
            match (line("url"), line("sha256")) {
                (Some(u), Some(s)) => (String::from(u), String::from(s)),
                _ => return Err(alloc::format!("{} says no url= and sha256=", BUNDLE_FILE)),
            }
        }
        _ => return Err(String::from("bundle= and sha256= go together")),
    };
    if !url(&url_text) {
        return Err(alloc::format!("the bundle's URL, {}, is no http:// or https:// URL", url_text));
    }
    if !sha256_hex(&sum) {
        return Err(String::from("a bundle's SHA-256 is 64 hex digits"));
    }
    Ok((url_text, sum.to_ascii_lowercase()))
}

/// root's password's hash, as nos-install wrote it.
fn password_hash(state: &State) -> Result<String, String> {
    let path = state.dir.path(PASSWORD_FILE);
    let bytes = kcore::fs::read(&path, PASSWORD_MAX).map_err(|_| {
        alloc::format!("no {}: the hash of root's password for the panel -- nos-install --panel makes it, and prints \
                        the password once", path)
    })?;
    let text = core::str::from_utf8(&bytes).map_err(|_| alloc::format!("{} is not text", path))?.trim();
    let parts: Vec<&str> = text.split('$').collect();
    let ok = parts.len() == 6 && parts[0] == "scrypt"
        && parts[1..4].iter().all(|n| !n.is_empty() && n.len() <= 8 && n.bytes().all(|b| b.is_ascii_digit()))
        && parts[4..].iter().all(|b| !b.is_empty() && b.bytes().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'+' | b'/' | b'=')));
    if !ok {
        return Err(alloc::format!("{} is not scrypt$<N>$<r>$<p>$<salt>$<hash>", path));
    }
    Ok(String::from(text))
}

/// Where the panel is on the guests' switch: its port -- as told, or the
/// highest no VM has, since a VM without a port of its own takes the lowest
/// free one as it starts -- that port's address, and the host's, which the
/// panel reaches the API at. hv's to say: the switch, and hv0 with it, is
/// made only as the first VM starts.
struct Place {
    port: u64,
    vm_ip: u32,
    host_ip: u32,
}

fn place(asked: Option<u64>) -> Result<Place, String> {
    let host = ask("vm", "host", b"{}").map_err(|(_, said)| alloc::format!("vm host: {}", said))?;
    let host_ip = host.get("host_ip").and_then(Value::as_str).and_then(ipv4)
        .ok_or_else(|| String::from("vm host says of no address of the host's on the switch"))?;
    let ips: Vec<&str> = host.get("port_ips").and_then(Value::as_list).unwrap_or(&[]).iter()
        .filter_map(Value::as_str).collect();
    if ips.is_empty() {
        return Err(String::from("vm host says of no port's address"));
    }
    let list = ask("vm", "list", b"{}").map_err(|(_, said)| alloc::format!("vm list: {}", said))?;
    let mut used = Vec::new();
    used.try_reserve_exact(ips.len()).map_err(|_| String::from("out of memory"))?;
    used.resize(ips.len(), false);
    for vm in list.get("vms").and_then(Value::as_list).unwrap_or(&[]) {
        if let Some(u) = vm.get("port").and_then(Value::as_u64).and_then(|p| used.get_mut(p as usize)) {
            *u = true;
        }
    }
    let port = match asked {
        Some(p) if p as usize >= ips.len() => return Err(alloc::format!("port= is 0..{}", ips.len() - 1)),
        Some(p) => p,
        None => used.iter().rposition(|u| !u).ok_or_else(|| String::from("every port of the switch is a VM's"))? as u64,
    };
    let vm_ip = ipv4(ips[port as usize]).ok_or_else(|| String::from("vm host says of an address that is none"))?;
    Ok(Place { port, vm_ip, host_ip })
}

/// The keys nos's sshd lets in: the file's lines that are not comments.
fn ssh_keys() -> Vec<String> {
    let Ok(text) = kcore::fs::read(AUTHORIZED_KEYS, KEYS_MAX) else { return Vec::new() };
    let Ok(text) = core::str::from_utf8(&text) else { return Vec::new() };
    text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).map(String::from).collect()
}

/// `"text"` as YAML has a double-quoted string: read as written, never as
/// YAML.
fn quoted(text: &str) -> String {
    let mut q = String::from("\"");
    for c in text.chars() {
        match c {
            '"' => q.push_str("\\\""),
            '\\' => q.push_str("\\\\"),
            c if (c as u32) < 0x20 || c as u32 == 0x7F => {
                let _ = write!(q, "\\x{:02x}", c as u32);
            }
            c => q.push(c),
        }
    }
    q.push('"');
    q
}

/// A file cloud-init writes: its text a literal block, every line as it is.
fn yaml_file(u: &mut String, path: &str, mode: &str, content: &str) {
    let _ = writeln!(u, "  - path: {}\n    permissions: '{}'\n    content: |", path, mode);
    for line in content.lines() {
        if line.is_empty() {
            u.push('\n');
        } else {
            let _ = writeln!(u, "      {}", line);
        }
    }
}

/// The VM's user-data: cloud-config writing the seed, the panel's
/// certificate and key as a server and the script that installs it, and
/// running that script once.
fn user_data(seed: &str, cert: &str, key: &str, keys: &[String], bundle: &str, sum: &str) -> String {
    let mut u = String::from("#cloud-config\n# nos's management panel, made by `api panel create` on the host it \
                              manages: /etc/nos-panel/seed.json is that host's API -- where it is, its key -- the \
                              panel's identity there, and root's password's hash.\n");
    if !keys.is_empty() {
        u.push_str("ssh_authorized_keys:\n");
        for k in keys {
            let _ = writeln!(u, "  - {}", quoted(k));
        }
    }
    u.push_str("write_files:\n");
    yaml_file(&mut u, "/etc/nos-panel/seed.json", "0600", seed);
    yaml_file(&mut u, "/etc/nos-panel/tls/cert.pem", "0644", cert);
    yaml_file(&mut u, "/etc/nos-panel/tls/key.pem", "0600", key);
    yaml_file(&mut u, "/usr/local/sbin/nos-panel-fetch", "0755", FETCH);
    let _ = writeln!(u, "runcmd:\n  - [/usr/local/sbin/nos-panel-fetch, {}, {}]", quoted(bundle), quoted(sum));
    u
}

fn pem(label: &str, der: &[u8]) -> Result<String, String> {
    web::x509::pem(label, der).map_err(|_| String::from("out of memory"))
}

/// The panel's VM made: what of the host it needs, then the VM, by a job of
/// hv's provider -- or the panel there already started, if it is not
/// running.
fn create<'a>(state: &State, words: impl Iterator<Item = &'a str>, out: &mut Output) -> Result<(), String> {
    let asked = asked(words)?;
    if let Some(vm) = panel_vm()? {
        if asked.any {
            let _ = writeln!(out, "api: the panel is there already: what was asked is for a new one -- hv delete \
                                   panel force, then api panel create");
        }
        return start(&vm, out);
    }
    let password = password_hash(state)?;
    let (bundle_url, sum) = bundle(&asked)?;
    let config = super::read_config(&state.dir)?;
    let api_port = config.port;
    if !config.listen.iter().any(|n| n == SWITCH_IF) {
        let _ = writeln!(out, "api: api.toml's listen has no {}: the panel reaches the API there, and cannot until it \
                               does", SWITCH_IF);
    }
    let (host, _) = state.files.with(|_| files::identity(&state.dir, &super::cert_ips()))?;
    let Place { port, vm_ip, host_ip } = place(asked.port)?;
    let public = nic_ip(PUBLIC_IF);

    /* The panel's two identities: a client of the API's, a server to the
     * browser. Their keys go into the seed, and nowhere else. */
    let client = files::new_key("nos panel", &[], &[], CLIENT_DAYS, web::x509::Usage::Client)?;
    let client_key = tls::server::fingerprint(&client.cert).ok_or_else(|| String::from("the panel's certificate \
                                                                                         reads as none"))?;
    let public_ips: Vec<[u8; 4]> = public.into_iter().map(u32::to_be_bytes).collect();
    let server = files::new_key("nos panel", &public_ips, &[SERVER_NAME], SERVER_DAYS, web::x509::Usage::Server)?;
    let client_cert_pem = pem("CERTIFICATE", &client.cert)?;
    let client_key_pem = zeroize::Zeroizing::new(pem("PRIVATE KEY", &client.key)?);
    let server_cert_pem = pem("CERTIFICATE", &server.cert)?;
    let server_key_pem = zeroize::Zeroizing::new(pem("PRIVATE KEY", &server.key)?);

    let seed = to_json(|w| {
        w.begin_object();
        w.field_str("host", HOST_NAME);
        w.field_str("host_url", &alloc::format!("{}:{}", Ip(host_ip), api_port));
        w.field_str("host_pin", &files::hex(&host.key_fingerprint));
        w.field_str("host_id", &host.id);
        w.field_str("client_cert", &client_cert_pem);
        w.field_str("client_key", &client_key_pem);
        w.field_str("password_hash", &password);
        w.end_object();
    })?;
    let seed = zeroize::Zeroizing::new(String::from_utf8(seed).map_err(|_| String::from("the seed is not text"))?);
    let user = zeroize::Zeroizing::new(user_data(&seed, &server_cert_pem, &server_key_pem, &ssh_keys(), &bundle_url,
                                                 &sum));
    let forward = alloc::format!("{}:{}", PUBLIC_PORT, GUEST_PORT);
    let params = zeroize::Zeroizing::new(to_json(|w| {
        w.begin_object();
        w.field_str("name", NAME);
        w.field_str("url", asked.image);
        w.field_uint("memory_mib", asked.memory_mib);
        w.field_uint("cpus", 1);
        w.field_uint("port", port);
        w.key("forwards");
        w.begin_list();
        w.str(&forward);
        w.end_list();
        w.field_bool("protected", true);
        w.field_bool("start", true);
        w.field_str("user_data", &user);
        w.end_object();
    })?);

    /* What the host keeps of it, each undone if the job does not start:
     * the client let in, the certificate shown, the VM's address let in. */
    let before = state.files.with(|_| -> Result<Vec<Client>, String> {
        let before = files::read_clients(&state.dir)?;
        let mut list: Vec<Client> = before.iter().filter(|c| c.name != NAME).cloned().collect();
        if list.len() == files::CLIENTS_LIMIT {
            return Err(alloc::format!("{} clients is all there may be", files::CLIENTS_LIMIT));
        }
        list.push(Client { key: client_key, role: Role::Admin, name: String::from(NAME) });
        let _ = kcore::fs::create_dir(&state.dir.root);
        files::write_clients(&state.dir, &list)?;
        Ok(before)
    })?;
    let toml_path = state.dir.path("api.toml");
    let toml_before = match kcore::fs::read(&toml_path, config::MAX_BYTES) {
        Ok(b) => Some(String::from_utf8(b).map_err(|_| alloc::format!("{} is not UTF-8", toml_path))),
        Err(kcore::error::Error::NotFound) => None,
        Err(_) => Some(Err(alloc::format!("cannot read {}", toml_path))),
    };
    let made = (|| -> Result<(bool, u64), String> {
        let old = match &toml_before {
            Some(Ok(t)) => t.as_str(),
            Some(Err(e)) => return Err(e.clone()),
            None => "",
        };
        let allowed = match config::allowing(old, SWITCH_IF, vm_ip).map_err(|e| alloc::format!("{}: {}", toml_path, e))? {
            Some(text) => {
                kcore::fs::write(&toml_path, text.as_bytes()).map_err(|_| alloc::format!("cannot write {}", toml_path))?;
                true
            }
            None => false,
        };
        let undo = || {
            let _ = match &toml_before {
                Some(Ok(t)) if allowed => kcore::fs::write(&toml_path, t.as_bytes()),
                None if allowed => kcore::fs::remove(&toml_path),
                _ => Ok(()),
            };
        };
        if let Err(e) = kcore::fs::write(&state.dir.path(CERT_FILE), server_cert_pem.as_bytes()) {
            undo();
            return Err(alloc::format!("cannot write {} -- {}", state.dir.path(CERT_FILE), e));
        }
        match bus::job_start("vm", "create", &params) {
            Ok(id) => Ok((allowed, id)),
            Err(s) => {
                undo();
                let _ = kcore::fs::remove(&state.dir.path(CERT_FILE));
                Err(alloc::format!("vm create would not start: {}", s.name()))
            }
        }
    })();
    let (allowed, job) = match made {
        Ok(m) => m,
        Err(e) => {
            let _ = state.files.with(|_| files::write_clients(&state.dir, &before));
            return Err(e);
        }
    };

    /* The running server given both from its next connection on */
    let list = state.files.with(|_| files::read_clients(&state.dir))?;
    super::apply(state, list, out);
    if allowed {
        super::apply_allow(state, super::read_config(&state.dir)?, out);
        let _ = writeln!(out, "api: {} lets {} in on {}", toml_path, Ip(vm_ip), SWITCH_IF);
    }
    let _ = writeln!(out, "api: the panel's VM is being made: job {} -- mgmt job {} follows it; port {} of the \
                           switch, {}", job, job, port, Ip(vm_ip));
    match public {
        Some(ip) => {
            let _ = writeln!(out, "api: https://{}:{} once it has installed itself, minutes after its image is in",
                             Ip(ip), PUBLIC_PORT);
        }
        None => {
            let _ = writeln!(out, "api: on {}'s port {} once it has installed itself", PUBLIC_IF, PUBLIC_PORT);
        }
    }
    let _ = writeln!(out, "api: its certificate, as a browser shows it: SHA-256 {}",
                     web::x509::fingerprint_hex(&web::x509::sha256(&server.cert)));
    Ok(())
}

/// The panel there already: started where it is not running.
fn start(vm: &Value, out: &mut Output) -> Result<(), String> {
    let state = vm.get("state").and_then(Value::as_str).unwrap_or("");
    if !matches!(state, "off" | "stopped") {
        let _ = writeln!(out, "api: the panel's VM is there, {} -- api panel says more", state);
        return Ok(());
    }
    let params = to_json(|w| {
        w.begin_object();
        w.field_str("name", NAME);
        w.end_object();
    })?;
    let job = bus::job_start("vm", "start", &params).map_err(|s| alloc::format!("vm start would not start: {}",
                                                                                 s.name()))?;
    let _ = writeln!(out, "api: the panel's VM is there -- being started: job {}", job);
    Ok(())
}
