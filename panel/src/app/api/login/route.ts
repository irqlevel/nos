// POST /api/login {user, password}: a session, in the panel's cookie.

import { COOKIE, login, mayTry, sameOrigin } from '@/lib/auth';
import { json, refused } from '@/lib/route';

export async function POST(req: Request) {
  if (!sameOrigin(req.headers)) {
    return refused(403, 'forbidden', 'a login comes from the panel\'s own page');
  }
  /* Caddy adds the address the request came from last; what came before
   * it is the client's own to say, and no address to count by. */
  const ip = req.headers.get('x-forwarded-for')?.split(',').pop()?.trim() || 'local';
  if (!mayTry(ip)) {
    return refused(429, 'busy', 'too many tries: wait a few minutes');
  }
  let body: { user?: unknown; password?: unknown };
  try {
    body = await req.json();
  } catch {
    return refused(400, 'invalid', 'a login is JSON');
  }
  if (typeof body.user !== 'string' || typeof body.password !== 'string' || body.password.length > 256) {
    return refused(400, 'invalid', 'a user and a password');
  }
  const id = await login(body.user, body.password, ip, req.headers.get('user-agent') || '');
  if (!id) {
    return refused(401, 'unauthorized', 'wrong user or password');
  }
  return json(200, { ok: true }, {
    'Set-Cookie': `${COOKIE}=${id}; Path=/; Secure; HttpOnly; SameSite=Strict`,
  });
}
