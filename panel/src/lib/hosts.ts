// The hosts the panel manages: the one it runs on first, others added by
// their API's address and key. The panel is one client to all of them --
// its certificate and key, made by nos for the host it was made on and
// handed to it in its seed -- and each host lets it in by its key's
// fingerprint, which `api client add panel-<its first 8 digits> admin
// <fingerprint>` puts in that host's list (`panel` is that host's own
// panel's, where it has one). Each host's own key is pinned as it is added.

import crypto from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';

import { HostLink, spkiPin } from './nosapi';
import { hashPassword, USER } from './auth';
import { DATA, store } from './store';

export interface Host {
  id: string;
  name: string;
  url: string;
  pin: string;
  added_ms: number;
}

/** The panel's identity: its certificate and key, PEM. */
export function identity(): { cert: string; key: string } | null {
  const row = store().prepare('SELECT cert, key FROM identity WHERE id = 1').get() as { cert: string; key: string } | undefined;
  return row ?? null;
}

/** The panel's key's fingerprint, as a host's `api client add` takes it. */
export function fingerprint(): string | null {
  const id = identity();
  return id ? spkiPin(new crypto.X509Certificate(id.cert).raw) : null;
}

export function hosts(): Host[] {
  return store().prepare('SELECT id, name, url, pin, added_ms FROM hosts ORDER BY added_ms').all() as unknown as Host[];
}

export function host(id: string): Host | undefined {
  return store().prepare('SELECT id, name, url, pin, added_ms FROM hosts WHERE id = ?').get(id) as Host | undefined;
}

/** A host as the API client reaches it. */
export function link(h: Host): HostLink {
  const id = identity();
  if (!id) {
    throw new Error('the panel has no identity yet: its seed was not read');
  }
  return { id: h.id, url: h.url, pin: h.pin, cert: id.cert, key: id.key };
}

const PIN = /^[0-9a-f]{64}$/;
const NAME = /^[A-Za-z0-9][A-Za-z0-9._-]{0,31}$/;

export function addHost(name: string, url: string, pin: string): Host {
  if (!NAME.test(name)) {
    throw new Error('a host\'s name is a letter or a digit, then letters, digits, . _ and -');
  }
  const u = new URL(url.includes('://') ? url : 'https://' + url);
  if (u.protocol !== 'https:' || u.pathname !== '/' || u.search) {
    throw new Error('a host is an https:// address and a port');
  }
  if (!PIN.test(pin.toLowerCase())) {
    throw new Error('a host\'s key is 64 hex digits: the SHA-256 `api status` prints');
  }
  const h = { id: crypto.randomUUID(), name, url: u.host, pin: pin.toLowerCase(), added_ms: Date.now() };
  store().prepare('INSERT INTO hosts (id, name, url, pin, added_ms) VALUES (?, ?, ?, ?, ?)')
    .run(h.id, h.name, h.url, h.pin, h.added_ms);
  return h;
}

export function removeHost(id: string) {
  store().prepare('DELETE FROM hosts WHERE id = ?').run(id);
  store().prepare('DELETE FROM samples WHERE host = ?').run(id);
}

/* ---- the seed ---- */

/** What nos hands the appliance as it makes it (`api panel create`), in
 * its cloud-init seed, written to `PANEL_SEED` by cloud-init: the host's
 * API, its key, the panel's identity there and root's password's hash. */
interface Seed {
  host: string;
  host_url: string;
  host_pin: string;
  client_cert: string;
  client_key: string;
  password_hash: string;
}

export const SEED = process.env.PANEL_SEED || '/etc/nos-panel/seed.json';

/** The seed read into the store, once -- on the first start, or on one that
 * finds the store has nothing yet. What it says of the password goes in
 * only where there is no password yet: one changed in the panel stays. */
export async function readSeed(): Promise<string> {
  const d = store();
  const have = d.prepare('SELECT COUNT(*) AS n FROM users').get() as { n: number };
  if (!fs.existsSync(/*turbopackIgnore: true*/ SEED)) {
    if (have.n === 0 && process.env.PANEL_DEV_PASSWORD) {
      /* Development alone: a password from the environment. */
      d.prepare('INSERT INTO users (name, hash, changed_ms) VALUES (?, ?, ?)')
        .run(USER, await hashPassword(process.env.PANEL_DEV_PASSWORD), Date.now());
    }
    return 'no seed at ' + SEED;
  }
  const seed = JSON.parse(fs.readFileSync(/*turbopackIgnore: true*/ SEED, 'utf8')) as Seed;
  if (have.n === 0 && seed.password_hash) {
    d.prepare('INSERT INTO users (name, hash, changed_ms) VALUES (?, ?, ?)').run(USER, seed.password_hash, Date.now());
  }
  if (!identity() && seed.client_cert && seed.client_key) {
    d.prepare('INSERT INTO identity (id, cert, key) VALUES (1, ?, ?)').run(seed.client_cert, seed.client_key);
    fs.writeFileSync(/*turbopackIgnore: true*/ path.join(DATA, '.identity-from-seed'), new Date().toISOString() + '\n', { mode: 0o600 });
  }
  if (hosts().length === 0 && seed.host_url && seed.host_pin) {
    addHost(seed.host || 'nos', seed.host_url, seed.host_pin);
  }
  return 'seed read from ' + SEED;
}
