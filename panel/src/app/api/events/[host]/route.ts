// GET /api/events/<host>: what the hub hears of a host, as server-sent
// events -- its state, then its metrics, log and jobs as they come. A
// browser's page watches one host at a time; the hub's one stream feeds
// every page.

import { hub, HubEvent } from '@/lib/hub';
import { gate, refused } from '@/lib/route';

type Ctx = { params: Promise<{ host: string }> };

/** How often a quiet event stream says it is alive: what keeps a proxy from
 * taking it for a dead one. */
const KEEPALIVE_MS = 15_000;

export async function GET(req: Request, ctx: Ctx) {
  const ok = await gate(req, false);
  if (ok instanceof Response) return ok;
  const { host } = await ctx.params;
  const h = hub(host);
  if (!h) {
    return refused(404, 'not_found', 'no such host');
  }
  const enc = new TextEncoder();
  let watcher: ((e: HubEvent) => void) | null = null;
  let timer: ReturnType<typeof setInterval> | null = null;
  const body = new ReadableStream<Uint8Array>({
    start(ctrl) {
      const send = (e: HubEvent) => ctrl.enqueue(enc.encode(`data: ${JSON.stringify(e)}\n\n`));
      send({ type: 'state', online: h.online, why: h.why || undefined });
      if (h.last) send(h.last);
      if (h.lastLog.length) send({ type: 'log', lines: h.lastLog });
      send({ type: 'jobs', jobs: h.lastJobs });
      watcher = send;
      h.watchers.add(send);
      timer = setInterval(() => ctrl.enqueue(enc.encode(': alive\n\n')), KEEPALIVE_MS);
      req.signal.addEventListener('abort', () => {
        if (watcher) h.watchers.delete(watcher);
        if (timer) clearInterval(timer);
        try {
          ctrl.close();
        } catch {
          /* closed already */
        }
      });
    },
    cancel() {
      if (watcher) h.watchers.delete(watcher);
      if (timer) clearInterval(timer);
    },
  });
  return new Response(body, {
    headers: { 'Content-Type': 'text/event-stream', 'Cache-Control': 'no-store', 'X-Accel-Buffering': 'no' },
  });
}
