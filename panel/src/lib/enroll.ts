import crypto from 'node:crypto';

import { addHost, Host, hosts } from './hosts';
import { store } from './store';

const TTL_MS = 15 * 60 * 1000;
const TOKEN = /^[0-9a-f]{64}$/;
const NAME = /^[a-z0-9-]{1,63}$/;

interface Pending { name: string; expires_ms: number }

function key(token: string): string {
  return 'enrollment:' + token;
}

function pending(token: string): Pending | null {
  if (!TOKEN.test(token)) return null;
  const row = store().prepare('SELECT value FROM settings WHERE key = ?').get(key(token)) as { value: string } | undefined;
  if (!row) return null;
  try {
    const p = JSON.parse(row.value) as Pending;
    if (!p || typeof p.name !== 'string' || typeof p.expires_ms !== 'number') return null;
    if (p.expires_ms <= Date.now()) {
      store().prepare('DELETE FROM settings WHERE key = ?').run(key(token));
      return null;
    }
    return p;
  } catch {
    return null;
  }
}

export function beginEnrollment(name: string): { token: string; expires_s: number } {
  if (!NAME.test(name)) throw new Error('a host\'s name is lowercase letters, digits and -, up to 63 characters');
  if (hosts().some((h) => h.name === name)) throw new Error('a host of this name is in the panel already');
  const token = crypto.randomBytes(32).toString('hex');
  const expires_ms = Date.now() + TTL_MS;
  store().prepare('DELETE FROM settings WHERE key LIKE \'enrollment:%\' AND json_extract(value, \'$.expires_ms\') <= ?')
    .run(Date.now());
  store().prepare('INSERT INTO settings (key, value) VALUES (?, ?)').run(key(token), JSON.stringify({ name, expires_ms }));
  return { token, expires_s: Math.floor(expires_ms / 1000) };
}

export function completeEnrollment(token: string, name: string, url: string, pin: string): Host {
  const p = pending(token);
  if (!p || p.name !== name) throw new Error('the enrollment token is unknown, used or past its 15 minutes');
  if (hosts().some((h) => h.name === name || h.url === url || h.pin.toLowerCase() === pin.toLowerCase())) {
    throw new Error('a host of this name, address or key is in the panel already');
  }
  const h = addHost(name, url, pin);
  store().prepare('DELETE FROM settings WHERE key = ?').run(key(token));
  return h;
}
