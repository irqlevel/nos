//! The `net` provider on the management bus: the network devices and what
//! each has moved, TCP's connections and how many of its slots they take,
//! what ARP knows, the frame pool. Each is read as the shell's commands read
//! it -- a device's counters as they stand, a connection copied under its
//! lock -- and written with no lock held.

use crate::abi;
use crate::device::DEVICES;
use crate::frame::POOL;
use crate::tcp::{self, TCP};
use crate::wire::{Ipv4 as Ip, Mac, MacHex};
use kcore::trace;
use mgmt::{Call, Kind, Method, Params, Provider, Status, Writer};

struct Net;

static NET: Net = Net;

const DESCRIPTOR: &str = include_str!("net.descriptor.json");

static METHODS: [Method; 4] = [
    Method { name: "interfaces", kind: Kind::Read },
    Method { name: "tcp", kind: Kind::Read },
    Method { name: "arp", kind: Kind::Read },
    Method { name: "pool", kind: Kind::Read },
];

/// ARP's entries read at most: its cache has 16.
const ARP_ENTRIES: usize = 32;

/// The provider registered for good: from the layer's init.
pub fn init() {
    match mgmt::register("net", &METHODS, DESCRIPTOR, &NET) {
        Ok(registration) => core::mem::forget(registration),
        Err(status) => trace!(0, "net: cannot register the provider: {}", status.name()),
    }
}

impl Provider for Net {
    fn call(&self, method: &str, params: &[u8], call: &mut Call) -> Status {
        /* None of them takes a parameter */
        let value = match mgmt::parse_params(params) {
            Ok(v) => v,
            Err(s) => return call.refuse(s, "the parameters are not JSON as the bus reads it", None),
        };
        if let Err(e) = Params::new(&value).and_then(Params::finish) {
            return call.invalid(e);
        }
        match method {
            "interfaces" => interfaces(call),
            "tcp" => connections(call),
            "arp" => arp(call),
            "pool" => pool(call),
            _ => Status::NoMethod,
        }
    }
}

/// An address as a client reads it: dotted, or null for none.
fn ip(w: &mut Writer, key: &str, addr: u32) {
    w.key(key);
    if addr == 0 {
        w.null();
    } else {
        w.str(mgmt::text::<16>(format_args!("{}", Ip(addr))).as_str());
    }
}

fn mac(w: &mut Writer, key: &str, m: Mac) {
    w.field_str(key, mgmt::text::<18>(format_args!("{}", MacHex(m))).as_str());
}

fn interfaces(call: &mut Call) -> Status {
    call.answer(|w| {
        w.begin_object();
        w.key("interfaces");
        w.begin_list();
        for index in 0..DEVICES.count() {
            let Some(dev) = DEVICES.at(index) else { continue };
            let st = dev.stats();
            w.begin_object();
            w.field_str_lossy("name", dev.name());
            mac(w, "mac", dev.mac());
            ip(w, "ipv4", dev.ip());
            ip(w, "netmask", dev.mask());
            ip(w, "gateway", dev.gw());
            w.field_uint("rx_frames", st.rx_total as u64);
            w.field_uint("rx_bytes", st.rx_bytes as u64);
            w.field_uint("rx_dropped", st.rx_drop as u64);
            w.field_uint("tx_frames", st.tx_total as u64);
            w.field_uint("tx_bytes", st.tx_bytes as u64);
            w.field_uint("tx_refused", st.tx_refused as u64);
            w.end_object();
        }
        w.end_list();
        w.end_object();
    })
}

fn connections(call: &mut Call) -> Status {
    let st = TCP.stats();
    /* Each copied under its own lock, all of them before any is written */
    let mut conns: [Option<tcp::ConnInfo>; tcp::MAX_CONNECTIONS] = [const { None }; tcp::MAX_CONNECTIONS];
    for (index, slot) in conns.iter_mut().enumerate() {
        *slot = TCP.snapshot(index);
    }
    let used = conns.iter().flatten().count();
    call.answer(|w| {
        w.begin_object();
        w.field_uint("slots", tcp::MAX_CONNECTIONS as u64);
        w.field_uint("used", used as u64);
        w.field_uint("tx_segments", st.tx as u64);
        w.field_uint("rx_segments", st.rx as u64);
        w.field_uint("rx_errors", st.rx_err as u64);
        w.field_uint("retransmits", st.retransmits as u64);
        w.field_uint("opened", st.conns as u64);
        w.key("connections");
        w.begin_list();
        for (index, c) in conns.iter().enumerate() {
            let Some(c) = c else { continue };
            w.begin_object();
            w.field_uint("slot", index as u64);
            w.field_str("state", c.state.name());
            ip(w, "local_ip", c.local_ip);
            w.field_uint("local_port", u64::from(c.local_port));
            ip(w, "remote_ip", c.remote_ip);
            w.field_uint("remote_port", u64::from(c.remote_port));
            w.field_uint("send_queued_bytes", c.send_used as u64);
            w.field_uint("in_flight_bytes", c.in_flight as u64);
            w.field_uint("recv_queued_bytes", c.recv_used as u64);
            w.field_uint("window_bytes", c.window as u64);
            w.end_object();
        }
        w.end_list();
        w.end_object();
    })
}

fn arp(call: &mut Call) -> Status {
    let mut entries = [(0u32, [0u8; 6]); ARP_ENTRIES];
    let count = abi::arp_table().map_or(0, |arp| arp.snapshot(&mut entries));
    call.answer(|w| {
        w.begin_object();
        w.key("entries");
        w.begin_list();
        for (addr, m) in &entries[..count] {
            w.begin_object();
            ip(w, "ipv4", *addr);
            mac(w, "mac", *m);
            w.end_object();
        }
        w.end_list();
        w.end_object();
    })
}

fn pool(call: &mut Call) -> Status {
    let st = POOL.stats();
    call.answer(|w| {
        w.begin_object();
        w.field_bool("ready", st.ready);
        w.field_uint("frames", st.frames as u64);
        w.field_uint("frame_bytes", st.capacity as u64);
        w.field_uint("in_flight", st.in_flight as u64);
        w.field_uint("hits", st.hits as u64);
        w.field_uint("misses", st.misses as u64);
        w.end_object();
    })
}
