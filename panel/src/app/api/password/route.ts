// POST /api/password {old, new}: root's password changed, the old one asked
// again, every other session ended.

import { cookies } from 'next/headers';

import { changePassword, checkPassword, COOKIE, USER } from '@/lib/auth';
import { gate, json, refused } from '@/lib/route';
import { store } from '@/lib/store';

/** The shortest password the panel takes. */
const MIN = 12;

export async function POST(req: Request) {
  const ok = await gate(req, true);
  if (ok instanceof Response) return ok;
  let b: { old?: unknown; new?: unknown };
  try {
    b = await req.json();
  } catch {
    return refused(400, 'invalid', 'the passwords are JSON');
  }
  if (typeof b.old !== 'string' || typeof b.new !== 'string' || b.new.length < MIN || b.new.length > 256) {
    return refused(400, 'invalid', `the new password is ${MIN} to 256 characters`);
  }
  const row = store().prepare('SELECT hash FROM users WHERE name = ?').get(USER) as { hash: string } | undefined;
  if (!row || !(await checkPassword(b.old, row.hash))) {
    return refused(403, 'forbidden', 'the present password is wrong');
  }
  const jar = await cookies();
  await changePassword(USER, b.new, jar.get(COOKIE)?.value || '');
  return json(200, { ok: true });
}
