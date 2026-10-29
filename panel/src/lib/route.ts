// What every route of the panel's own API (`/api/...`) asks first: a live
// session, and for what changes something, a request from this origin --
// then the host the path names. The browser reaches a host only through
// here: the panel's server holds the host's identity, the browser never does.

import { cookies } from 'next/headers';

import { api, HostError } from './nosapi';
import { COOKIE, sameOrigin, session } from './auth';
import { host as hostOf, link } from './hosts';

export function json(status: number, body: unknown, headers: Record<string, string> = {}): Response {
  return new Response(JSON.stringify(body), {
    status, headers: { 'Content-Type': 'application/json', 'Cache-Control': 'no-store', ...headers },
  });
}

export function refused(status: number, code: string, message: string): Response {
  return json(status, { error: { code, message } });
}

/** The session's user, or the refusal to answer with: no session, or a
 * change asked from another origin. */
export async function gate(req: Request, changes: boolean): Promise<{ user: string } | Response> {
  const jar = await cookies();
  const user = session(jar.get(COOKIE)?.value);
  if (!user) {
    return refused(401, 'unauthorized', 'log in first');
  }
  if (changes && !sameOrigin(req.headers)) {
    return refused(403, 'forbidden', 'a change is asked from the panel\'s own pages alone');
  }
  return { user };
}

/** A host's API asked, its answer handed back as it is -- or why the host
 * could not be reached, as a 502. */
export async function relay(hostId: string, method: string, path: string, body?: unknown): Promise<Response> {
  const h = hostOf(hostId);
  if (!h) {
    return refused(404, 'not_found', 'no such host');
  }
  try {
    const a = await api(link(h), method, path, body);
    return json(a.status, a.json ?? {}, a.headers['etag'] ? { ETag: a.headers['etag'] } : {});
  } catch (e) {
    const why = e instanceof HostError ? e.message : String(e);
    return refused(502, e instanceof HostError && e.kind === 'pin' ? 'pin' : 'unreachable', why);
  }
}

/** A name a path segment may be: a provider's, a method's, a VM's. */
export function name(s: string, max = 32): boolean {
  return s.length > 0 && s.length <= max && /^[A-Za-z0-9][A-Za-z0-9._-]*$/.test(s);
}
