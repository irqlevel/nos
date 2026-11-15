// /api/console/<host>/<vm>: a VM's console, live, for the page's terminal.
//
//   GET   server-sent events: first {"type":"id"} -- what this page's keys
//         are posted with -- then the console's pages as the host's stream's
//         console channel sends them (docs/api.md), each with where it begins
//         and ends, and {"type":"refused"} for what was typed and did not go
//         in; {"type":"end"} once the stream is over;
//   POST  ?id=<id>, the keys typed, as text: sent on that same stream, its
//         typing a line of the host's audit log once.
//
// One stream of the host's a VM's console, however many tabs show it: each
// is a connection of the panel's key, which api.ko holds to eight
// (`max_per_key`), the panel's own stream and its requests' among them --
// a stream a tab, a second tab or a reload past them was reset, and the
// terminal said `read ECONNRESET` (2026-11-15). A tab that comes to one
// open already is given what the panel has kept of it -- the last 64 KiB,
// as much as the host's ring keeps -- and then what it prints, as the tabs
// before it are; the last tab gone, the stream is kept a few seconds more,
// for a reload to take up again, and then closed.

import crypto from 'node:crypto';

import { host as hostOf, link } from '@/lib/hosts';
import { HostError, stream, Stream } from '@/lib/nosapi';
import { gate, json, name, refused } from '@/lib/route';

type Ctx = { params: Promise<{ host: string; vm: string }> };

/** A page of the console, as the host sends it (`vm.console_read`). */
type Page = { from: number; next: number; total?: number; text: string };

/** A tab showing a console: what is sent to it, and its end. */
type Tab = { send: (e: unknown) => void; close: () => void };

/** A VM's console as the panel holds it open: its stream of the host's, the
 * tabs it feeds, what it printed lately for a tab coming to it, and what was
 * typed before the stream was open. */
type Shared = {
  key: string;
  host: string;
  vm: string;
  stream: Stream | null;
  tabs: Map<string, Tab>;
  pages: Page[];
  bytes: number;
  typed: string;
  linger: ReturnType<typeof setTimeout> | null;
};

/* One of each a process, as nosapi.ts keeps its pools: Next.js may load this
 * module once for each of its bundles. */
const g = globalThis as unknown as { __nosConsoles?: Map<string, Shared>; __nosConsoleTabs?: Map<string, string> };
/** The consoles open, by host and VM. */
const consoles = g.__nosConsoles ?? (g.__nosConsoles = new Map());
/** A tab's id: the console it shows. */
const tabs = g.__nosConsoleTabs ?? (g.__nosConsoleTabs = new Map());

/** What is typed at once, at most: what a VM's input holds. */
const KEYS_MAX = 4096;
/** What is kept of a console for a tab coming to it: the host's ring's. */
const KEPT_BYTES = 65536;
/** How long a console no tab shows is kept open: a reload takes it up. */
const LINGER_MS = 5000;

function keyOf(host: string, vm: string): string {
  return `${host}\u0000${vm}`;
}

/** The console of `vm` on host `h` opened, its stream's messages fed to its
 * tabs; once over -- closed, refused, broken -- each tab told why. */
async function open(c: Shared, h: NonNullable<ReturnType<typeof hostOf>>) {
  try {
    const s = await stream(link(h), `?channels=console&vm=${c.vm}&console_from=0`, (m) => message(c, m),
      (why) => ended(c, why));
    /* Every tab gone meanwhile, and its time out: not wanted any more */
    if (consoles.get(c.key) !== c) {
      s.close();
      return;
    }
    c.stream = s;
    if (c.typed) {
      s.send(c.typed);
      c.typed = '';
    }
  } catch (e) {
    ended(c, e instanceof HostError ? e.message : String(e));
  }
}

function broadcast(c: Shared, e: unknown) {
  for (const tab of c.tabs.values()) tab.send(e);
}

function message(c: Shared, m: unknown) {
  const msg = m as { channel?: string; data?: unknown; refused?: string; message?: string; error?: string };
  if (msg.channel !== 'console') return;
  if (msg.data) {
    const page = msg.data as Page;
    c.pages.push(page);
    c.bytes += page.text.length;
    while (c.bytes > KEPT_BYTES && c.pages.length > 1) {
      c.bytes -= c.pages.shift()!.text.length;
    }
    broadcast(c, { type: 'page', ...page });
  } else if (msg.refused) {
    broadcast(c, { type: 'refused', code: msg.refused, message: msg.message });
  } else if (msg.error) {
    broadcast(c, { type: 'refused', code: 'invalid', message: msg.error });
  }
}

/** The console's stream over: every tab told why and ended, and the console
 * forgotten -- a tab after opens it anew. */
function ended(c: Shared, why: string) {
  if (consoles.get(c.key) === c) {
    consoles.delete(c.key);
  }
  if (c.linger) {
    clearTimeout(c.linger);
    c.linger = null;
  }
  for (const [id, tab] of c.tabs) {
    tab.send({ type: 'end', why });
    tab.close();
    tabs.delete(id);
  }
  c.tabs.clear();
}

/** Tab `id` gone: the console closed a while after its last tab. */
function leave(key: string, id: string) {
  tabs.delete(id);
  const c = consoles.get(key);
  if (!c || !c.tabs.delete(id) || c.tabs.size !== 0 || c.linger) {
    return;
  }
  c.linger = setTimeout(() => {
    c.linger = null;
    if (c.tabs.size === 0 && consoles.get(c.key) === c) {
      consoles.delete(c.key);
      c.stream?.close();
    }
  }, LINGER_MS);
}

export async function GET(req: Request, ctx: Ctx) {
  const ok = await gate(req, false);
  if (ok instanceof Response) return ok;
  const { host, vm } = await ctx.params;
  const h = hostOf(host);
  if (!h || !name(vm)) {
    return refused(404, 'not_found', 'no such host or vm');
  }
  const from = Number(new URL(req.url).searchParams.get('from') || 0);
  const enc = new TextEncoder();
  const id = crypto.randomBytes(16).toString('base64url');
  const key = keyOf(host, vm);
  const body = new ReadableStream<Uint8Array>({
    start(ctrl) {
      const tab: Tab = {
        send: (e: unknown) => {
          try {
            ctrl.enqueue(enc.encode(`data: ${JSON.stringify(e)}\n\n`));
          } catch {
            /* the page went */
          }
        },
        close: () => {
          try {
            ctrl.close();
          } catch {
            /* closed already */
          }
        },
      };
      tab.send({ type: 'id', id });
      let c = consoles.get(key);
      const opening = !c;
      if (!c) {
        c = { key, host, vm, stream: null, tabs: new Map(), pages: [], bytes: 0, typed: '', linger: null };
        consoles.set(key, c);
      }
      if (c.linger) {
        clearTimeout(c.linger);
        c.linger = null;
      }
      /* What it printed lately first, then what it prints, as every tab */
      for (const page of c.pages) {
        if (page.next > (Number.isFinite(from) ? from : 0)) tab.send({ type: 'page', ...page });
      }
      c.tabs.set(id, tab);
      tabs.set(id, key);
      req.signal.addEventListener('abort', () => leave(key, id));
      if (opening) void open(c, h);
    },
    cancel() {
      leave(key, id);
    },
  });
  return new Response(body, {
    headers: { 'Content-Type': 'text/event-stream', 'Cache-Control': 'no-store', 'X-Accel-Buffering': 'no' },
  });
}

export async function POST(req: Request, ctx: Ctx) {
  const ok = await gate(req, true);
  if (ok instanceof Response) return ok;
  const { host, vm } = await ctx.params;
  const id = new URL(req.url).searchParams.get('id') || '';
  const key = tabs.get(id);
  const c = key ? consoles.get(key) : undefined;
  if (!c || c.host !== host || c.vm !== vm || !c.tabs.has(id)) {
    return refused(404, 'not_found', 'no such console open');
  }
  const keys = await req.text();
  if (keys.length > KEYS_MAX) {
    return refused(413, 'too_large', 'more keys at once than a VM takes');
  }
  if (c.stream) {
    c.stream.send(keys);
  } else if (c.typed.length + keys.length <= KEYS_MAX) {
    /* Typed before the stream is open: sent as it opens */
    c.typed += keys;
  } else {
    return refused(413, 'too_large', 'more keys at once than a VM takes');
  }
  return json(200, { sent: keys.length });
}
