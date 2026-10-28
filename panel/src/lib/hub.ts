// One stream a host, kept open by the panel's server for as long as it runs
// (`instrumentation.ts` starts it): the host's metrics, log and jobs as they
// come (docs/api.md's /v1/stream), handed to every browser watching
// (`/api/h/<host>/events`, server-sent events), and the numbers kept as
// history -- what a CPU was busy, the memory used, what the interfaces moved
// -- a second at a time for an hour and a minute at a time for thirty days.
// A host the stream cannot reach is offline, said so, and tried again a
// little later each time.

import { HostError, stream, Stream } from './nosapi';
import { Host, hosts, link } from './hosts';
import { store } from './store';

/** What the hub says of a host, and hands its watchers. */
export type HubEvent =
  | { type: 'state'; online: boolean; why?: string }
  | { type: 'metrics'; ms: number; cpu_busy_permille: number | null; mem_used_bytes: number | null;
      mem_total_bytes: number | null; rx_bytes_per_s: number | null; tx_bytes_per_s: number | null; cpus: number[] }
  | { type: 'log'; lines: Array<{ seq: number; level: number; text: string; ms: number }> }
  | { type: 'jobs'; jobs: unknown[] };

type Watcher = (e: HubEvent) => void;

const FINE_KEEP_MS = 60 * 60 * 1000;
const COARSE_KEEP_MS = 30 * 24 * 60 * 60 * 1000;
const RETRY_MIN_MS = 1000;
/** The log's lines a hub keeps for a page that opens. */
const LOG_KEEP = 100;
const RETRY_MAX_MS = 30_000;

interface Prev {
  ms: number;
  idle: number[];
  rx: number;
  tx: number;
}

class HostHub {
  online = false;
  why = 'connecting';
  last: HubEvent | null = null;
  lastJobs: unknown[] = [];
  /** The log's last lines, for a page that opens now. */
  lastLog: Array<{ seq: number; level: number; text: string; ms: number }> = [];
  watchers = new Set<Watcher>();
  private s: Stream | null = null;
  private prev: Prev | null = null;
  private retry = RETRY_MIN_MS;
  private minute: { n: number; cpu: number; mem: number; rx: number; tx: number; ms: number } | null = null;
  private stopped = false;

  constructor(readonly host: Host) {
    void this.open();
  }

  private emit(e: HubEvent) {
    for (const w of this.watchers) {
      try {
        w(e);
      } catch {
        this.watchers.delete(w);
      }
    }
  }

  private async open() {
    if (this.stopped) return;
    try {
      /* The log from where the hub last read it -- or, the first time, from
       * as far back as the host keeps it: the hub keeps the last lines. */
      const since = this.lastLog.length ? this.lastLog[this.lastLog.length - 1]!.seq : 0;
      this.s = await stream(link(this.host), `?channels=metrics,log,jobs&log_since=${since}`, (m) => this.message(m),
        (why) => this.lost(why));
      this.online = true;
      this.why = '';
      this.retry = RETRY_MIN_MS;
      this.emit({ type: 'state', online: true });
    } catch (e) {
      this.lost(e instanceof HostError ? e.message : String(e));
    }
  }

  private lost(why: string) {
    this.s = null;
    this.prev = null;
    if (this.online || this.why !== why) {
      this.online = false;
      this.why = why;
      this.emit({ type: 'state', online: false, why });
    }
    if (!this.stopped) {
      setTimeout(() => void this.open(), this.retry).unref();
      this.retry = Math.min(this.retry * 2, RETRY_MAX_MS);
    }
  }

  private message(m: unknown) {
    const msg = m as { channel?: string; [k: string]: unknown };
    if (msg.channel === 'metrics') {
      this.metrics(msg);
    } else if (msg.channel === 'log') {
      const data = msg.data as { lines?: Array<{ seq: number; level: number; text: string; ms: number }> };
      if (data?.lines?.length) {
        this.lastLog = [...this.lastLog, ...data.lines].slice(-LOG_KEEP);
        this.emit({ type: 'log', lines: data.lines });
      }
    } else if (msg.channel === 'jobs') {
      const data = msg.data as { jobs?: unknown[] };
      this.lastJobs = data?.jobs ?? [];
      this.emit({ type: 'jobs', jobs: this.lastJobs });
    }
  }

  /** A metrics message made rates, against the one before it. */
  private metrics(msg: Record<string, unknown>) {
    const ms = Number(msg.uptime_ms) || Date.now();
    const cpus = ((msg.cpus as { cpus?: Array<{ idle_ns: number }> })?.cpus ?? []).map((c) => Number(c.idle_ns));
    const mem = msg.memory as { total_bytes?: number; free_bytes?: number } | null;
    const ifs = (msg.interfaces as { interfaces?: Array<{ name: string; rx_bytes: number; tx_bytes: number }> })?.interfaces ?? [];
    /* The host's own traffic: what crosses its NICs, not the guests' switch. */
    const own = ifs.filter((i) => i.name !== 'hv0');
    const rx = own.reduce((a, i) => a + Number(i.rx_bytes || 0), 0);
    const tx = own.reduce((a, i) => a + Number(i.tx_bytes || 0), 0);
    const prev = this.prev;
    this.prev = { ms, idle: cpus, rx, tx };
    if (!prev || ms <= prev.ms || prev.idle.length !== cpus.length) {
      return;
    }
    const dt = ms - prev.ms;
    const perCpu = cpus.map((idle, i) => {
      const idleMs = (idle - (prev.idle[i] ?? idle)) / 1e6;
      return Math.max(0, Math.min(1000, Math.round(1000 - (idleMs / dt) * 1000)));
    });
    const busy = perCpu.length ? Math.round(perCpu.reduce((a, b) => a + b, 0) / perCpu.length) : null;
    const used = mem?.total_bytes != null && mem.free_bytes != null ? mem.total_bytes - mem.free_bytes : null;
    const rxs = rx >= prev.rx ? Math.round(((rx - prev.rx) * 1000) / dt) : null;
    const txs = tx >= prev.tx ? Math.round(((tx - prev.tx) * 1000) / dt) : null;
    const e: HubEvent = { type: 'metrics', ms: Date.now(), cpu_busy_permille: busy, mem_used_bytes: used,
      mem_total_bytes: mem?.total_bytes ?? null, rx_bytes_per_s: rxs, tx_bytes_per_s: txs, cpus: perCpu };
    this.last = e;
    this.emit(e);
    this.keep(e);
  }

  /** A sample kept: every second, and a minute's average every minute. */
  private keep(e: Extract<HubEvent, { type: 'metrics' }>) {
    const d = store();
    d.prepare('INSERT OR REPLACE INTO samples (host, ms, fine, cpu_busy_permille, mem_used_bytes, rx_bytes_per_s, '
      + 'tx_bytes_per_s) VALUES (?, ?, 1, ?, ?, ?, ?)')
      .run(this.host.id, e.ms, e.cpu_busy_permille, e.mem_used_bytes, e.rx_bytes_per_s, e.tx_bytes_per_s);
    const minute = Math.floor(e.ms / 60_000) * 60_000;
    const m = this.minute;
    if (m && m.ms !== minute && m.n) {
      d.prepare('INSERT OR REPLACE INTO samples (host, ms, fine, cpu_busy_permille, mem_used_bytes, rx_bytes_per_s, '
        + 'tx_bytes_per_s) VALUES (?, ?, 0, ?, ?, ?, ?)')
        .run(this.host.id, m.ms, Math.round(m.cpu / m.n), Math.round(m.mem / m.n), Math.round(m.rx / m.n),
             Math.round(m.tx / m.n));
      d.prepare('DELETE FROM samples WHERE host = ? AND ((fine = 1 AND ms < ?) OR (fine = 0 AND ms < ?))')
        .run(this.host.id, e.ms - FINE_KEEP_MS, e.ms - COARSE_KEEP_MS);
    }
    if (!m || m.ms !== minute) {
      this.minute = { n: 0, cpu: 0, mem: 0, rx: 0, tx: 0, ms: minute };
    }
    const now = this.minute!;
    now.n += 1;
    now.cpu += e.cpu_busy_permille ?? 0;
    now.mem += e.mem_used_bytes ?? 0;
    now.rx += e.rx_bytes_per_s ?? 0;
    now.tx += e.tx_bytes_per_s ?? 0;
  }

  stop() {
    this.stopped = true;
    this.s?.close();
  }
}

const g = globalThis as unknown as { __nosHubs?: Map<string, HostHub> };
const hubs: Map<string, HostHub> = g.__nosHubs ?? (g.__nosHubs = new Map());

/** Every host's hub running -- one each for the hosts there are, and none
 * for one gone. */
export function syncHubs() {
  const want = new Map(hosts().map((h) => [h.id, h]));
  for (const [id, hub] of hubs) {
    const h = want.get(id);
    if (!h || h.url !== hub.host.url || h.pin !== hub.host.pin) {
      hub.stop();
      hubs.delete(id);
    }
  }
  for (const [id, h] of want) {
    if (!hubs.has(id)) {
      hubs.set(id, new HostHub(h));
    }
  }
}

export function hub(id: string): HostHub | undefined {
  syncHubs();
  return hubs.get(id);
}

/** A host's history, from `since` on: the fine samples for an hour or
 * less, the minutes for more. */
export function history(id: string, sinceMs: number) {
  /* An hour asked as "an hour ago" is a few milliseconds more by now */
  const fine = Date.now() - sinceMs <= FINE_KEEP_MS + 60_000 ? 1 : 0;
  return store().prepare('SELECT ms, cpu_busy_permille, mem_used_bytes, rx_bytes_per_s, tx_bytes_per_s FROM samples '
    + 'WHERE host = ? AND fine = ? AND ms >= ? ORDER BY ms').all(id, fine, sinceMs);
}
