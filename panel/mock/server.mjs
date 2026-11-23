#!/usr/bin/env node
// mock/server.mjs: nos's host API (docs/api.md) as the panel sees it, for
// the panel's development and its tests -- the routes under /v1, TLS 1.3
// with the client's certificate required and let in by its key's SHA-256
// alone, the providers' own descriptors read from the tree, and a host whose
// numbers move: CPUs idling, interfaces counting, a log growing, VMs
// starting, stopping and printing on their consoles, jobs that take a few
// seconds and say so. It lets in at once what api.ko lets in with no
// api.toml -- two connections in their handshake, four of one key, eight in
// all -- and resets one past them as nos does, `busy` in api.status counting
// them: a panel that opens connections as it pleases fails here as it fails
// against a real host, with ECONNRESET.
//
//     node mock/server.mjs [--port 39234] [--state mock/state] [--name nos]
//                          [--trust <admin.crt>] [--fresh]
//
// It makes, in its state folder, the host's key and certificate and an
// admin's and a reader's, and prints the host's key and where the clients'
// are -- what the panel is pointed at (panel/README.md). Its id is its key's,
// so each state folder is a host of its own; --name names it, and --trust
// lets another's key in as an admin -- the panel's, made for the first
// host, as `api client add` lets it in on a second (a cluster). --fresh
// makes it a host of no cluster yet, its registry's or NBS's, for the
// panel to make one of. No dependency but Node's own and openssl for the
// keys.

import crypto from 'node:crypto';
import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import https from 'node:https';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const ROOT = path.resolve(HERE, '..', '..');

const args = process.argv.slice(2);
const opt = (name, fallback) => {
  const i = args.indexOf('--' + name);
  return i >= 0 && i + 1 < args.length ? args[i + 1] : fallback;
};
const PORT = Number(opt('port', '39234'));
const STATE = path.resolve(opt('state', path.join(HERE, 'state')));
const NAME = opt('name', 'nos');
const TRUST = opt('trust', null);
const FRESH = args.includes('--fresh');

/* ---- keys ---- */

/** The SHA-256 of a certificate's SubjectPublicKeyInfo, as nos pins one. */
export function fingerprint(certPem) {
  const spki = new crypto.X509Certificate(certPem).publicKey.export({ type: 'spki', format: 'der' });
  return crypto.createHash('sha256').update(spki).digest('hex');
}

/** A P-256 key and a self-signed certificate for `name`, made once. */
function identity(name) {
  const key = path.join(STATE, name + '.key');
  const crt = path.join(STATE, name + '.crt');
  if (!fs.existsSync(key) || !fs.existsSync(crt)) {
    execFileSync('openssl', ['req', '-x509', '-newkey', 'ec', '-pkeyopt', 'ec_paramgen_curve:prime256v1', '-nodes',
      '-keyout', key, '-out', crt, '-days', '825', '-subj', '/CN=' + name], { stdio: 'ignore' });
  }
  const cert = fs.readFileSync(crt, 'utf8');
  return { key, crt, cert, pin: fingerprint(cert) };
}

/* ---- the providers' descriptors, the tree's own ---- */

/** Providers only the kernel's own tests register: not a host's. */
const TESTS_ONLY = ['selftest', 'modtest'];

function descriptors() {
  const found = {};
  const crates = path.join(ROOT, 'src', 'crates');
  for (const rel of fs.readdirSync(crates, { recursive: true })) {
    if (!String(rel).endsWith('.descriptor.json') || String(rel).includes('vendor')) {
      continue;
    }
    const text = fs.readFileSync(path.join(crates, rel), 'utf8');
    const d = JSON.parse(text);
    if (!TESTS_ONLY.includes(d.provider)) {
      found[d.provider] = { text, d };
    }
  }
  return found;
}

/* ---- the host, as its numbers move ---- */

const START = Date.now();
const NS_PER_MS = 1e6;
const CPUS = 4;
const MIB = 1024 * 1024;
const uptimeMs = () => Date.now() - START;

/** What a CPU has idled since boot: most of the time, less on cpu 1. */
const idleNs = (cpu) => Math.floor(uptimeMs() * NS_PER_MS * (cpu === 1 ? 0.35 + 0.25 * Math.sin(uptimeMs() / 9000) : 0.9));

const log = [];
let logSeq = 0;
function logLine(text, level = 0) {
  logSeq += 1;
  log.push({ seq: logSeq, ms: uptimeMs(), level, text });
  if (log.length > 400) {
    log.shift();
  }
}
for (const t of ['boot: complete', 'net: eth0 up, 10.0.2.15/24 by DHCP', 'module: hv loaded at 0xffff800001a00000',
  'hv: loaded -- AMD-V, ready', 'module: api loaded', 'api: serving port 39234 on hv0 -- host key ' + 'ab'.repeat(32)]) {
  logLine(t);
}
setInterval(() => logLine('heartbeat: ' + Math.floor(uptimeMs() / 1000) + ' s up, all well'), 5000).unref();

const ifaces = { eth0: { rx: 0, tx: 0 }, hv0: { rx: 0, tx: 0 } };
setInterval(() => {
  ifaces.eth0.rx += 120000 + Math.floor(Math.random() * 40000);
  ifaces.eth0.tx += 40000 + Math.floor(Math.random() * 20000);
  ifaces.hv0.rx += 10000;
  ifaces.hv0.tx += 15000;
}, 1000).unref();

/* ---- the VMs ---- */

const vms = new Map();
let nextVmId = 0;
function vm(name, fields) {
  vms.set(name, {
    name, id: null, state: 'off', protected: false, cpus: 1, memory_mib: 1024, node: null, net: true, port: null,
    network: null, firewall: 'default', public: null, kernel: '/lib/hvboot', console: '', bootMs: 0, restarts: 0, reason: null,
    forwards: [], disks: [{ file: 'disk.raw', size_bytes: 20 * 1024 * MIB, read_only: false },
      { file: 'seed.iso', size_bytes: 65536, read_only: true }],
    ...fields,
  });
}
/* The host's PCI functions as VMs are given them (the vm provider's `pci`):
 * a GPU and its audio function kept for guests (guestpci=), behind a root
 * port without ACS -- one IOMMU group with it -- the host's NIC, a driver's,
 * and the port, a bridge. */
const PCI = [
  { address: '00:01.0', id: '8086:a70d', class: '06.04', state: 'free', held: false, group: '01:00.0 01:00.1',
    reset: 'none', interrupts: 'MSI', refused: 'a bridge' },
  { address: '00:1f.6', id: '8086:1a1c', class: '02.00', state: 'driver', held: false, group: '', reset: 'FLR',
    interrupts: 'MSI', refused: "a driver of the host's has it -- guestpci=00:1f.6 keeps it" },
  { address: '01:00.0', id: '10de:2571', class: '03.00', state: 'kept', held: false,
    group: '00:01.0(bridge) 01:00.1', reset: 'bus reset', interrupts: 'MSI', refused: null },
  { address: '01:00.1', id: '10de:228e', class: '04.03', state: 'kept', held: false,
    group: '00:01.0(bridge) 01:00.0', reset: 'bus reset', interrupts: 'MSI', refused: null },
];
/* The host's devices a VM's disk may be (the vm provider's `devices`): its
 * root's disk, of which nothing is given -- the EFI System partition and the
 * root it boots -- and a second disk, one partition empty and one holding an
 * ext4. */
const DEVICES = [
  { name: 'nvme0', id: 'disk:nvme-SAMSUNG_MZVL2512HCJQ-00B00_S1', disk: null, size_bytes: 512 * 1024 * MIB, type: null,
    boots_the_machine: null, held_by: null, in_use_by: 'a mounted filesystem', holds: null,
    refused: 'nvme0 is in use by a mounted filesystem' },
  { name: 'nvme01', id: 'part:2f1c3a4b-0001-4e69-94e6-46e271b44854', disk: 'nvme0', size_bytes: 256 * MIB,
    type: 'c12a7328-f81f-11d2-ba4b-00a0c93ec93b', boots_the_machine: 'an EFI System partition', held_by: null,
    in_use_by: null, holds: 'a FAT filesystem', refused: 'nvme01 is an EFI System partition: the machine boots from it, and no vm is given it' },
  { name: 'nvme02', id: 'part:2f1c3a4b-0002-4e69-94e6-46e271b44854', disk: 'nvme0', size_bytes: 16 * 1024 * MIB,
    type: '0fc63daf-8483-4772-8e79-3d69d8477de4', boots_the_machine: null, held_by: 'a mounted filesystem',
    in_use_by: 'a mounted filesystem', holds: null, refused: 'nvme02 is in use by a mounted filesystem' },
  { name: 'nvme16', id: 'part:704db7dd-0006-4e69-94e6-46e271b44854', disk: 'nvme1', size_bytes: 15 * 1024 * MIB,
    type: '0fc63daf-8483-4772-8e79-3d69d8477de4', boots_the_machine: null, held_by: null, in_use_by: null, holds: null, refused: null },
  { name: 'nvme17', id: 'part:704db7dd-0007-4e69-94e6-46e271b44854', disk: 'nvme1', size_bytes: 8 * 1024 * MIB,
    type: '0fc63daf-8483-4772-8e79-3d69d8477de4', boots_the_machine: null, held_by: null,
    in_use_by: null, holds: 'an ext4 filesystem labelled "data"',
    refused: 'nvme17 holds an ext4 filesystem labelled "data" -- the vm would have its data over it; give it anyway with force' },
  { name: 'sda', id: 'disk:ata-ST4000NM0035_ZC1A2B3C', disk: null, size_bytes: 64 * 1024 * MIB, type: null,
    boots_the_machine: null, held_by: null, in_use_by: null, holds: null, refused: null },
];

/* Where a VM's disk is, as `disks` says it: a device's id and a volume of
 * NBS's (nbs:<volume>) as they are, a file in its folder. */
const diskPath = (v, d) => (d.device || String(d.file).startsWith('nbs:') ? d.file : '/vms/' + v.name + '/' + d.file);

/* What a device's disk is, as `disks` lists it: its id the word. */
function deviceDisk(d, readOnly) {
  return { file: d.id, size_bytes: d.size_bytes, read_only: readOnly, device: true, device_name: d.name };
}

/* The device a word names -- its id, or dev: and its name -- given as a
 * VM's disk where it may be: refused as the host would, force over what is
 * on it. */
function deviceGiven(word, force) {
  const d = DEVICES.find((x) => x.id === word || 'dev:' + x.name === word);
  if (!d) {
    throw new Refused('not_found', 'no device is ' + word, 'device');
  }
  if (d.boots_the_machine || d.in_use_by || (d.refused && !force)) {
    throw new Refused('conflict', d.refused, 'device');
  }
  return d;
}

vm('panel', { protected: true, port: 0, firewall: 'panel', forwards: [{ port: 39233, guest_port: 443 }] });
vm('web', { cpus: 2, memory_mib: 2048, port: 3, firewall: 'web', forwards: [{ port: 2201, guest_port: 22 }] });
vm('db', { memory_mib: 4096, network: 'back' });
if (!FRESH) {
  /* On volumes of NBS's: one with a replica down, one with a chunk unreachable */
  const seed = { file: 'seed.iso', size_bytes: 65536, read_only: true };
  vm('store', { disks: [{ file: 'nbs:deg-vol', size_bytes: null, read_only: false }, seed] });
  vm('broken', { disks: [{ file: 'nbs:lost-vol', size_bytes: null, read_only: false }, seed] });
}

/* ---- the host's firewalls (the fw provider) ---- */

/** Each firewall of the host's by name: its policies and rules, as a person
 * writes them, and the flows each rule and policy judged. `default` judges a
 * VM that names none; the panel's VM and web have their own, letting in at
 * their forwarded ports, as hv's first start under firewalls makes them. */
const firewalls = new Map();
function firewallMade(name, inbound = 'deny', outbound = 'allow', rules = []) {
  firewalls.set(name, { in: inbound, out: outbound, rules, hits: rules.map(() => 3), policyIn: 12, policyOut: 40 });
}
firewallMade('default');
firewallMade('panel', 'deny', 'allow', ['allow in tcp 443']);
firewallMade('web', 'deny', 'allow', ['allow in tcp 22', 'allow in tcp 80,443 from any']);

const FIREWALL_NAME = /^[A-Za-z0-9][A-Za-z0-9._-]{0,31}$/;
/** A rule as hv's `Rule::parse` takes one, roughly: its action and direction
 * first, then what it is about. */
const RULE = /^(allow|deny) (in|out)( (tcp|udp|icmp))?( [0-9][0-9,-]*)?( (from|to) (any|net|[0-9./,]+))?$/;

const firewallNamed = (p, key = 'name') => {
  const name = need(p, key);
  const f = firewalls.get(name);
  if (!f) {
    throw new Refused('not_found', 'no firewall ' + name + ' on this host -- hv fw create ' + name, key);
  }
  return [name, f];
};
function firewallRow(name, f) {
  return { name, in: f.in, out: f.out, rules: f.rules.length,
    vms: [...vms.values()].filter((v) => v.state === 'running' && v.firewall === name).map((v) => v.name),
    policy_in_flows: f.policyIn, policy_out_flows: f.policyOut, made: true };
}
/** A rule's number from the call, 1 to `count`. */
function ruleNumber(p, key, count) {
  const n = Number(need(p, key));
  if (!Number.isInteger(n) || n < 1 || n > count) {
    throw new Refused('invalid', count ? n + ' is not a rule\'s number -- 1 to ' + count : 'the firewall has no rule', key);
  }
  return n;
}
function policyOf(p, key) {
  if (p[key] === undefined) return undefined;
  if (p[key] !== 'allow' && p[key] !== 'deny') {
    throw new Refused('invalid', 'is allow or deny', key);
  }
  return p[key];
}

/** A VM started: on the list, its console printing a boot. */
function startVm(v) {
  v.id = nextVmId++;
  v.state = 'running';
  v.bootMs = uptimeMs();
  v.reason = null;
  v.console += '\r\n\x1b[1mhvboot\x1b[0m: booting /boot/vmlinuz-6.12 from /dev/vda1\r\n';
}
for (const name of ['panel', 'web', 'store', 'broken']) {
  if (vms.has(name)) {
    startVm(vms.get(name));
  }
}
setInterval(() => {
  for (const v of vms.values()) {
    if (v.state === 'running') {
      const s = Math.floor((uptimeMs() - v.bootMs) / 1000);
      v.console += s < 6 ? `[    ${s}.000000] systemd[1]: starting step ${s}\r\n` : '';
      if (s === 6) {
        v.console += `\r\nDebian GNU/Linux 13 ${v.name} ttyS0\r\n\r\n${v.name} login: `;
      }
      if (v.console.length > 65536) {
        v.console = v.console.slice(-65536);
      }
    }
  }
}, 1000).unref();

/** What a VM's console has printed in all: its text's length is the ring's. */
const consoleTotal = new Map();
function consoleOf(v) {
  return { text: v.console, total: (consoleTotal.get(v.name) || 0) + v.console.length };
}

function vmJson(v) {
  const port = v.port;
  return {
    name: v.name, id: v.id, state: v.state, folder: true, protected: v.protected, cpus: v.cpus,
    memory_mib: v.memory_mib, node: v.node, node_memory_pct: v.id !== null && v.node !== null ? 100 : null,
    net: v.net, port, ip: port === null ? null : '10.0.100.' + (port + 2),
    network: v.network, firewall: v.firewall, public: v.public, kernel: v.kernel,
    ...(v.id !== null ? {
      uptime_ms: v.state === 'running' ? uptimeMs() - v.bootMs : 0, restarts: v.restarts,
      exits: Math.floor((uptimeMs() - v.bootMs) * 37), console_bytes: consoleOf(v).total,
      ...(v.reason ? { reason: v.reason } : {}),
    } : {}),
  };
}

/* ---- jobs ---- */

const jobs = [];
let nextJob = 1;
/** A job of `steps` steps a second apart, each saying `say(i)`; `finish`
 * makes its result -- or throws, its status and message. */
function job(provider, method, steps, say, finish) {
  const j = {
    id: nextJob++, provider, method, state: 'running', status: null, done: 0, total: steps,
    queued_ms: uptimeMs(), started_ms: uptimeMs(), ended_ms: 0, notes: [], result: null, cancel: false,
  };
  jobs.push(j);
  if (jobs.length > 32) {
    jobs.shift();
  }
  const tick = () => {
    if (j.cancel) {
      return end('cancelled', { error: { code: 'cancelled', message: 'stopped, as asked -- what it had made taken away' } });
    }
    if (j.done < steps) {
      j.notes.push({ seq: j.notes.length, text: say(j.done) });
      j.done += 1;
      setTimeout(tick, 1000).unref();
      return;
    }
    try {
      end('ok', finish());
    } catch (e) {
      end(e.code || 'failed', { error: { code: e.code || 'failed', message: e.message } });
    }
  };
  const end = (status, result) => {
    j.state = 'done';
    j.status = status;
    j.result = result;
    j.ended_ms = uptimeMs();
  };
  setTimeout(tick, 200).unref();
  return j;
}

/* ---- vnet: the cluster's networks for VMs, as vnet.ko's provider answers ---- */

const vnet = {
  member: true, name: 'lab',
  hosts: [{ host: 'mock-a', member: 4011, vote: 'voter', leader: true, lost: false }],
  networks: [{ name: 'lan', vni: 1, net: [10, 10, 0, 0], prefix: 24, mtu: 1420, out: true }],
  ports: [{ id: 1, network: 'lan', address: '10.10.0.2', host: 'mock-a', generation: 1, firewall: 'default',
    vm: '11111111-2222-3333-4444-555555555555', nic: 0 }],
  public: [],
  firewalls: [{ name: 'default', in: 'deny', out: 'allow', rules: [], revision: 1 }],
  nextVni: 2,
};
const VNET_NAME = /^[a-z0-9-]{1,26}$/;
const quad = (a) => a.join('.');
function vnetSubnet(text) {
  const m = /^(\d+)\.(\d+)\.(\d+)\.(\d+)\/(\d+)$/.exec(text || '');
  if (!m) throw new Refused('invalid', 'is a subnet: a.b.c.d/prefix', 'subnet');
  return { net: m.slice(1, 5).map(Number), prefix: Number(m[5]) };
}
const vnetSpan = (n) => {
  const start = ((n.net[0] << 24) | (n.net[1] << 16) | (n.net[2] << 8) | n.net[3]) >>> 0;
  return [start, start + 2 ** (32 - n.prefix) - 1];
};
const vnetFirewall = (p, key = 'name') => {
  const name = need(p, key);
  const f = vnet.firewalls.find((x) => x.name === name);
  if (!f) throw new Refused('not_found', 'no firewall ' + name);
  return f;
};
function vnetSetRules(f, rules, inbound = f.in, outbound = f.out) {
  f.rules = rules;
  f.in = inbound;
  f.out = outbound;
  f.revision += 1;
}

function jobInfo(j) {
  return {
    id: j.id, provider: j.provider, method: j.method, state: j.state, status: j.status, done: j.done, total: j.total,
    queued_ms: j.queued_ms, started_ms: j.started_ms, ended_ms: j.ended_ms, notes_next: j.notes.length,
  };
}

/* ---- the providers' methods ---- */

class Refused extends Error {
  constructor(code, message, field) {
    super(message);
    this.code = code;
    this.field = field;
  }
}

const need = (p, key) => {
  if (p[key] === undefined || p[key] === null) {
    throw new Refused('invalid', 'is needed', key);
  }
  return p[key];
};
const vmNamed = (p) => {
  const v = vms.get(need(p, 'name'));
  if (!v) {
    throw new Refused('not_found', 'no vm ' + p.name + ': there is no /vms/' + p.name + '/vm.toml', 'name');
  }
  return v;
};

/* ---- the cluster's registry, as a member of it has it ---- */

/** This host as ipcnet has it: the registry's first host, where the mock is
 * a member of its own; its own name, where it is fresh. */
const me = {
  name: FRESH ? NAME : 'mock-a', ip: FRESH ? '10.9.0.5' : '10.9.0.1',
  key: crypto.createHash('sha256').update(FRESH ? NAME : 'mock-a').digest('hex'),
};
const HOST_NAME = /^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$/;
const CLUSTER_NO_MEMBER = 'this host is no member -- `cluster init`, or a join';
const cluster = {
  member: !FRESH,
  name: FRESH ? null : 'test',
  hosts: FRESH ? [] : [
    { name: 'mock-a', node: 1, vote: 'voter', leader: true, ip: '10.9.0.1', underlay: null, public: null, maintenance: false,
      key: crypto.createHash('sha256').update('mock-a').digest('hex'), moving_to: null },
    { name: 'mock-b', node: 2, vote: 'voter', leader: false, ip: '10.9.0.2', underlay: null, public: '203.0.113.7',
      maintenance: false, key: crypto.createHash('sha256').update('mock-b').digest('hex'), moving_to: null },
    { name: 'mock-d', node: 4, vote: 'voter', leader: false, ip: '10.9.0.4', underlay: null, public: null,
      maintenance: false, key: crypto.createHash('sha256').update('mock-d').digest('hex'), moving_to: null },
  ],
  invitations: [],
  removed: 0,
};
const clusterHost = (name) => cluster.hosts.find((h) => h.name === name);
const clusterMember = () => {
  if (!cluster.member) {
    throw new Refused('failed', CLUSTER_NO_MEMBER);
  }
};
const clusterFresh = (field) => {
  if (cluster.member) {
    throw new Refused('conflict', 'this host is a member of a cluster already', field);
  }
};
/** This host as the registry's status says it: what an invitation asks. */
const thisHost = () => ({ name: me.name, key: (clusterHost(me.name) || me).key, ip: me.ip, port: null, serving: true });

/* ---- NBS's metadata, as this host's member has it: the registry's three
   hosts its members, a chunk server of one device on each, and the volumes
   -- every chunk on all three, as there is nowhere else ---- */

const SLOT = 4 * MIB;
const NBS_SLOTS = 1022;
const NBS_NAME = /^[a-z0-9][a-z0-9._-]{0,62}$/;
const NBS_NO_MEMBER = 'this host is in no NBS cluster -- `nbs init <name>`, or `nbs host add <this host>` on a member';
const nbs = {
  member: !FRESH,
  name: FRESH ? null : 'store',
  hosts: cluster.hosts.map((h, i) => ({ host: h.name, member: 4096 + i, vote: 'voter', leader: i === 0 })),
  /* A volume in each state of health NBS says beside the whole one: its
   * chunks' counts by their replicas, as a fixture sets them */
  volumes: FRESH ? [] : [
    { name: 'web-root', volume: 1, size_mib: 2048, host: 'mock-a', writer: null, generation: 0, readers: [] },
    { name: 'deg-vol', volume: 2, size_mib: 64, host: 'mock-a', writer: 'mock-a', generation: 1, readers: [],
      counts: { chunks_at_two: 3 } },
    { name: 'copy-vol', volume: 3, size_mib: 64, host: 'mock-a', writer: null, generation: 0, readers: [],
      counts: { chunks_copying: 4 } },
    { name: 'crit-vol', volume: 4, size_mib: 64, host: 'mock-a', writer: null, generation: 0, readers: [],
      counts: { chunks_at_one: 2 } },
    { name: 'lost-vol', volume: 5, size_mib: 64, host: 'mock-a', writer: 'mock-a', generation: 1, readers: [],
      counts: { chunks_unreachable: 1 } },
  ],
  next: 6,
  /* Each host's chunk server's devices: one each, every slot of it the
   * cluster's */
  devices: cluster.hosts.map((h, i) => ({ host: h.name, device: i + 1, id: 'part:'
    + crypto.createHash('sha256').update(h.name).digest('hex').slice(0, 8) + '-0000-4000-8000-000000000001',
  size: NBS_SLOTS * SLOT, state: 'active' })),
};
nbs.nextMember = 4096 + nbs.hosts.length;
nbs.nextDevice = nbs.devices.length + 1;
const nbsMember = () => {
  if (!nbs.member) {
    throw new Refused('conflict', NBS_NO_MEMBER);
  }
};
const nbsHostNamed = (p) => {
  const host = need(p, 'host');
  if (!HOST_NAME.test(host)) {
    throw new Refused('invalid', "a host's name: lowercase letters, digits and -", 'host');
  }
  return host;
};
const nbsChunks = (size) => Math.ceil(size / 4);
const nbsTaken = () => nbs.volumes.reduce((n, v) => n + nbsChunks(v.size_mib), 0);
const nbsVolume = (p) => {
  const v = nbs.volumes.find((x) => x.name === need(p, 'name'));
  if (!v) {
    throw new Refused('not_found', 'no volume of that name', 'name');
  }
  return v;
};
const nbsRoom = (more) => {
  if (nbsTaken() + more > NBS_SLOTS) {
    throw new Refused('failed', 'not room for its chunks on three hosts');
  }
};
/** A volume's health as NBS says it (nbs-mds's `health_word`): the worst of
 * its chunks' counts by their replicas. */
const nbsHealth = (v) => {
  const c = { chunks_at_two: 0, chunks_at_one: 0, chunks_unreachable: 0, chunks_copying: 0, chunks_unlike: 0,
    ...(v.counts || {}) };
  const health = c.chunks_unreachable || c.chunks_unlike ? 'failed' : c.chunks_at_one ? 'critical'
    : c.chunks_at_two || c.chunks_copying ? 'degraded' : 'ok';
  return { health, ...c };
};
const nbsJson = (v, replicas) => ({
  name: v.name, volume: v.volume, kind: v.kind || 'volume', ...nbsHealth(v),
  origin: (nbs.volumes.find((o) => v.origin && o.volume === v.origin) || { name: null }).name,
  size: v.size_mib * MIB, chunks: nbsChunks(v.size_mib), host: v.host, writer: v.writer,
  generation: v.generation, readers: v.readers,
  ...(replicas ? { replicas: nbs.hosts.map((h) => ({ host: h.host, chunks: nbsChunks(v.size_mib) })) } : {}),
});
/** A volume made of `from` -- a snapshot or a clone, thin: nothing reserved
 * (N10d). Made again of the same origin, answered as it is. */
const nbsShare = (from, name, kind, host) => {
  if (!NBS_NAME.test(name)) {
    throw new Refused('invalid', 'lowercase letters, digits, . _ and -, up to 63', 'name');
  }
  const had = nbs.volumes.find((v) => v.name === name);
  if (had) {
    if ((had.kind || 'volume') !== kind || had.origin !== from.volume) {
      throw new Refused('conflict', 'a volume of that name is there already, of another size or kind');
    }
    return { volume: had.volume };
  }
  const v = { name, volume: nbs.next, kind, origin: from.volume, size_mib: from.size_mib, host, writer: null,
              generation: 0, readers: [] };
  nbs.next += 1;
  nbs.volumes.push(v);
  return { volume: v.volume };
};

const reads = {
  'system.info': () => ({
    version: '0.0.28-mock', revision: 'rev-' + me.name, arch: 'x86_64', cpu_model: 'AMD Ryzen 5 3600 6-Core Processor',
    cpus: CPUS, uptime_ms: uptimeMs(), time_s: Math.floor(Date.now() / 1000), log_level: 0,
    cmdline: 'root=UUID=0000 console=ttyS0',
  }),
  'system.cpus': () => ({ cpus: [...Array(CPUS).keys()].map((cpu) => ({ cpu, idle_ns: idleNs(cpu) })), uptime_ms: uptimeMs() }),
  'system.memory': () => ({ total_bytes: 8192 * MIB, free_bytes: Math.floor((4096 + 512 * Math.sin(uptimeMs() / 20000)) * MIB) }),
  /* Two nodes of two CPUs and 4 GiB each, as QEMU's -numa makes them */
  'system.numa': () => ({ nodes: [0, 1].map((node) => ({
    node, domain: node, cpus: node === 0 ? '0-1' : '2-3', cpu_count: CPUS / 2, memory_bytes: 4096 * MIB,
    free_bytes: (node === 0 ? 1900 : 2100) * MIB, zeroed_bytes: 1024 * MIB, huge_total_bytes: 512 * MIB,
    huge_free_bytes: (node === 0 ? 256 : 512) * MIB, distances: node === 0 ? '10 21' : '21 10',
  })) }),
  'system.tasks': () => ({ tasks: [
    { pid: 1, name: 'shell', state: 'waiting', idle: false, blocked: true, stopping: false, runtime_ns: 1e9, switches: 120 },
    { pid: 7, name: 'hv/vm0', state: 'running', idle: false, blocked: false, stopping: false, runtime_ns: uptimeMs() * 4e5, switches: 9000 },
    { pid: 9, name: 'api/conn', state: 'waiting', idle: false, blocked: true, stopping: false, runtime_ns: 2e8, switches: 300 },
  ] }),
  'system.log': (p) => {
    const since = p.since ?? 0;
    const lines = log.filter((l) => l.seq > since);
    return { lines, last_seq: lines.length ? lines[lines.length - 1].seq : since, next_seq: logSeq + 1 };
  },
  'system.modules': () => ({ modules: ['hv', 'api', 'sshd'].map((name) => ({ name, size_bytes: 900000, imports: 120, state: 'live', permanent: false })) }),
  'system.interrupts': () => ({ sources: [{ name: 'timer', count: uptimeMs() * 4 }, { name: 'virtio-net', count: uptimeMs() }] }),
  'net.interfaces': () => ({ interfaces: Object.entries(ifaces).map(([name, c]) => ({
    name, ip: name === 'eth0' ? '10.0.2.15' : '10.0.100.1', mac: name === 'eth0' ? '52:54:00:12:34:56' : '02:6e:6f:73:00:01',
    up: true, rx_frames: Math.floor(c.rx / 900), tx_frames: Math.floor(c.tx / 900), rx_bytes: c.rx, tx_bytes: c.tx,
  })) }),
  'net.tcp': () => ({ slots: 64, used: 9, connections: [] }),
  'net.arp': () => ({ entries: [{ ip: '10.0.2.2', mac: '52:55:0a:00:02:02' }] }),
  'net.pool': () => ({ frames: 4096, free: 3900 }),
  'block.disks': () => ({ disks: [{ name: 'nvme0n1', size_bytes: 512 * 1024 * MIB, holder: null }] }),
  'fs.mounts': () => ({ mounts: [{ path: '/', fs: 'ext2', device: 'nvme0n1p5', read_only: false,
    total_bytes: 400 * 1024 * MIB, free_bytes: 310 * 1024 * MIB }] }),
  'api.status': () => ({ serving: true, port: 39234, connections: [], key: hostPin, accepted: limits.accepted,
    busy: limits.busy, consoles: Object.fromEntries(consoleStreams) }),
  'api.clients': () => ({ clients: [{ name: 'panel', role: 'admin', key: adminPin }, { name: 'reader', role: 'read', key: readerPin }] }),
  'vm.list': () => ({ vms: [...vms.values()].map(vmJson) }),
  'fw.list': () => ({ firewalls: [...firewalls].map(([name, f]) => firewallRow(name, f)) }),
  'fw.rules': () => ({ rules: [...firewalls].flatMap(([name, f]) => f.rules.map((rule, i) => ({
    id: name + '/' + (i + 1), firewall: name, n: i + 1, rule, flows: f.hits[i] }))) }),
  'fw.get': (p) => {
    const [name, f] = firewallNamed(p);
    return { firewall: firewallRow(name, f), rules: f.rules.map((rule, i) => ({ n: i + 1, rule, flows: f.hits[i] })) };
  },
  'vm.get': (p) => {
    const v = vmNamed(p);
    return {
      vm: vmJson(v),
      config: { kernel: v.kernel, initrd: null, cmdline: null, forward: v.forwards, restart: true, console_log: true,
        uuid: '7c1e5f3a-0000-4000-8000-' + String(v.name).padEnd(12, '0').slice(0, 12).replace(/[^0-9a-f]/g, '0') },
      disks: v.disks.map((d) => ({ device: false, device_name: null, ...d, path: diskPath(v, d), vm: v.name })),
      forwards: v.state === 'running' ? v.forwards.map((f) => ({ ...f, vm: v.name, id: v.id,
        guest_ip: '10.0.100.' + ((v.port ?? 0) + 2), active: 0, accepted: 3, refused: 0, failed: 0, stalled: 0 })) : [],
    };
  },
  'vm.host': () => ({ extension: 'AMD-V', on_cpu_mask: 15, vms_root: '/vms', stop_secs: 120, cores: 'own',
    vms_listed: [...vms.values()].filter((v) => v.id !== null).length, vms_max: 16, huge_pool_bytes: 0,
    huge_pool_free_bytes: 0, switch: { ip: '10.0.100.1', nat: '10.0.2.15', frames_to_host: 1000, frames_refused: 0,
      dhcp_answers: 4, frames_foreign: 0 } }),
  'vm.networks': () => {
    const on = [...vms.values()].filter((v) => v.state === 'running');
    /* The host's in use, and every one of the cluster's, as vnet:<name> */
    const cluster = vnet.networks.map((n) => 'vnet:' + n.name);
    const names = [...new Set([...on.map((v) => v.network).filter(Boolean), ...cluster])].sort();
    return { networks: names.map((n) => {
      const c = vnet.networks.find((x) => 'vnet:' + x.name === n);
      return { name: n, cluster: c !== undefined, ...(c ? { subnet: quad(c.net) + '/' + c.prefix, free: 250 } : {}),
               vms: on.filter((v) => v.network === n).map((v) => v.name) };
    }), alone: on.filter((v) => !v.network).map((v) => v.name) };
  },
  'vm.local_images': () => ({ images: LOCAL_IMAGES }),
  'vm.config_get': (p) => {
    const v = vmNamed(p);
    const disks = v.disks.map((d) => `"${d.file}${d.read_only ? ':ro' : ''}"`).join(', ');
    return { text: `# A VM of nos's hypervisor\nkernel = "${v.kernel}"\n`
      + (v.kernel === '/lib/CLOUDHV.fd' ? 'firmware = true\nhyperv = true\n' : '')
      + `memory = ${v.memory_mib}\ncpus = ${v.cpus}\ndisks = [${disks}]\nnet = true\n`
      + (v.protected ? 'protected = true\n' : ''), prev: false };
  },
  'vm.console_read': (p) => {
    const v = vmNamed(p);
    if (v.id === null) {
      throw new Refused('not_found', 'no vm ' + v.name, 'name');
    }
    const { text, total } = consoleOf(v);
    const oldest = total - text.length;
    const from = Math.min(Math.max(p.from ?? 0, oldest), total);
    const max = p.max_bytes ?? 16384;
    const page = text.slice(from - oldest, from - oldest + max);
    return { from, next: from + page.length, total, text: p.terminal ? page : page.replace(/\x1b\[[0-9;]*m/g, '') };
  },
  'vm.devices': () => ({ devices: DEVICES }),
  'vm.pci': () => ({ functions: PCI.map((f) => ({ ...f,
    named_by: [...vms.values()].filter((v) => (v.pci ?? []).includes(f.address)).map((v) => v.name).join(' ') })) }),
  /* Each disk's counters since its guest's boot, moving as a guest's would:
   * its first disk busy, a wave on it, its others near idle. */
  'vm.disk_io': (p) => {
    const v = vmNamed(p);
    const now = uptimeMs();
    if (v.state !== 'running') {
      return { vm: v.name, running: false, restarts: v.restarts, at_ms: now, disks: [] };
    }
    const zero = () => ({ reads: 0, writes: 0, read_bytes: 0, written_bytes: 0, flushes: 0, errors: 0, discards: 0,
      zeroings: 0, wait_ns: 0 });
    if (!v.io || v.ioBoot !== v.bootMs) {
      v.io = [];
      v.ioBoot = v.bootMs;
      v.ioAt = v.bootMs;
    }
    while (v.io.length < v.disks.length) {
      v.io.push(zero());
    }
    const secs = (now - v.ioAt) / 1000;
    v.ioAt = now;
    v.disks.forEach((d, i) => {
      const c = v.io[i];
      const vol = String(d.file).startsWith('nbs:') ? nbs.volumes.find((x) => 'nbs:' + x.name === d.file) : null;
      if (vol?.counts?.chunks_unreachable) {
        c.errors += Math.max(1, Math.round(2 * secs));
      }
      const wave = 1 + 0.6 * Math.sin(now / 9000 + i);
      const reads = Math.round((i === 0 ? 220 : 1) * wave * secs);
      const writes = d.read_only ? 0 : Math.round((i === 0 ? 70 : 0) * wave * secs);
      c.reads += reads;
      c.read_bytes += reads * 16384;
      c.writes += writes;
      c.written_bytes += writes * 65536;
      c.flushes += Math.round(writes / 8);
      /* A volume with a replica down answers slowly: 80 ms a request */
      const ms = vol?.counts?.chunks_at_two ? 80 : 1;
      c.wait_ns = (c.wait_ns || 0) + Math.round((reads + writes) * ms * 1e6);
    });
    return { vm: v.name, running: true, restarts: v.restarts, at_ms: now,
      disks: v.disks.map((d, i) => ({ file: d.file, ...v.io[i] })) };
  },
  'vm.disks': (p) => {
    const v = vmNamed(p);
    return { disks: v.disks.map((d) => ({ device: false, device_name: null, ...d, path: diskPath(v, d), vm: v.name })) };
  },
  'vm.forwards': () => ({ forwards: [...vms.values()].filter((v) => v.state === 'running').flatMap((v) => v.forwards.map((f) => ({
    ...f, vm: v.name, id: v.id, guest_ip: '10.0.100.' + ((v.port ?? 0) + 2), active: 0, accepted: 3, refused: 0, failed: 0, stalled: 0 }))) }),
  'cluster.status': () => (cluster.member ? {
    member: true, this_host: thisHost(), cluster: cluster.name, node: (clusterHost(me.name) || { node: 1 }).node,
    role: 'leader', term: 3, leader: cluster.hosts[0].name, applied: 40 + cluster.invitations.length,
    hosts: cluster.hosts.length, voters: cluster.hosts.length, learners: 0, refused: 0, stopped: null,
  } : { member: false, this_host: thisHost() }),
  'cluster.hosts': () => {
    clusterMember();
    return { hosts: cluster.hosts, invitations: cluster.invitations, removed: cluster.removed };
  },
  'nbs.status': () => (!nbs.member ? { member: false } : {
    member: true, cluster: nbs.name, node: nbs.hosts[0].member, role: 'leader', term: 2, leader: nbs.hosts[0].host,
    applied: 70 + nbs.next, members: nbs.hosts.length, volumes: nbs.volumes.length, chunks: nbsTaken(), digest_at: 64,
    digest: crypto.createHash('sha256').update('nbs').digest('hex'), stopped: null,
  }),
  'nbs.hosts': () => {
    nbsMember();
    return { hosts: nbs.hosts };
  },
  'nbs.cs': () => {
    nbsMember();
    return { cs: nbs.hosts.map((h) => {
      const devices = nbs.devices.filter((d) => d.host === h.host);
      const size = devices.filter((d) => d.state === 'active').reduce((n, d) => n + d.size, 0);
      const reserved = Math.min(size, nbsTaken() * SLOT);
      return { host: h.host, member: true, size, reserved, free: size - reserved,
        devices: devices.map(({ device, id, size: bytes, state }) => ({ device, id, size: bytes, state })) };
    }) };
  },
  'nbs.devices': () => {
    nbsMember();
    return { devices: nbs.devices };
  },
  /* What to look at, the worst first, as nbs-mds's `alerts` says it: the
   * volumes' chunks short of three, devices, room, hosts in maintenance */
  'nbs.alerts': () => {
    const order = ['stopped', 'no_leader', 'chunks_unreachable', 'unlike', 'device_failed', 'no_room', 'chunks_at_one',
      'cs_down', 'cs_out', 'chunks_at_two', 'copying', 'cs_full', 'maintenance', 'device_draining', 'no_member', 'ok'];
    const out = [];
    const add = (kind, host = null, detail = null, count = null) => out.push({ kind, host, detail, count });
    if (!nbs.member) {
      add('no_member');
    } else {
      const sum = (k) => nbs.volumes.reduce((n, v) => n + ((v.counts || {})[k] || 0), 0);
      for (const [kind, k] of [['chunks_unreachable', 'chunks_unreachable'], ['chunks_at_one', 'chunks_at_one'],
        ['chunks_at_two', 'chunks_at_two'], ['copying', 'chunks_copying']]) {
        if (sum(k)) add(kind, null, null, sum(k));
      }
      for (const d of nbs.devices) {
        if (d.state === 'draining') add('device_draining', d.host, d.id);
        if (d.state === 'failed') add('device_failed', d.host, d.id);
      }
      const withRoom = nbs.hosts.filter((h) => nbs.devices.some((d) => d.host === h.host && d.state === 'active')).length;
      if (withRoom < 3) add('no_room', null, null, withRoom);
      for (const h of cluster.hosts) {
        if (h.maintenance && nbs.hosts.some((x) => x.host === h.name)) add('maintenance', h.name);
      }
    }
    if (!out.length) add('ok');
    out.sort((a, b) => order.indexOf(a.kind) - order.indexOf(b.kind));
    return { alerts: out.map((a, id) => ({ id, ...a })) };
  },
  'nbs.volumes': () => {
    nbsMember();
    return { volumes: nbs.volumes.map((v) => nbsJson(v, false)) };
  },
  'vnet.status': () => (!vnet.member ? { member: false, stopped: null } : {
    member: true, vnet: vnet.name, node: 4011, role: 'leader', term: 2, leader: 'mock-a', applied: 30, members: 1,
    voters: 1, learners: 0, networks: vnet.networks.length, ports: vnet.ports.length, public: vnet.public.length,
    firewalls: vnet.firewalls.length, revision: 7, refused: 0, stopped: null }),
  'vnet.hosts': () => ({ hosts: vnet.hosts }),
  'vnet.networks': () => ({ networks: vnet.networks.map((n) => {
    const ports = vnet.ports.filter((x) => x.network === n.name).length;
    const gw = [...n.net];
    gw[3] += 1;
    return { name: n.name, vni: n.vni, subnet: quad(n.net) + '/' + n.prefix, gateway: quad(gw), mtu: n.mtu, out: n.out,
      ports, free: 2 ** (32 - n.prefix) - 3 - ports };
  }) }),
  'vnet.ports': () => ({ ports: vnet.ports.map((x) => ({ ...x,
    public: (vnet.public.find((a) => a.port === x.id) || {}).address || null })) }),
  'vnet.public': () => ({ public: vnet.public }),
  'vnet.firewalls': () => ({ firewalls: vnet.firewalls.map((f) => ({ name: f.name, in: f.in, out: f.out,
    rules: f.rules.length, revision: f.revision, ports: vnet.ports.filter((x) => x.firewall === f.name).length })) }),
  'vnet.firewall': (p) => {
    const f = vnetFirewall(p);
    return { name: f.name, in: f.in, out: f.out, revision: f.revision, rules: f.rules.map((rule, i) => ({ n: i + 1, rule })) };
  },
  'vnet.rules': () => ({ rules: vnet.firewalls.flatMap((f) => f.rules.map((rule, i) => ({
    id: f.name + '/' + (i + 1), firewall: f.name, n: i + 1, rule }))) }),
  'vnet.alerts': () => ({ alerts: [] }),
  'nbs.volume': (p) => {
    nbsMember();
    return nbsJson(nbsVolume(p), true);
  },
};

const actions = {
  'vnet.network_create': (p) => {
    const name = need(p, 'name');
    if (!VNET_NAME.test(name)) throw new Refused('invalid', 'not a name: lowercase letters, digits and -, a network\'s up to 26', 'subnet');
    const { net, prefix } = vnetSubnet(need(p, 'subnet'));
    const mtu = p.mtu === undefined ? 1420 : Number(p.mtu);
    const had = vnet.networks.find((n) => n.name === name);
    if (had) throw new Refused('conflict', 'a network of that name is there already, another');
    const [a0, a1] = vnetSpan({ net, prefix });
    if (vnet.networks.some((n) => { const [b0, b1] = vnetSpan(n); return a0 <= b1 && b0 <= a1; })) {
      throw new Refused('conflict', 'the subnet overlaps another network\'s, or holds a public address');
    }
    const n = { name, vni: vnet.nextVni, net, prefix, mtu, out: p.out !== false };
    vnet.nextVni += 1;
    vnet.networks.push(n);
    return { name, vni: n.vni };
  },
  'vnet.network_delete': (p) => {
    const name = need(p, 'name');
    if (vnet.ports.some((x) => x.network === name)) {
      throw new Refused('conflict', 'ports are on the network -- their VMs deleted first, or their ports');
    }
    const had = vnet.networks.length;
    vnet.networks = vnet.networks.filter((n) => n.name !== name);
    return { name, deleted: vnet.networks.length !== had };
  },
  'vnet.port_delete': (p) => {
    const id = Number(need(p, 'port'));
    vnet.ports = vnet.ports.filter((x) => x.id !== id);
    const a = vnet.public.find((x) => x.port === id);
    if (a) a.port = null;
    return { port: id, public: a ? a.address : null };
  },
  'vnet.firewall_create': (p) => {
    const name = need(p, 'name');
    if (!vnet.firewalls.some((f) => f.name === name)) {
      vnet.firewalls.push({ name, in: p.in || 'deny', out: p.out || 'allow', rules: [], revision: 1 });
    }
    return { name };
  },
  'vnet.firewall_delete': (p) => {
    const f = vnetFirewall(p);
    if (f.name === 'default') throw new Refused('conflict', '`default` is never deleted');
    if (vnet.ports.some((x) => x.firewall === f.name)) throw new Refused('conflict', 'ports have the firewall -- each given another first');
    vnet.firewalls = vnet.firewalls.filter((x) => x !== f);
    return { name: f.name, deleted: true };
  },
  'vnet.firewall_add': (p) => {
    const f = vnetFirewall(p);
    const rule = need(p, 'rule');
    if (!RULE.test(rule)) throw new Refused('invalid', '"' + rule + '": a rule begins with allow or deny', 'rule');
    const at = p.at === undefined ? f.rules.length : Number(p.at) - 1;
    const rules = [...f.rules];
    rules.splice(at, 0, rule);
    vnetSetRules(f, rules);
    return { done: 'rule ' + (at + 1) + ': ' + rule };
  },
  'vnet.firewall_policy': (p) => {
    const f = vnetFirewall(p);
    vnetSetRules(f, f.rules, policyOf(p, 'in') || f.in, policyOf(p, 'out') || f.out);
    return { done: 'in ' + f.in + ', out ' + f.out };
  },
  'vnet.firewall_remove': (p) => {
    const f = vnetFirewall(p);
    const n = ruleNumber(p, 'n', f.rules.length);
    const rules = [...f.rules];
    const [rule] = rules.splice(n - 1, 1);
    vnetSetRules(f, rules);
    return { done: 'rule ' + n + ' taken out: ' + rule };
  },
  'vnet.firewall_move': (p) => {
    const f = vnetFirewall(p);
    const n = ruleNumber(p, 'n', f.rules.length);
    const to = ruleNumber(p, 'to', f.rules.length);
    const rules = [...f.rules];
    const [rule] = rules.splice(n - 1, 1);
    rules.splice(to - 1, 0, rule);
    vnetSetRules(f, rules);
    return { done: 'rule ' + n + ' is rule ' + to + ' now' };
  },
  'fw.create': (p) => {
    const name = need(p, 'name');
    if (!FIREWALL_NAME.test(name) || ['create', 'delete', 'set', 'help'].includes(name)) {
      throw new Refused('invalid', '"' + name + '" is not a firewall\'s name -- a letter or a digit, then letters, digits, . _ and -, at most 32', 'name');
    }
    if (firewalls.has(name)) {
      throw new Refused('conflict', 'there is a firewall ' + name + ' already');
    }
    firewallMade(name, policyOf(p, 'in') ?? 'deny', policyOf(p, 'out') ?? 'allow');
    const f = firewalls.get(name);
    return { name, in: f.in, out: f.out };
  },
  'fw.delete': (p) => {
    const [name] = firewallNamed(p);
    if (name === 'default') {
      throw new Refused('conflict', 'default is what a VM that names no firewall is judged by: it is changed, never deleted');
    }
    const naming = [...vms.values()].filter((v) => v.firewall === name).map((v) => v.name);
    if (naming.length) {
      throw new Refused('conflict', naming.join(', ') + ' name' + (naming.length === 1 ? 's ' : ' ') + name
        + ' in their vm.toml -- hv fw set <vm> <another> first');
    }
    firewalls.delete(name);
    return { deleted: name };
  },
  'fw.policy': (p) => {
    const [, f] = firewallNamed(p);
    const inbound = policyOf(p, 'in');
    const outbound = policyOf(p, 'out');
    if (inbound === undefined && outbound === undefined) {
      throw new Refused('invalid', 'is allow or deny: in, out or both are given', 'in');
    }
    f.in = inbound ?? f.in;
    f.out = outbound ?? f.out;
    f.policyIn = 0;
    f.policyOut = 0;
    return { in: f.in, out: f.out, cut: 0 };
  },
  'fw.add': (p) => {
    const [, f] = firewallNamed(p);
    const rule = String(need(p, 'rule')).trim().replace(/\s+/g, ' ');
    if (!RULE.test(rule)) {
      throw new Refused('invalid', 'allow or deny, in or out, then [tcp|udp|icmp] [ports] [from|to any|net|<a.b.c.d/n,...>]', 'rule');
    }
    if (f.rules.length >= 64) {
      throw new Refused('invalid', 'a firewall has 64 rules at most', 'rule');
    }
    const at = p.at === undefined ? f.rules.length + 1 : ruleNumber(p, 'at', f.rules.length + 1);
    f.rules.splice(at - 1, 0, rule);
    f.hits = f.rules.map(() => 0);
    return { n: at, rule, cut: 0 };
  },
  'fw.remove': (p) => {
    const [, f] = firewallNamed(p);
    const n = ruleNumber(p, 'n', f.rules.length);
    const [rule] = f.rules.splice(n - 1, 1);
    f.hits = f.rules.map(() => 0);
    return { rule, cut: 0 };
  },
  'fw.move': (p) => {
    const [, f] = firewallNamed(p);
    const n = ruleNumber(p, 'n', f.rules.length);
    const to = ruleNumber(p, 'to', f.rules.length);
    const [rule] = f.rules.splice(n - 1, 1);
    f.rules.splice(to - 1, 0, rule);
    f.hits = f.rules.map(() => 0);
    return { n: to, cut: 0 };
  },
  'fw.assign': (p) => {
    const [name] = firewallNamed(p, 'firewall');
    const v = vms.get(need(p, 'vm'));
    if (!v) {
      throw new Refused('not_found', 'no vm ' + p.vm + ', running or in the VMs\' folder', 'vm');
    }
    v.firewall = name;
    return { vm: v.name, firewall: name, id: v.id, live: v.state === 'running', kept: true, cut: 0 };
  },
  'nbs.volume_create': (p) => {
    const name = need(p, 'name');
    if (!NBS_NAME.test(name)) {
      throw new Refused('invalid', 'lowercase letters, digits, . _ and -, up to 63', 'name');
    }
    const size = Number(need(p, 'size_mib'));
    const had = nbs.volumes.find((v) => v.name === name);
    if (had) {
      if (had.size_mib !== size) {
        throw new Refused('conflict', 'a volume of that name is there already, of another size or kind');
      }
      return { volume: had.volume };
    }
    nbsRoom(nbsChunks(size));
    const host = p.host || nbs.hosts[0].host;
    if (!nbs.hosts.some((h) => h.host === host)) {
      throw new Refused('not_found', 'the host is no member of the NBS cluster', 'host');
    }
    const v = { name, volume: nbs.next, size_mib: size, host, writer: null, generation: 0, readers: [] };
    nbs.next += 1;
    nbs.volumes.push(v);
    return { volume: v.volume };
  },
  'nbs.volume_resize': (p) => {
    const v = nbsVolume(p);
    const size = Number(need(p, 'size_mib'));
    if (size < v.size_mib) {
      throw new Refused('invalid', 'a volume is never made smaller');
    }
    nbsRoom(nbsChunks(size) - nbsChunks(v.size_mib));
    v.size_mib = size;
    return { volume: v.volume };
  },
  'nbs.volume_delete': (p) => {
    const at = nbs.volumes.findIndex((v) => v.name === need(p, 'name'));
    if (at < 0) {
      return { volume: 0 };
    }
    const [v] = nbs.volumes.splice(at, 1);
    return { volume: v.volume };
  },
  'nbs.volume_snapshot': (p) => {
    const v = nbs.volumes.find((x) => x.name === need(p, 'volume'));
    if (!v) {
      throw new Refused('not_found', 'no volume of that name', 'volume');
    }
    if ((v.kind || 'volume') !== 'volume') {
      throw new Refused('conflict', 'an image or a snapshot is read-only');
    }
    if (v.writer && v.writer !== nbs.hosts[0].host) {
      throw new Refused('conflict', `volume ${v.name} is attached on host ${v.writer}: its snapshot is asked there, `
                        + 'where its writer is paused for it');
    }
    return nbsShare(v, need(p, 'name'), 'snapshot', v.host);
  },
  'nbs.volume_freeze': (p) => {
    const v = nbsVolume(p);
    if ((v.kind || 'volume') === 'snapshot') {
      throw new Refused('conflict', 'an image or a snapshot is read-only');
    }
    if (v.writer || v.readers.length) {
      throw new Refused('conflict', 'the volume is attached');
    }
    v.kind = 'image';
    return { volume: v.volume };
  },
  'nbs.volume_clone': (p) => {
    const from = nbs.volumes.find((x) => x.name === need(p, 'from'));
    if (!from) {
      throw new Refused('not_found', 'no volume of that name', 'from');
    }
    if ((from.kind || 'volume') === 'volume') {
      throw new Refused('conflict', 'a clone is of an image or a snapshot -- freeze or snapshot the volume first');
    }
    const host = p.host || nbs.hosts[0].host;
    if (!nbs.hosts.some((h) => h.host === host)) {
      throw new Refused('not_found', 'the host is no member of the NBS cluster', 'host');
    }
    return nbsShare(from, need(p, 'name'), 'volume', host);
  },
  /* A snapshot of a VM's disk that is a volume of NBS's -- its first, or
   * `disk` -- as the vm provider takes one; a VM of a file refused. */
  'vm.snapshot': (p) => {
    const v = vmNamed(p);
    const snap = need(p, 'snapshot');
    if (!NBS_NAME.test(snap)) {
      throw new Refused('invalid', "is a volume's name: lowercase letters, digits, . _ and -, a letter or a digit "
        + 'first, up to 63', 'snapshot');
    }
    const field = p.disk !== undefined ? 'disk' : 'name';
    const d = p.disk !== undefined ? v.disks.find((x) => x.file === p.disk) : v.disks[0];
    if (!d) {
      throw new Refused('not_found', p.disk !== undefined ? "is none of " + v.name + "'s disks" : v.name + ' has no disk',
        field);
    }
    if (!String(d.file).startsWith('nbs:')) {
      throw new Refused('conflict', v.name + "'s disk " + diskPath(v, d) + " is no volume of NBS's, and a snapshot is "
        + 'of one -- a clone of the vm copies it whole', field);
    }
    nbsMember();
    const volume = String(d.file).slice('nbs:'.length);
    const from = nbs.volumes.find((x) => x.name === volume);
    if (!from) {
      throw new Refused('failed', 'nbs:' + snap + ' of nbs:' + volume + ': no volume of that name');
    }
    const had = nbs.volumes.find((x) => x.name === snap);
    if (had) {
      throw new Refused('failed', 'nbs:' + snap + ' of nbs:' + volume + ': ' + snap + ' is a ' + (had.kind || 'volume')
        + ' already, made before: a name is not taken again -- delete it first');
    }
    const made = nbsShare(from, snap, 'snapshot', from.host);
    return { vm: v.name, volume, snapshot: snap, number: made.volume };
  },
  'nbs.init': (p) => {
    if (nbs.member) {
      throw new Refused('conflict', 'this host is a member of an NBS cluster already', 'name');
    }
    const name = need(p, 'name');
    if (!HOST_NAME.test(name)) {
      throw new Refused('invalid', '"' + name + '" is not a cluster\'s name: lowercase letters, digits and -', 'name');
    }
    nbs.member = true;
    nbs.name = name;
    nbs.hosts = [{ host: me.name, member: nbs.nextMember++, vote: 'voter', leader: true }];
    return { cluster: name, id: crypto.randomBytes(16).toString('hex'), host: me.name, member: nbs.hosts[0].member };
  },
  'nbs.host_remove': (p) => {
    nbsMember();
    const host = nbsHostNamed(p);
    const at = nbs.hosts.findIndex((h) => h.host === host);
    if (at < 0) {
      throw new Refused('conflict', host + ' is no member -- `nbs host add ' + host + '`', 'host');
    }
    const [h] = nbs.hosts.splice(at, 1);
    nbs.devices = nbs.devices.filter((d) => d.host !== host);
    return { host, member: h.member };
  },
  /* A device of this host's to its chunk server, as store::device_add takes
   * it: refused where something holds it, or something not NBS's is on it
   * and wipe is not said. */
  'nbs.device_add': (p) => {
    nbsMember();
    const word = need(p, 'device');
    const d = DEVICES.find((x) => x.id === word || x.name === word || 'dev:' + x.name === word);
    if (!d) {
      throw new Refused('failed', 'no device is ' + word, 'device');
    }
    const had = nbs.devices.find((x) => x.id === d.id && x.host === me.name);
    if (had) {
      had.state = 'active';
      return { id: d.id, device: had.device, size: had.size, wiped: false };
    }
    if (d.in_use_by || d.boots_the_machine) {
      throw new Refused('failed', d.id + ' is held by ' + (d.in_use_by || d.boots_the_machine), 'device');
    }
    if (d.holds && p.wipe !== true) {
      throw new Refused('failed', d.id + " has something on it that is not NBS's -- a filesystem, a partition table? "
        + '-- `nbs devices add ' + word + ' wipe` takes it all the same, and what is on it is lost', 'device');
    }
    const size = Math.floor(d.size_bytes / SLOT) * SLOT;
    const added = { host: me.name, device: nbs.nextDevice++, id: d.id, size, state: 'active' };
    nbs.devices.push(added);
    const wiped = Boolean(d.holds);
    Object.assign(d, { holds: "NBS's chunk server's", in_use_by: "NBS's chunk server", held_by: 'nbs',
      refused: d.name + " is in use by NBS's chunk server" });
    return { id: d.id, device: added.device, size, wiped };
  },
  'nbs.device_drain': (p) => {
    nbsMember();
    const id = need(p, 'device');
    const host = p.host ?? me.name;
    const d = nbs.devices.find((x) => x.id === id && x.host === host);
    if (!d) {
      throw new Refused('not_found', 'no device ' + id + ' of ' + host + "'s chunk server");
    }
    d.state = 'draining';
    return { id, device: d.device };
  },
  'cluster.init': (p) => {
    clusterFresh('name');
    const name = need(p, 'name');
    if (!HOST_NAME.test(name)) {
      throw new Refused('invalid', '"' + name + '" is not a cluster\'s name: lowercase letters, digits and -', 'name');
    }
    cluster.member = true;
    cluster.name = name;
    cluster.hosts = [{ name: me.name, node: 1, vote: 'voter', leader: true, ip: me.ip, underlay: null, public: null,
      maintenance: false, key: me.key, moving_to: null }];
    return { cluster: name, host: me.name, node: 1 };
  },
  'cluster.invite': (p) => {
    clusterMember();
    const host = need(p, 'host');
    if (!/^[0-9a-f]{64}$/.test(need(p, 'key'))) {
      throw new Refused('invalid', '64 hex digits, as `ipcnet id` says them', 'key');
    }
    if (clusterHost(host) || cluster.invitations.some((i) => i.host === host)) {
      throw new Refused('failed', 'a host of that name is in the cluster or invited');
    }
    const expires_s = Math.floor(Date.now() / 1000) + 900;
    cluster.invitations.push({ host, ip: need(p, 'ip'), expires_s });
    return { host, token: 'nos1-' + crypto.randomBytes(40).toString('hex'), expires_s };
  },
  'cluster.revoke': (p) => {
    clusterMember();
    const at = cluster.invitations.findIndex((i) => i.host === need(p, 'host'));
    if (at < 0) {
      throw new Refused('not_found', 'no open invitation of ' + p.host);
    }
    cluster.invitations.splice(at, 1);
    return { host: p.host };
  },
  'cluster.remove': (p) => {
    clusterMember();
    const h = clusterHost(need(p, 'host'));
    if (!h) {
      throw new Refused('not_found', 'no host of that name');
    }
    cluster.hosts = cluster.hosts.filter((x) => x !== h);
    cluster.removed += 1;
    return { host: h.name, node: h.node };
  },
  'cluster.maintenance': (p) => {
    clusterMember();
    const h = clusterHost(need(p, 'host'));
    if (!h) {
      throw new Refused('not_found', 'no host of that name');
    }
    h.maintenance = Boolean(need(p, 'on'));
    return { host: h.name, maintenance: h.maintenance };
  },
  'cluster.address': (p) => {
    clusterMember();
    const h = clusterHost(need(p, 'host'));
    if (!h) {
      throw new Refused('not_found', 'no host of that name');
    }
    const role = need(p, 'role');
    h[role] = p.ip ?? null;
    return { host: h.name, role, ip: p.ip ?? null };
  },
  'cluster.rekey': () => {
    clusterMember();
    const me = cluster.hosts[0];
    const old = me.key;
    me.key = crypto.randomBytes(32).toString('hex');
    return { old, new: me.key };
  },
  'system.set_log_level': (p) => ({ log_level: need(p, 'level') }),
  'system.set_nice': (p) => ({ pid: need(p, 'pid'), nice: need(p, 'nice') }),
  'system.load_module': (p) => ({ loaded: need(p, 'name'), said: 'module: ' + p.name + ' loaded' }),
  'system.unload_module': (p) => ({ unloaded: need(p, 'name'), said: 'module: ' + p.name + ' unloaded' }),
  'system.power_off': () => ({}),
  'system.reboot': () => ({}),
  'vm.config_set': (p) => {
    vmNamed(p);
    need(p, 'text');
    return { changed: true };
  },
  'vm.forward_add': (p) => {
    const v = vmNamed(p);
    v.forwards = v.forwards.filter((f) => f.port !== need(p, 'port')).concat([{ port: p.port, guest_port: need(p, 'guest_port') }]);
    return { kept: true, live: v.state === 'running' };
  },
  'vm.forward_del': (p) => {
    const v = vmNamed(p);
    const had = v.forwards.length;
    v.forwards = v.forwards.filter((f) => f.port !== need(p, 'port'));
    if (had === v.forwards.length) {
      throw new Refused('not_found', 'port ' + p.port + ' is not forwarded to ' + v.name, 'port');
    }
    return { kept: true, live: v.state === 'running' };
  },
  'vm.console_write': (p) => {
    const v = vmNamed(p);
    if (v.state !== 'running') {
      throw new Refused('conflict', 'vm ' + v.id + ' is not running');
    }
    /* A line typed is echoed, as a shell's would be. */
    v.console += String(need(p, 'text')).replace(/\r/g, '\r\n');
    return { queued: p.text.length };
  },
  'vm.disk_add': (p) => {
    const v = vmNamed(p);
    v.disks.push({ file: need(p, 'file'), size_bytes: need(p, 'size_bytes'), read_only: false });
    return { path: '/vms/' + v.name + '/' + p.file, size_bytes: p.size_bytes };
  },
  'vm.disk_attach': (p) => {
    const v = vmNamed(p);
    const d = deviceGiven(need(p, 'device'), p.force === true);
    if (v.state === 'running') {
      throw new Refused('conflict', v.name + ' is on the list -- hv stop it first');
    }
    v.disks.push(deviceDisk(d, p.read_only === true));
    return { vm: v.name, id: d.id, read_only: p.read_only === true };
  },
  'vm.pci_attach': (p) => {
    const v = vmNamed(p);
    const at = need(p, 'address');
    const f = PCI.find((x) => x.address === at);
    if (!f) {
      throw new Refused('conflict', at + ': no such function', 'address');
    }
    if (f.refused) {
      throw new Refused('conflict', at + ': ' + f.refused, 'address');
    }
    if (v.state === 'running') {
      throw new Refused('conflict', v.name + ' is on the list -- hv stop it first');
    }
    v.pci = v.pci ?? [];
    if (v.pci.includes(at)) {
      throw new Refused('conflict', v.name + "'s vm.toml names " + at + ' already', 'address');
    }
    v.pci.push(at);
    return { vm: v.name, address: at };
  },
  'vm.pci_detach': (p) => {
    const v = vmNamed(p);
    const at = need(p, 'address');
    const i = (v.pci ?? []).indexOf(at);
    if (i < 0) {
      throw new Refused('conflict', v.name + "'s vm.toml names no function " + at, 'address');
    }
    v.pci.splice(i, 1);
    return { vm: v.name, address: at };
  },
  'vm.disk_detach': (p) => {
    const v = vmNamed(p);
    const word = need(p, 'device');
    const at = v.disks.findIndex((x) => x.device && (x.file === word || 'dev:' + x.device_name === word));
    if (at < 0) {
      throw new Refused('not_found', v.name + "'s vm.toml names no device " + word, 'device');
    }
    const [d] = v.disks.splice(at, 1);
    return { vm: v.name, id: d.file };
  },
  'vm.disk_grow': (p) => {
    const v = vmNamed(p);
    const d = v.disks.find((x) => x.file === need(p, 'file'));
    if (!d) {
      throw new Refused('not_found', 'is none of its disks', 'file');
    }
    d.size_bytes = Math.max(d.size_bytes, need(p, 'size_bytes'));
    return { size_bytes: d.size_bytes };
  },
};

const SEED_DISK = { file: 'seed.iso', size_bytes: 65536, read_only: true };
const LOCAL_IMAGES = [
  { name: 'debian-local.qcow2', path: '/images/debian-local.qcow2', size_bytes: 620 * MIB },
  { name: 'alpine-local.raw', path: '/images/alpine-local.raw', size_bytes: 512 * MIB },
];
const createKernel = (p) => p.os === 'windows' ? '/lib/CLOUDHV.fd' : '/lib/hvboot';
const createDisks = (p, first) => p.os === 'windows' ? [first] : [first, SEED_DISK];

/** A VM of an image of NBS's (`image=nbs:<image>`): a clone of it its disk,
 * named the VM's unless onto=nbs:<volume> names it, grown to size_bytes --
 * refused as cloud::create refuses. */
function createOfImage(name, p) {
  nbsMember();
  const image = nbs.volumes.find((v) => v.name === p.image);
  if (!image) {
    throw new Refused('failed', 'no image nbs:' + p.image + ' -- `hv image` makes one');
  }
  if ((image.kind || 'volume') === 'volume') {
    throw new Refused('failed', 'nbs:' + image.name + ' is a volume: a VM is made of an image, or a snapshot -- '
      + '`hv image` makes one');
  }
  let clone = name;
  if (p.onto !== undefined) {
    if (!String(p.onto).startsWith('nbs:')) {
      throw new Refused('failed', "onto= of a VM of an image is its clone's name, nbs:<volume>");
    }
    clone = String(p.onto).slice(4);
  }
  if (nbs.volumes.some((v) => v.name === clone)) {
    throw new Refused('failed', 'nbs:' + clone + " is there already: a VM's disk is a clone of its own -- "
      + 'onto=nbs:<volume> names it otherwise');
  }
  const sizeMib = p.size_bytes !== undefined ? Math.ceil(p.size_bytes / MIB) : image.size_mib;
  if (sizeMib < image.size_mib) {
    throw new Refused('failed', "size= is less than the image's disk, " + image.size_mib + ' MiB');
  }
  return job('vm', 'create', 2, (i) => ['hv: nbs:' + clone + ' cloned from nbs:' + image.name + ', ' + sizeMib
    + ' MiB -- thin: its chunks copied as they are written', '/vms/' + name + '/vm.toml written'][i], () => {
    nbsShare(image, clone, 'volume', nbs.hosts[0].host);
    nbs.volumes.find((v) => v.name === clone).size_mib = sizeMib;
    vm(name, { memory_mib: p.memory_mib ?? 1024, cpus: p.cpus ?? 1, node: p.node ?? null, network: p.network ?? null,
               firewall: p.firewall ?? 'default', public: p.public ?? null, kernel: createKernel(p),
      disks: createDisks(p, { file: 'nbs:' + clone, size_bytes: null, read_only: false }) });
    return { name, file: '/vms/' + name + '/vm.toml', disk_bytes: sizeMib * MIB, keys: p.os === 'windows' ? 0 : 1,
             port: null, id: null };
  });
}

/** A URL's or local file's image poured onto a volume of NBS's
 * (onto=nbs:<volume>): over what is on it, taken from a writer elsewhere with
 * force. */
function createOntoVolume(name, p) {
  nbsMember();
  const word = p.onto.slice(4);
  const v = nbs.volumes.find((x) => x.name === word);
  if (!v) {
    throw new Refused('failed', 'no volume nbs:' + word);
  }
  if ((v.kind || 'volume') !== 'volume') {
    throw new Refused('failed', 'nbs:' + word + ' is an ' + v.kind + ", read-only: a URL's image is poured onto a volume");
  }
  if (p.size_bytes !== undefined) {
    throw new Refused('invalid', "size= is a file's: a device's disk is the device's size, a volume's the volume's",
      'size_bytes');
  }
  if (v.writer && v.writer !== me.name && p.force !== true) {
    throw new Refused('conflict', 'nbs:' + word + ' is attached on host ' + v.writer + ' -- force takes it', 'onto');
  }
  const lines = [
    ...(p.file === undefined ? ['download: 120 MiB of 590 MiB, 20%', 'download: 590 MiB of 590 MiB, 100%'] : []),
    'nbs:' + word + ': 3584 MiB poured, the rest of the volume zeroes', '/vms/' + name + '/vm.toml written'];
  return job('vm', 'create', lines.length, (i) => lines[i], () => {
    vm(name, { memory_mib: p.memory_mib ?? 1024, cpus: p.cpus ?? 1, node: p.node ?? null, network: p.network ?? null,
               firewall: p.firewall ?? 'default', public: p.public ?? null, kernel: createKernel(p),
      disks: createDisks(p, { file: 'nbs:' + word, size_bytes: null, read_only: false }) });
    return { name, file: '/vms/' + name + '/vm.toml', disk_bytes: v.size_mib * MIB, keys: p.os === 'windows' ? 0 : 1,
             port: null, id: null };
  });
}

const guarded = (v, p) => {
  if (v.protected && !p.force) {
    throw new Refused('conflict', v.name + ' is protected -- force does it all the same', 'force');
  }
};

const jobMethods = {
  /* An image of NBS's made of a URL's image or a file's, as `hv image`
   * makes one: downloaded, a volume of its disk's size, poured, frozen. */
  'vm.image': (p) => {
    const given = String(need(p, 'name'));
    const name = given.startsWith('nbs:') ? given.slice('nbs:'.length) : given;
    if (!NBS_NAME.test(name)) {
      throw new Refused('invalid', "is a volume's name: lowercase letters, digits, . _ and -, a letter or a digit "
        + 'first, up to 63', 'name');
    }
    const from = String(need(p, 'from'));
    const url = /^https?:\/\//.test(from);
    if (!url && !from.startsWith('/')) {
      throw new Refused('invalid', "is an http:// or https:// URL, or a file of nos's by its path", 'from');
    }
    nbsMember();
    if (nbs.volumes.some((x) => x.name === name)) {
      throw new Refused('failed', 'nbs:' + name + ' is there already');
    }
    const steps = [...(url ? ['download: 120 MiB of 590 MiB, 20%', 'download: 590 MiB of 590 MiB, 100%'] : []),
      'hv: ' + (url ? '/vms/.image-' + name : from) + ': qcow2, a disk of 3584 MiB -- onto nbs:' + name
        + ', 3584 MiB, fresh: its zeros left out',
      'hv: nbs:' + name + ' is an image now, 3584 MiB, read-only for good -- hv create <vm> image=nbs:' + name
        + ' makes a vm of a clone of it'];
    return job('vm', 'image', steps.length, (i) => steps[i], () => {
      nbs.volumes.push({ name, volume: nbs.next, kind: 'image', origin: null, size_mib: 3584, host: nbs.hosts[0].host,
        writer: null, generation: 0, readers: [] });
      nbs.next += 1;
      return { image: name, size_bytes: 3584 * MIB };
    });
  },
  'nbs.host_add': (p) => {
    nbsMember();
    const host = nbsHostNamed(p);
    if (nbs.hosts.some((h) => h.host === host)) {
      throw new Refused('failed', host + ' is a member already', 'host');
    }
    return job('nbs', 'host_add', 2, (i) => [host + ' offered the NBS cluster', host + ' took the offer'][i], () => {
      /* The offer reaches a host of the registry's, its mds there */
      if (!clusterHost(host)) {
        throw new Refused('failed', 'the offer to ' + host + ' stopped: ' + host + ' out of reach: no route -- is it a '
          + 'host of the cluster registry, with nbs-mds loaded?');
      }
      const m = { host, member: nbs.nextMember++, vote: nbs.hosts.length < 5 ? 'voter' : 'learner', leader: false };
      nbs.hosts.push(m);
      return { host, member: m.member };
    });
  },
  'nbs.host_replace': (p) => {
    nbsMember();
    const host = nbsHostNamed(p);
    const h = nbs.hosts.find((x) => x.host === host);
    if (!h) {
      throw new Refused('conflict', host + ' is no member -- `nbs host add ' + host + '`', 'host');
    }
    return job('nbs', 'host_replace', 2, (i) => [host + "'s old member " + h.member + ' removed',
      host + ' offered the NBS cluster anew'][i], () => {
      const removed = h.member;
      h.member = nbs.nextMember++;
      return { host, removed, member: h.member };
    });
  },
  'cluster.join': (p) => {
    clusterFresh('token');
    const token = need(p, 'token');
    if (!/^nos1-[0-9a-f]{16,}$/.test(token)) {
      throw new Refused('invalid', 'not a token: `cluster invite` prints one', 'token');
    }
    return job('cluster', 'join', 2, (i) => ['cluster: the join asked of the inviting member',
      'cluster: ' + me.name + ' let in as node 5'][i], () => {
      cluster.member = true;
      cluster.name = 'test';
      cluster.hosts = [
        { name: 'mock-a', node: 1, vote: 'voter', leader: true, ip: '10.9.0.1', underlay: null, public: null,
          maintenance: false, key: crypto.createHash('sha256').update('mock-a').digest('hex'), moving_to: null },
        { name: me.name, node: 5, vote: 'learner', leader: false, ip: me.ip, underlay: null, public: null,
          maintenance: false, key: me.key, moving_to: null },
      ];
      return { host: me.name, node: 5, hosts: cluster.hosts.length };
    });
  },
  'vm.start': (p) => {
    const v = vmNamed(p);
    if (v.state === 'running') {
      throw new Refused('conflict', 'vm ' + v.id + ' is ' + v.name + ' already', 'name');
    }
    return job('vm', 'start', 1, () => 'vm ' + nextVmId + ' (' + v.name + ') started on cpu 2', () => {
      startVm(v);
      return { id: v.id };
    });
  },
  'vm.stop': (p) => {
    const v = vmNamed(p);
    guarded(v, p);
    return job('vm', 'stop', 2, (i) => (i === 0 ? 'its power button pressed' : 'it turned itself off'), () => {
      const id = v.id;
      v.state = 'off';
      v.id = null;
      return { id, reason: 'it turned itself off (ACPI S5)', asked: 'answered', forwards_taken: 0 };
    });
  },
  /* A running VM moved to another host of the cluster's, as `hv migrate`
   * moves it: refused for this host, for a host the cluster has not and for
   * a VM that is not running; its rounds said as they go, and then it is
   * gone from this host's list -- the other host's now. */
  'vm.migrate': (p) => {
    const v = vmNamed(p);
    const host = need(p, 'host');
    if (host === me.name) {
      throw new Refused('failed', host + ' is this host');
    }
    if (!clusterHost(host)) {
      throw new Refused('failed', v.name + ' not moved -- hv.migrate@' + host + ' not reached: no_route');
    }
    if (v.state !== 'running') {
      throw new Refused('failed', 'vm ' + v.name + ' is not running');
    }
    const cold = p.cold === true;
    const mib = v.memory_mib;
    const lines = cold
      ? [v.name + ' offered to ' + host + ': ' + mib + ' MiB, ' + v.cpus + ' cpu, 1 file of its folder',
        v.name + ' round 0: 212 MiB of its memory (190000 pages of zeros) and 14 KiB of files in 2210 ms',
        v.name + ' sent in 1 round: 212 MiB of its memory, 14 KiB of files, its state 2 KiB, 4 flows of its firewall'
          + ' -- paused 2230 ms so far',
        v.name + ' runs on ' + host + ' -- paused 2260 ms; its folder here taken away']
      : [v.name + ' offered to ' + host + ': ' + mib + ' MiB, ' + v.cpus + ' cpu, 1 file of its folder',
        v.name + ' round 0: 212 MiB of its memory (190000 pages of zeros) and 14 KiB of files in 2210 ms, the pages it'
          + ' wrote meanwhile marked',
        v.name + ' round 1: 1840 pages (12 of zeros) in 74 ms; 210 written meanwhile',
        v.name + ' sent in 3 rounds: 220 MiB of its memory, 14 KiB of files, its state 2 KiB, 4 flows of its firewall'
          + ' -- paused 190 ms so far',
        v.name + ' runs on ' + host + ' -- paused 212 ms; its folder here taken away'];
    return job('vm', 'migrate', lines.length, (i) => lines[i], () => {
      vms.delete(v.name);
      return { vm: v.name, host, rounds: cold ? 1 : 3, paused_ms: cold ? 2260 : 212, sent_bytes: (cold ? 212 : 220) * MIB,
               flows: 4 };
    });
  },
  'vm.restart': (p) => {
    const v = vmNamed(p);
    return job('vm', 'restart', 1, () => 'booting again -- on request', () => {
      v.restarts += 1;
      v.bootMs = uptimeMs();
      return { id: v.id };
    });
  },
  'vm.create': (p) => {
    const name = need(p, 'name');
    const sources = ['url', 'file', 'image'].filter((key) => p[key] !== undefined);
    if (sources.length > 1) {
      throw new Refused('invalid', 'a VM is made of one URL, local file or NBS image', sources[1]);
    }
    if (sources.length === 0) {
      throw new Refused('invalid', "is wanted -- or a local image, `file`, or an image of NBS's, `image`", 'url');
    }
    if (p.file !== undefined && !LOCAL_IMAGES.some((image) => image.path === p.file)) {
      if (typeof p.file !== 'string' || !/^\/images\/[^/]+$/.test(p.file)) {
        throw new Refused('invalid', "is a file's path directly under /images", 'file');
      }
      throw new Refused('failed', p.file + ': no such file', 'file');
    }
    if (vms.has(name)) {
      throw new Refused('conflict', name + ' is there already', 'name');
    }
    /* A node the host has: system.numa's two */
    if (p.node !== undefined && !(Number.isInteger(p.node) && p.node >= 0 && p.node < 2)) {
      throw new Refused('invalid', 'is a NUMA node of this machine\'s: it has 2 (system.numa)', 'node');
    }
    /* On a network of the cluster's: its port made as hv makes it, of
     * vnet's, judged by one of the cluster's firewalls */
    const cluster = typeof p.network === 'string' && p.network.startsWith('vnet:') ? p.network.slice(5) : null;
    if (cluster !== null) {
      if (!vnet.networks.some((n) => n.name === cluster)) {
        throw new Refused('failed', 'vnet:' + cluster + ': no network ' + cluster);
      }
      if (p.public !== undefined) {
        throw new Refused('invalid', "port= and public= are the host's networks'", 'public');
      }
      const fw = p.firewall ?? 'default';
      if (!vnet.firewalls.some((f) => f.name === fw)) {
        throw new Refused('failed', 'vnet:' + cluster + ': no firewall ' + fw);
      }
      const id = Math.max(0, ...vnet.ports.map((x) => x.id)) + 1;
      const taken = new Set(vnet.ports.filter((x) => x.network === cluster).map((x) => x.address));
      let host = 2;
      while (taken.has('10.10.0.' + host)) host += 1;
      vnet.ports.push({ id, network: cluster, address: '10.10.0.' + host, host: 'mock-a', generation: id, firewall: fw,
                        vm: '00000000-0000-4000-8000-' + String(id).padStart(12, '0'), nic: 0 });
    } else if (p.firewall !== undefined && !firewalls.has(p.firewall)) {
      throw new Refused('invalid', 'no firewall ' + p.firewall + ' on this host -- hv fw create ' + p.firewall, 'firewall');
    }
    if (p.image !== undefined) {
      return createOfImage(name, p);
    }
    if (typeof p.onto === 'string' && p.onto.startsWith('nbs:')) {
      return createOntoVolume(name, p);
    }
    /* Onto a device: refused before anything is fetched, as the host does. */
    const onto = p.onto ? deviceGiven(p.onto, p.force === true) : null;
    if (onto && p.size_bytes !== undefined) {
      throw new Refused('invalid', "size= is a file's: a device's disk is the device's size", 'size_bytes');
    }
    const lines = [
      ...(p.file === undefined ? ['download: 120 MiB of 590 MiB, 20%', 'download: 590 MiB of 590 MiB, 100%'] : []),
      p.file ? p.file + (onto ? ': qcow2, a disk of 3584 MiB -- onto ' + onto.name
        : ': qcow2 version 3 -- into /vms/' + name + '/disk.raw')
        : onto ? '/vms/' + name + '/image: qcow2, a disk of 3584 MiB -- onto ' + onto.name
          : 'qcow2 version 3 -- into /vms/' + name + '/disk.raw',
      onto ? onto.name + ': 3584 MiB written in 4.2 s, the rest of the device given back to it'
        : 'disk.raw: 1785 MiB written of 3584 MiB, the rest holes',
      '/vms/' + name + '/vm.toml written'];
    return job('vm', 'create', lines.length, (i) => lines[i], () => {
      vm(name, { memory_mib: p.memory_mib ?? 1024, cpus: p.cpus ?? 1, node: p.node ?? null, network: p.network ?? null,
                 firewall: p.firewall ?? 'default', public: p.public ?? null, kernel: createKernel(p),
                 disks: createDisks(p, onto ? deviceDisk(onto, false)
                   : { file: 'disk.raw', size_bytes: p.size_bytes ?? 20 * 1024 * MIB, read_only: false }) });
      return { name, file: '/vms/' + name + '/vm.toml', disk_bytes: onto ? onto.size_bytes : p.size_bytes ?? 3584 * MIB,
               keys: p.os === 'windows' ? 0 : 1 };
    });
  },
  'vm.clone': (p) => {
    const v = vmNamed(p);
    const to = need(p, 'to');
    if (vms.has(to)) {
      throw new Refused('conflict', to + ' is there already', 'to');
    }
    return job('vm', 'clone', 3, (i) => ['disk.raw -- 20480 MiB, 2215 MiB of it data', '1100 of 2215 MiB copied',
      'vm.toml written'][i], () => {
      vm(to, { cpus: v.cpus, memory_mib: v.memory_mib });
      return { name: to, file: '/vms/' + to + '/vm.toml', port: null, seed: true,
        copied: [{ path: '/vms/' + to + '/disk.raw', size_bytes: 20 * 1024 * MIB, written_bytes: 2190 * MIB }] };
    });
  },
  'vm.delete': (p) => {
    const v = vmNamed(p);
    guarded(v, p);
    if (v.state !== 'off') {
      throw new Refused('conflict', v.name + ' is on the list -- stop it first', 'name');
    }
    return job('vm', 'delete', 1, () => v.name + ' deleted -- 6 files and folders taken away', () => {
      vms.delete(v.name);
      return { removed: 6, kept: [] };
    });
  },
};

/* ---- HTTP ---- */

const STATUS = { invalid: 400, not_found: 404, no_provider: 404, no_method: 404, forbidden: 403, conflict: 409,
  kind: 409, cancelled: 409, failed: 500, too_large: 413, busy: 429, unsupported: 501, gone: 503 };

function send(res, status, body, headers = {}) {
  const text = body === undefined ? '' : JSON.stringify(body);
  res.writeHead(status, { 'Content-Type': 'application/json', 'Content-Length': Buffer.byteLength(text),
    Server: 'nos-mock', ...headers });
  res.end(text);
}
const error = (res, status, code, message, field) =>
  send(res, status, { error: { code, message, ...(field ? { field } : {}) } });

/** The kind each method is registered as: its descriptor's. */
function kindOf(provider, method) {
  return all[provider]?.d.methods?.[method]?.kind;
}

function readBody(req) {
  return new Promise((resolve) => {
    const chunks = [];
    req.on('data', (c) => chunks.push(c));
    req.on('end', () => resolve(Buffer.concat(chunks)));
  });
}

async function route(req, res, who) {
  const url = new URL(req.url, 'https://nos');
  const parts = url.pathname.split('/').filter(Boolean);
  if (parts[0] !== 'v1') {
    return error(res, 404, 'not_found', 'no such route: the API is under /v1');
  }
  const body = await readBody(req);
  let params = {};
  if (body.length) {
    try {
      params = JSON.parse(body.toString('utf8'));
    } catch {
      return error(res, 400, 'invalid', 'the parameters are not JSON as the bus reads it');
    }
  }
  const admin = who.role === 'admin';
  const [, what, a, b, c] = parts;
  if (what === 'host' && req.method === 'GET') {
    return send(res, 200, { id: hostId, name: NAME, version: '0.0.28-mock',
      revision: 'mock', arch: 'x86_64', module_abi: 'mock', api: 1, capabilities: ['call', 'jobs', 'stream'],
      time_s: Math.floor(Date.now() / 1000), uptime_ms: uptimeMs(), key: hostPin, client: { name: who.name, role: who.role } });
  }
  if (what === 'providers' && req.method === 'GET' && !a) {
    return send(res, 200, { providers: Object.values(all).map(({ d }) => ({
      name: d.provider, methods: Object.entries(d.methods).filter(([, m]) => admin || m.kind === 'read')
        .map(([name, m]) => ({ name, kind: m.kind })) })) });
  }
  if (what === 'providers' && req.method === 'GET' && a) {
    const found = all[a];
    if (!found) {
      return error(res, 404, 'no_provider', 'no such provider: GET /v1/providers lists them');
    }
    const tag = '"' + crypto.createHash('sha256').update(found.text).digest('hex').slice(0, 16) + '"';
    if (req.headers['if-none-match'] === tag) {
      res.writeHead(304, { ETag: tag });
      return res.end();
    }
    res.writeHead(200, { 'Content-Type': 'application/json', ETag: tag, 'Content-Length': Buffer.byteLength(found.text) });
    return res.end(found.text);
  }
  if (what === 'call' && req.method === 'POST' && a && b) {
    const key = a + '.' + b;
    const kind = kindOf(a, b);
    if (!kind) {
      return error(res, 404, all[a] ? 'no_method' : 'no_provider', 'no such method');
    }
    if (kind === 'job') {
      return error(res, 409, 'kind', 'a job: POST /v1/jobs/<provider>/<method> starts it');
    }
    if (kind === 'action' && !admin) {
      return error(res, 403, 'forbidden', 'a client of the read role calls reads alone');
    }
    const fn = kind === 'read' ? reads[key] : actions[key];
    if (!fn) {
      return send(res, 200, {});
    }
    try {
      return send(res, 200, fn(params));
    } catch (e) {
      return e instanceof Refused ? error(res, STATUS[e.code] || 500, e.code, e.message, e.field) : error(res, 500, 'failed', String(e));
    }
  }
  if (what === 'jobs' && req.method === 'POST' && a && b && !c && !/^\d+$/.test(a)) {
    if (!admin) {
      return error(res, 403, 'forbidden', 'a client of the read role starts no job');
    }
    const kind = kindOf(a, b);
    if (kind !== 'job') {
      return error(res, kind ? 409 : 404, kind ? 'kind' : 'no_method', kind ? 'not a job' : 'no such method');
    }
    const fn = jobMethods[a + '.' + b];
    try {
      const j = fn ? fn(params) : job(a, b, 1, () => 'working', () => ({}));
      return send(res, 202, { id: j.id });
    } catch (e) {
      /* Refused before it began: a job that ends so at once. */
      const j = job(a, b, 0, () => '', () => {
        throw e;
      });
      return send(res, 202, { id: j.id });
    }
  }
  if (what === 'jobs' && req.method === 'GET' && !a) {
    return send(res, 200, { jobs: jobs.map(jobInfo) });
  }
  if (what === 'jobs' && a && /^\d+$/.test(a)) {
    const j = jobs.find((x) => x.id === Number(a));
    if (!j) {
      return error(res, 404, 'not_found', 'no such job');
    }
    if (b === 'cancel' && req.method === 'POST') {
      if (!admin) {
        return error(res, 403, 'forbidden', 'a client of the read role cancels no job');
      }
      if (j.state === 'done') {
        return error(res, 404, 'not_found', 'no such job running');
      }
      j.cancel = true;
      return send(res, 200, { cancelled: j.id });
    }
    if (!b && req.method === 'GET') {
      const since = Number(url.searchParams.get('notes_since') || 0);
      return send(res, 200, { ...jobInfo(j), notes: j.notes.filter((n) => n.seq >= since), result: j.result });
    }
  }
  if (what === 'stream') {
    return error(res, 426, 'invalid', 'a WebSocket: Upgrade: websocket');
  }
  return error(res, 404, 'not_found', 'no such route');
}

/* ---- the stream: a WebSocket of RFC 6455's, the server's side ---- */

function frame(opcode, payload) {
  const data = Buffer.isBuffer(payload) ? payload : Buffer.from(payload);
  let head;
  if (data.length < 126) {
    head = Buffer.from([0x80 | opcode, data.length]);
  } else if (data.length < 65536) {
    head = Buffer.alloc(4);
    head[0] = 0x80 | opcode;
    head[1] = 126;
    head.writeUInt16BE(data.length, 2);
  } else {
    head = Buffer.alloc(10);
    head[0] = 0x80 | opcode;
    head[1] = 127;
    head.writeBigUInt64BE(BigInt(data.length), 2);
  }
  return Buffer.concat([head, data]);
}

/** The client's frames out of `buf`: [frames, bytes used]. */
function frames(buf) {
  const out = [];
  let at = 0;
  for (;;) {
    if (buf.length - at < 2) {
      break;
    }
    const b0 = buf[at];
    const b1 = buf[at + 1];
    let len = b1 & 0x7f;
    let p = at + 2;
    if (len === 126) {
      if (buf.length - at < 4) break;
      len = buf.readUInt16BE(at + 2);
      p = at + 4;
    } else if (len === 127) {
      if (buf.length - at < 10) break;
      len = Number(buf.readBigUInt64BE(at + 2));
      p = at + 10;
    }
    const masked = (b1 & 0x80) !== 0;
    if (buf.length < p + (masked ? 4 : 0) + len) {
      break;
    }
    const mask = masked ? buf.subarray(p, p + 4) : null;
    p += masked ? 4 : 0;
    const data = Buffer.from(buf.subarray(p, p + len));
    if (mask) {
      for (let i = 0; i < data.length; i++) data[i] ^= mask[i % 4];
    }
    out.push({ opcode: b0 & 0x0f, data });
    at = p + len;
  }
  return [out, at];
}

function stream(req, socket, who) {
  const url = new URL(req.url, 'https://nos');
  const key = req.headers['sec-websocket-key'];
  const accept = crypto.createHash('sha1').update(key + '258EAFA5-E914-47DA-95CA-C5AB0DC85B11').digest('base64');
  socket.write('HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n'
    + 'Sec-WebSocket-Accept: ' + accept + '\r\nServer: nos-mock\r\n\r\n');
  const channels = (url.searchParams.get('channels') || 'metrics,log,jobs').split(',');
  const has = (c) => channels.includes(c);
  const vmName = url.searchParams.get('vm');
  const message = (m) => socket.writable && socket.write(frame(1, JSON.stringify(m)));
  let since = Number(url.searchParams.get('log_since') ?? logSeq);
  let consoleFrom = Number(url.searchParams.get('console_from') || 0);
  let lastJobs = '';
  if (has('console') && !vms.has(vmName || '')) {
    message({ channel: 'console', error: 'vm= names no vm: a letter or a digit, then letters, digits, . _ and -' });
  }
  const timers = [];
  const every = (ms, fn) => {
    fn();
    timers.push(setInterval(fn, ms));
  };
  if (has('metrics')) {
    every(1000, () => message({ channel: 'metrics', uptime_ms: uptimeMs(), cpus: reads['system.cpus'](),
      memory: reads['system.memory'](), interfaces: reads['net.interfaces']() }));
  }
  if (has('log')) {
    every(500, () => {
      const data = reads['system.log']({ since });
      if (data.lines.length) {
        since = data.last_seq;
        message({ channel: 'log', data });
      }
    });
  }
  if (has('jobs')) {
    every(1000, () => {
      const data = JSON.stringify({ jobs: jobs.map(jobInfo) });
      if (data !== lastJobs) {
        lastJobs = data;
        message({ channel: 'jobs', data: JSON.parse(data) });
      }
    });
  }
  let counted = false;
  if (has('console') && vms.has(vmName)) {
    counted = true;
    consoleStreams.set(vmName, (consoleStreams.get(vmName) || 0) + 1);
    every(30, () => {
      try {
        const data = reads['vm.console_read']({ name: vmName, from: consoleFrom, max_bytes: 16384, terminal: true });
        if (data.next !== consoleFrom || data.from !== consoleFrom) {
          consoleFrom = data.next;
          message({ channel: 'console', vm: vmName, data });
        }
      } catch {
        /* Not on the list: nothing to show until it is. */
      }
    });
  }
  timers.push(setInterval(() => socket.writable && socket.write(frame(9, 'nos')), 5000));
  let buf = Buffer.alloc(0);
  socket.on('data', (d) => {
    buf = Buffer.concat([buf, d]);
    const [got, used] = frames(buf);
    buf = buf.subarray(used);
    for (const f of got) {
      if (f.opcode === 9) {
        socket.write(frame(10, f.data));
      } else if (f.opcode === 8) {
        socket.end(frame(8, Buffer.from([0x03, 0xe8])));
      } else if ((f.opcode === 1 || f.opcode === 2) && has('console') && vms.has(vmName)) {
        if (who.role !== 'admin') {
          message({ channel: 'console', vm: vmName, refused: 'forbidden', message: 'a reader types at no console' });
          continue;
        }
        try {
          actions['vm.console_write']({ name: vmName, text: f.data.toString('utf8') });
        } catch (e) {
          message({ channel: 'console', vm: vmName, refused: e.code || 'failed', message: e.message });
        }
      }
    }
  });
  const stop = () => {
    timers.forEach(clearInterval);
    if (counted) {
      counted = false;
      const n = (consoleStreams.get(vmName) || 1) - 1;
      if (n > 0) consoleStreams.set(vmName, n);
      else consoleStreams.delete(vmName);
    }
  };
  socket.on('close', stop);
  socket.on('error', stop);
}

/* ---- the server ---- */

fs.mkdirSync(STATE, { recursive: true });
const host = identity('host');
const admin = identity('admin');
const reader = identity('reader');
const hostPin = host.pin;
const adminPin = admin.pin;
const readerPin = reader.pin;
const clients = new Map([[adminPin, { name: 'panel', role: 'admin' }], [readerPin, { name: 'reader', role: 'read' }]]);
if (TRUST) {
  clients.set(fingerprint(fs.readFileSync(TRUST, 'utf8')), { name: 'trusted', role: 'admin' });
}
/** The host's id, as nos makes one at random once: here its key's, so a
 * state folder is the same host at every start, and another is another. */
const hostId = [hostPin.slice(0, 8), hostPin.slice(8, 12), '4' + hostPin.slice(13, 16), '8' + hostPin.slice(17, 20),
  hostPin.slice(20, 32)].join('-');
const all = descriptors();

/* ---- what nos lets in at once ---- */

/* api.ko's limits with no api.toml (modules/mgmt/api/src/config.rs): connections
 * in all, of one client's key, and in their handshake. One past them is
 * reset as nos resets it -- at the door, or right after its handshake when it
 * is its key's -- and counted as api.status counts it on nos, `busy`. */
const MAX_CONNECTIONS = 12;
const MAX_PER_KEY = 8;
const MAX_HANDSHAKES = 2;
const limits = { accepted: 0, busy: 0, open: new Set(), handshaking: 0 };
/** The streams open on a VM's console, by its name: what the panel's tests
 * hold to one however many tabs show it (`api.status`'s `consoles`, the
 * mock's own). */
const consoleStreams = new Map();

/** The SHA-256 of the key a TLS connection's certificate has, or null. */
function keyOf(socket) {
  const cert = socket.getPeerCertificate(false);
  if (!cert || !cert.raw) {
    return null;
  }
  const spki = new crypto.X509Certificate(cert.raw).publicKey.export({ type: 'spki', format: 'der' });
  return crypto.createHash('sha256').update(spki).digest('hex');
}

/** A TCP connection, at the door: taken while the limits allow it. */
function admit(raw) {
  if (limits.open.size >= MAX_CONNECTIONS || limits.handshaking >= MAX_HANDSHAKES) {
    limits.busy += 1;
    raw.resetAndDestroy();
    return;
  }
  limits.accepted += 1;
  limits.open.add(raw);
  limits.handshaking += 1;
  raw.nosHandshaking = true;
  raw.once('close', () => {
    limits.open.delete(raw);
    handshakeOver(raw);
  });
}

function handshakeOver(raw) {
  if (raw.nosHandshaking) {
    raw.nosHandshaking = false;
    limits.handshaking -= 1;
  }
}

/** Its handshake done: its key's fourth connection at most. */
function secured(tlsSocket) {
  const raw = tlsSocket._parent;
  if (!raw || !limits.open.has(raw)) {
    return;
  }
  handshakeOver(raw);
  const key = keyOf(tlsSocket);
  if (key && [...limits.open].filter((r) => r.nosKey === key).length >= MAX_PER_KEY) {
    limits.busy += 1;
    raw.resetAndDestroy();
    return;
  }
  raw.nosKey = key;
}

/** Who the connection's certificate says it is, by its key alone. */
function whoOf(socket) {
  const key = keyOf(socket);
  return key ? clients.get(key) || null : null;
}

const server = https.createServer({
  key: fs.readFileSync(host.key), cert: host.cert, requestCert: true, rejectUnauthorized: false,
  minVersion: 'TLSv1.3', ALPNProtocols: ['nos-api/1', 'http/1.1'],
}, (req, res) => {
  const who = whoOf(req.socket);
  if (!who) {
    /* What nos does in the handshake itself: nothing of HTTP for a stranger. */
    req.socket.destroy();
    return;
  }
  route(req, res, who).catch((e) => error(res, 500, 'failed', String(e)));
});
server.on('connection', admit);
server.on('secureConnection', secured);
server.on('upgrade', (req, socket) => {
  const who = whoOf(socket);
  if (!who || req.url.split('?')[0] !== '/v1/stream') {
    socket.destroy();
    return;
  }
  stream(req, socket, who);
});
server.listen(PORT, '127.0.0.1', () => {
  console.log('nos mock API on https://127.0.0.1:' + PORT + ', host ' + NAME + ' (' + hostId + ')');
  console.log('  host key   ' + hostPin);
  if (TRUST) {
    console.log('  trusted    ' + TRUST);
  }
  console.log('  admin      ' + admin.crt + ' ' + admin.key);
  console.log('  reader     ' + reader.crt + ' ' + reader.key);
  console.log('  providers  ' + Object.keys(all).join(', '));
});
