#!/usr/bin/env node
// mock/seed.mjs: a seed for the panel -- what `api panel create` hands the
// appliance through cloud-init -- made for the mock API (mock/server.mjs):
// its address and key, the mock's admin as the panel's identity, and root's
// password hashed as nos-install hashes it.
//
//     node mock/seed.mjs <mock state folder> <port> <password> > seed.json

import crypto from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';

const [state, port, password] = process.argv.slice(2);
if (!state || !port || !password) {
  console.error('usage: node mock/seed.mjs <mock state folder> <port> <password>');
  process.exit(2);
}
const pin = (pem) => {
  const spki = new crypto.X509Certificate(pem).publicKey.export({ type: 'spki', format: 'der' });
  return crypto.createHash('sha256').update(spki).digest('hex');
};
const N = 1 << 15;
const salt = crypto.randomBytes(16);
const hash = crypto.scryptSync(password, salt, 32, { N, r: 8, p: 1, maxmem: 128 * N * 8 * 2 });
const read = (f) => fs.readFileSync(path.join(state, f), 'utf8');
process.stdout.write(JSON.stringify({
  host: 'mock',
  host_url: '127.0.0.1:' + port,
  host_pin: pin(read('host.crt')),
  client_cert: read('admin.crt'),
  client_key: read('admin.key'),
  password_hash: ['scrypt', N, 8, 1, salt.toString('base64'), hash.toString('base64')].join('$'),
}, null, 2) + '\n');
