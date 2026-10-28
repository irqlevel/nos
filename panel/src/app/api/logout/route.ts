// POST /api/logout: the session ended, its cookie cleared.

import { cookies } from 'next/headers';

import { COOKIE, logout, sameOrigin } from '@/lib/auth';
import { json, refused } from '@/lib/route';

export async function POST(req: Request) {
  if (!sameOrigin(req.headers)) {
    return refused(403, 'forbidden', 'a logout comes from the panel\'s own page');
  }
  const jar = await cookies();
  logout(jar.get(COOKIE)?.value);
  return json(200, { ok: true }, { 'Set-Cookie': `${COOKIE}=; Path=/; Secure; HttpOnly; SameSite=Strict; Max-Age=0` });
}
