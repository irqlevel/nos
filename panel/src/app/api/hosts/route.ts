// /api/hosts: the hosts the panel manages.
//
//   GET     each, and whether the panel reaches it now
//   POST    {name, url, pin}: one added -- its key pinned as given
//   DELETE  ?id=<id>: one taken away, its history with it

import { addHost, fingerprint, hosts, removeHost } from '@/lib/hosts';
import { hub, syncHubs } from '@/lib/hub';
import { gate, json, refused } from '@/lib/route';

export async function GET(req: Request) {
  const ok = await gate(req, false);
  if (ok instanceof Response) return ok;
  syncHubs();
  return json(200, {
    panel_key: fingerprint(),
    hosts: hosts().map((h) => {
      const hb = hub(h.id);
      return { ...h, online: hb?.online ?? false, why: hb?.why || null };
    }),
  });
}

export async function POST(req: Request) {
  const ok = await gate(req, true);
  if (ok instanceof Response) return ok;
  let b: { name?: unknown; url?: unknown; pin?: unknown };
  try {
    b = await req.json();
  } catch {
    return refused(400, 'invalid', 'a host is JSON: its name, address and key');
  }
  if (typeof b.name !== 'string' || typeof b.url !== 'string' || typeof b.pin !== 'string') {
    return refused(400, 'invalid', 'a host is its name, address and key');
  }
  try {
    const h = addHost(b.name, b.url, b.pin);
    syncHubs();
    return json(200, h);
  } catch (e) {
    return refused(400, 'invalid', e instanceof Error ? e.message : String(e));
  }
}

export async function DELETE(req: Request) {
  const ok = await gate(req, true);
  if (ok instanceof Response) return ok;
  const id = new URL(req.url).searchParams.get('id') || '';
  removeHost(id);
  syncHubs();
  return json(200, { removed: id });
}
