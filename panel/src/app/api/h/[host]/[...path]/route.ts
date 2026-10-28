// /api/h/<host>/...: a host's API, as the panel's pages reach it -- the
// routes of docs/api.md, each a session's, a change from this origin alone:
//
//   GET  providers                  the host's providers and methods
//   GET  describe/<provider>        one's descriptor
//   POST call/<provider>/<method>   a read or an action
//   POST jobs/<provider>/<method>   a job started
//   GET  jobs, jobs/<id>[?notes_since=N]
//   POST jobs/<id>/cancel
//   GET  history?since_ms=N         the host's numbers, as the panel kept them

import { history } from '@/lib/hub';
import { gate, json, name, refused, relay } from '@/lib/route';

type Ctx = { params: Promise<{ host: string; path: string[] }> };

export async function GET(req: Request, ctx: Ctx) {
  const ok = await gate(req, false);
  if (ok instanceof Response) return ok;
  const { host, path } = await ctx.params;
  const url = new URL(req.url);
  const [what, a, b] = path;
  if (what === 'providers' && !a) {
    return relay(host, 'GET', '/v1/providers');
  }
  if (what === 'describe' && a && name(a) && !b) {
    return relay(host, 'GET', '/v1/providers/' + a);
  }
  if (what === 'jobs' && !a) {
    return relay(host, 'GET', '/v1/jobs');
  }
  if (what === 'jobs' && a && /^\d{1,12}$/.test(a) && !b) {
    const since = Number(url.searchParams.get('notes_since') || 0);
    return relay(host, 'GET', `/v1/jobs/${a}?notes_since=${Number.isFinite(since) ? since : 0}`);
  }
  if (what === 'history' && !a) {
    const since = Number(url.searchParams.get('since_ms') || Date.now() - 3600_000);
    return json(200, { samples: history(host, Number.isFinite(since) ? since : Date.now() - 3600_000) });
  }
  return refused(404, 'not_found', 'no such route');
}

export async function POST(req: Request, ctx: Ctx) {
  const ok = await gate(req, true);
  if (ok instanceof Response) return ok;
  const { host, path } = await ctx.params;
  const [what, a, b, c] = path;
  let body: unknown = undefined;
  const text = await req.text();
  if (text.length > 64 * 1024) {
    return refused(413, 'too_large', 'the parameters are larger than a host takes');
  }
  if (text) {
    try {
      body = JSON.parse(text);
    } catch {
      return refused(400, 'invalid', 'the parameters are JSON');
    }
  }
  if ((what === 'call' || what === 'jobs') && a && b && name(a) && name(b) && !c && !/^\d+$/.test(a)) {
    return relay(host, 'POST', `/v1/${what}/${a}/${b}`, body ?? {});
  }
  if (what === 'jobs' && a && /^\d{1,12}$/.test(a) && b === 'cancel' && !c) {
    return relay(host, 'POST', `/v1/jobs/${a}/cancel`);
  }
  return refused(404, 'not_found', 'no such route');
}
