// /api/enroll: a one-time enrollment made here, and the new host's callback.
// The callback needs no session: its 256-bit token is good once, for the host
// named when it was made, and expires after 15 minutes.

import { beginEnrollment, completeEnrollment } from '@/lib/enroll';
import { fingerprint } from '@/lib/hosts';
import { syncHubs } from '@/lib/hub';
import { gate, json, refused } from '@/lib/route';

export async function POST(req: Request) {
  const ok = await gate(req, true);
  if (ok instanceof Response) return ok;
  let b: { name?: unknown };
  try {
    b = await req.json();
  } catch {
    return refused(400, 'invalid', 'an enrollment is JSON: the host\'s name');
  }
  if (typeof b.name !== 'string') return refused(400, 'invalid', 'the host\'s name is required');
  try {
    return json(200, { ...beginEnrollment(b.name), panel_key: fingerprint() });
  } catch (e) {
    return refused(400, 'invalid', e instanceof Error ? e.message : String(e));
  }
}

export async function GET(req: Request) {
  const q = new URL(req.url).searchParams;
  const token = q.get('token') || '';
  const name = q.get('name') || '';
  const url = q.get('url') || '';
  const pin = q.get('pin') || '';
  try {
    const h = completeEnrollment(token, name, url, pin);
    syncHubs();
    return json(200, h);
  } catch (e) {
    return refused(403, 'refused', e instanceof Error ? e.message : String(e));
  }
}
