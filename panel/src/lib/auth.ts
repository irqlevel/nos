// Who may use the panel: `root`, with the password nos-install made and
// printed once -- its scrypt hash handed to the appliance in its seed
// (`seed.ts`) -- or the one it was changed to. A login makes a session: a
// random id in a cookie the browser sends only to this origin, over HTTPS,
// never to script (`__Host-`, Secure, HttpOnly, SameSite=Strict), and a
// session's every request that changes something must come from this origin
// (`sameOrigin`). Logins are counted per address, and a run of wrong ones
// waits.

import crypto from 'node:crypto';
import { promisify } from 'node:util';

import { store } from './store';

const scrypt = promisify(crypto.scrypt) as (pw: crypto.BinaryLike, salt: crypto.BinaryLike, len: number,
                                            opts: crypto.ScryptOptions) => Promise<Buffer>;

export const USER = 'root';
export const COOKIE = '__Host-nos-panel';
/** A session lives this long unused, and this long at most. */
const IDLE_MS = 12 * 60 * 60 * 1000;
const LIFE_MS = 7 * 24 * 60 * 60 * 1000;
/** Wrong passwords an address may give in a window before it waits. */
const TRIES = 8;
const WINDOW_MS = 10 * 60 * 1000;

/** scrypt's parameters, as nos-install (Python's hashlib.scrypt) uses them:
 * `scrypt$<N>$<r>$<p>$<salt, base64>$<hash, base64>`. */
const N = 1 << 15;
const R = 8;
const P = 1;
const LEN = 32;

export async function hashPassword(password: string): Promise<string> {
  const salt = crypto.randomBytes(16);
  const hash = await scrypt(password, salt, LEN, { N, r: R, p: P, maxmem: 128 * N * R * 2 });
  return ['scrypt', N, R, P, salt.toString('base64'), hash.toString('base64')].join('$');
}

/** Whether `password` is the one `stored` is the hash of. */
export async function checkPassword(password: string, stored: string): Promise<boolean> {
  const parts = stored.split('$');
  if (parts.length !== 6 || parts[0] !== 'scrypt') {
    return false;
  }
  const [n, r, p] = parts.slice(1, 4).map(Number);
  if (!n || !r || !p || n > 1 << 20 || r > 32 || p > 16) {
    return false;
  }
  const salt = Buffer.from(parts[4] ?? '', 'base64');
  const want = Buffer.from(parts[5] ?? '', 'base64');
  const got = await scrypt(password, salt, want.length, { N: n, r, p, maxmem: 128 * n * r * 2 });
  return got.length === want.length && crypto.timingSafeEqual(got, want);
}

const tries = new Map<string, { n: number; since: number }>();

/** Whether `ip` may try a password now. */
export function mayTry(ip: string): boolean {
  const t = tries.get(ip);
  return !t || Date.now() - t.since > WINDOW_MS || t.n < TRIES;
}

function tried(ip: string, ok: boolean) {
  if (ok) {
    tries.delete(ip);
    return;
  }
  const t = tries.get(ip);
  if (!t || Date.now() - t.since > WINDOW_MS) {
    tries.set(ip, { n: 1, since: Date.now() });
  } else {
    t.n += 1;
  }
}

/** A login: the session's id, or null for a wrong user or password. */
export async function login(user: string, password: string, ip: string, agent: string): Promise<string | null> {
  const row = store().prepare('SELECT hash FROM users WHERE name = ?').get(user) as { hash: string } | undefined;
  const ok = row !== undefined && await checkPassword(password, row.hash);
  tried(ip, ok);
  if (!ok) {
    return null;
  }
  const id = crypto.randomBytes(32).toString('base64url');
  const now = Date.now();
  store().prepare('INSERT INTO sessions (id, user, created_ms, seen_ms, ip, agent) VALUES (?, ?, ?, ?, ?, ?)')
    .run(id, user, now, now, ip, agent.slice(0, 200));
  return id;
}

/** The user a session is, if it is a live one -- its use noted. */
export function session(id: string | undefined): string | null {
  if (!id || id.length > 64) {
    return null;
  }
  const s = store().prepare('SELECT user, created_ms, seen_ms FROM sessions WHERE id = ?').get(id) as
    { user: string; created_ms: number; seen_ms: number } | undefined;
  const now = Date.now();
  if (!s || now - s.seen_ms > IDLE_MS || now - s.created_ms > LIFE_MS) {
    if (s) {
      store().prepare('DELETE FROM sessions WHERE id = ?').run(id);
    }
    return null;
  }
  if (now - s.seen_ms > 60_000) {
    store().prepare('UPDATE sessions SET seen_ms = ? WHERE id = ?').run(now, id);
  }
  return s.user;
}

export function logout(id: string | undefined) {
  if (id) {
    store().prepare('DELETE FROM sessions WHERE id = ?').run(id);
  }
}

/** The password changed: every session but `keep` ended with it. */
export async function changePassword(user: string, password: string, keep: string) {
  const hash = await hashPassword(password);
  const d = store();
  d.prepare('UPDATE users SET hash = ?, changed_ms = ? WHERE name = ?').run(hash, Date.now(), user);
  d.prepare('DELETE FROM sessions WHERE user = ? AND id != ?').run(user, keep);
}

/** Whether a request that changes something comes from this origin: its
 * Origin -- which a browser always sends with one -- names the host it was
 * sent to. */
export function sameOrigin(headers: Headers): boolean {
  const origin = headers.get('origin');
  const host = headers.get('x-forwarded-host') || headers.get('host');
  if (!origin || !host) {
    return false;
  }
  try {
    return new URL(origin).host === host;
  } catch {
    return false;
  }
}
