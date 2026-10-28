// /api/console/<host>/<vm>: a VM's console, live, for the page's terminal.
//
//   GET   server-sent events: first {"type":"id"} -- what this console's
//         keys are posted with -- then the host's console pages as its
//         stream's console channel sends them (docs/api.md), each with
//         where it begins and ends, and {"type":"refused"} for what was
//         typed and did not go in;
//   POST  ?id=<id>, the keys typed, as text: sent on that same stream --
//         one connection of the host's a console, and its typing a line of
//         the host's audit log once.

import crypto from 'node:crypto';

import { host as hostOf, link } from '@/lib/hosts';
import { HostError, stream, Stream } from '@/lib/nosapi';
import { gate, json, name, refused } from '@/lib/route';

type Ctx = { params: Promise<{ host: string; vm: string }> };

const g = globalThis as unknown as { __nosConsoles?: Map<string, { stream: Stream; host: string; vm: string }> };
const consoles = g.__nosConsoles ?? (g.__nosConsoles = new Map());

/** What is typed at once, at most: what a VM's input holds. */
const KEYS_MAX = 4096;

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
  let s: Stream | null = null;
  const body = new ReadableStream<Uint8Array>({
    async start(ctrl) {
      const send = (e: unknown) => {
        try {
          ctrl.enqueue(enc.encode(`data: ${JSON.stringify(e)}\n\n`));
        } catch {
          /* the page went */
        }
      };
      send({ type: 'id', id });
      try {
        s = await stream(link(h), `?channels=console&vm=${vm}&console_from=${Number.isFinite(from) ? from : 0}`,
          (m) => {
            const msg = m as { channel?: string; data?: unknown; refused?: string; message?: string; error?: string };
            if (msg.channel !== 'console') return;
            if (msg.data) send({ type: 'page', ...(msg.data as object) });
            else if (msg.refused) send({ type: 'refused', code: msg.refused, message: msg.message });
            else if (msg.error) send({ type: 'refused', code: 'invalid', message: msg.error });
          },
          (why) => {
            send({ type: 'end', why });
            consoles.delete(id);
            try {
              ctrl.close();
            } catch {
              /* closed already */
            }
          });
        consoles.set(id, { stream: s, host, vm });
      } catch (e) {
        send({ type: 'end', why: e instanceof HostError ? e.message : String(e) });
        ctrl.close();
      }
      req.signal.addEventListener('abort', () => {
        consoles.delete(id);
        s?.close();
      });
    },
    cancel() {
      consoles.delete(id);
      s?.close();
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
  const c = consoles.get(id);
  if (!c || c.host !== host || c.vm !== vm) {
    return refused(404, 'not_found', 'no such console open');
  }
  const keys = await req.text();
  if (keys.length > KEYS_MAX) {
    return refused(413, 'too_large', 'more keys at once than a VM takes');
  }
  c.stream.send(keys);
  return json(200, { sent: keys.length });
}
