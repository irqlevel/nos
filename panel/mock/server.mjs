#!/usr/bin/env node
// mock/server.mjs: nos's host API (docs/api.md) as the panel sees it, for
// the panel's development and its tests -- the routes under /v1, TLS 1.3
// with the client's certificate required and let in by its key's SHA-256
// alone, the providers' own descriptors read from the tree, and a host whose
// numbers move: CPUs idling, interfaces counting, a log growing, VMs
// starting, stopping and printing on their consoles, jobs that take a few
// seconds and say so.
//
//     node mock/server.mjs [--port 39234] [--state mock/state] [--name nos]
//                          [--trust <admin.crt>]
//
// It makes, in its state folder, the host's key and certificate and an
// admin's and a reader's, and prints the host's key and where the clients'
// are -- what the panel is pointed at (panel/README.md). Its id is its key's,
// so each state folder is a host of its own; --name names it, and --trust
// lets another's key in as an admin -- the panel's, made for the first
// host, as `api client add` lets it in on a second (a cluster). No
// dependency but Node's own and openssl for the keys.

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
    name, id: null, state: 'off', protected: false, cpus: 1, memory_mib: 1024, net: true, port: null,
    network: null, kernel: '/lib/hvboot', console: '', bootMs: 0, restarts: 0, reason: null,
    forwards: [], disks: [{ file: 'disk.raw', size_bytes: 20 * 1024 * MIB, read_only: false },
      { file: 'seed.iso', size_bytes: 65536, read_only: true }],
    ...fields,
  });
}
vm('panel', { protected: true, port: 0, forwards: [{ port: 39233, guest_port: 443 }] });
vm('web', { cpus: 2, memory_mib: 2048, port: 3, forwards: [{ port: 2201, guest_port: 22 }] });
vm('db', { memory_mib: 4096, network: 'back' });

/** A VM started: on the list, its console printing a boot. */
function startVm(v) {
  v.id = nextVmId++;
  v.state = 'running';
  v.bootMs = uptimeMs();
  v.reason = null;
  v.console += '\r\n\x1b[1mhvboot\x1b[0m: booting /boot/vmlinuz-6.12 from /dev/vda1\r\n';
}
for (const name of ['panel', 'web']) {
  startVm(vms.get(name));
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
    memory_mib: v.memory_mib, net: v.net, port, ip: port === null ? null : '10.0.100.' + (port + 2),
    network: v.network, kernel: v.kernel,
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

const reads = {
  'system.info': () => ({
    version: '0.0.28-mock', revision: 'mock', arch: 'x86_64', cpu_model: 'AMD Ryzen 5 3600 6-Core Processor',
    cpus: CPUS, uptime_ms: uptimeMs(), time_s: Math.floor(Date.now() / 1000), log_level: 0,
    cmdline: 'root=UUID=0000 console=ttyS0',
  }),
  'system.cpus': () => ({ cpus: [...Array(CPUS).keys()].map((cpu) => ({ cpu, idle_ns: idleNs(cpu) })), uptime_ms: uptimeMs() }),
  'system.memory': () => ({ total_bytes: 8192 * MIB, free_bytes: Math.floor((4096 + 512 * Math.sin(uptimeMs() / 20000)) * MIB) }),
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
  'api.status': () => ({ serving: true, port: 39234, connections: [], key: hostPin }),
  'api.clients': () => ({ clients: [{ name: 'panel', role: 'admin', key: adminPin }, { name: 'reader', role: 'read', key: readerPin }] }),
  'vm.list': () => ({ vms: [...vms.values()].map(vmJson) }),
  'vm.get': (p) => {
    const v = vmNamed(p);
    return {
      vm: vmJson(v),
      config: { kernel: v.kernel, initrd: null, cmdline: null, forward: v.forwards, restart: true, console_log: true,
        uuid: '7c1e5f3a-0000-4000-8000-' + String(v.name).padEnd(12, '0').slice(0, 12).replace(/[^0-9a-f]/g, '0') },
      disks: v.disks.map((d) => ({ ...d, path: '/vms/' + v.name + '/' + d.file, vm: v.name })),
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
    const names = [...new Set(on.map((v) => v.network).filter(Boolean))];
    return { networks: names.map((n) => ({ name: n, vms: on.filter((v) => v.network === n).map((v) => v.name) })),
      alone: on.filter((v) => !v.network).map((v) => v.name) };
  },
  'vm.config_get': (p) => {
    const v = vmNamed(p);
    return { text: `# A VM of nos's hypervisor\nkernel = "${v.kernel}"\nmemory = ${v.memory_mib}\ncpus = ${v.cpus}\n`
      + `disks = ["disk.raw", "seed.iso:ro"]\nnet = true\n` + (v.protected ? 'protected = true\n' : ''), prev: false };
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
  'vm.disks': (p) => ({ disks: vmNamed(p).disks.map((d) => ({ ...d, path: '/vms/' + p.name + '/' + d.file, vm: p.name })) }),
  'vm.forwards': () => ({ forwards: [...vms.values()].filter((v) => v.state === 'running').flatMap((v) => v.forwards.map((f) => ({
    ...f, vm: v.name, id: v.id, guest_ip: '10.0.100.' + ((v.port ?? 0) + 2), active: 0, accepted: 3, refused: 0, failed: 0, stalled: 0 }))) }),
};

const actions = {
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

const guarded = (v, p) => {
  if (v.protected && !p.force) {
    throw new Refused('conflict', v.name + ' is protected -- force does it all the same', 'force');
  }
};

const jobMethods = {
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
    need(p, 'url');
    if (vms.has(name)) {
      throw new Refused('conflict', name + ' is there already', 'name');
    }
    return job('vm', 'create', 5, (i) => ['download: 120 MiB of 590 MiB, 20%', 'download: 590 MiB of 590 MiB, 100%',
      'qcow2 version 3 -- into /vms/' + name + '/disk.raw', 'disk.raw: 1785 MiB written of 3584 MiB, the rest holes',
      '/vms/' + name + '/vm.toml written'][i], () => {
      vm(name, { memory_mib: p.memory_mib ?? 1024, cpus: p.cpus ?? 1, network: p.network ?? null });
      return { name, file: '/vms/' + name + '/vm.toml', disk_bytes: p.size_bytes ?? 3584 * MIB, keys: 1 };
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
  if (has('console') && vms.has(vmName)) {
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
  const stop = () => timers.forEach(clearInterval);
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

/** Who the connection's certificate says it is, by its key alone. */
function whoOf(socket) {
  const cert = socket.getPeerCertificate(false);
  if (!cert || !cert.raw) {
    return null;
  }
  const spki = new crypto.X509Certificate(cert.raw).publicKey.export({ type: 'spki', format: 'der' });
  return clients.get(crypto.createHash('sha256').update(spki).digest('hex')) || null;
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
