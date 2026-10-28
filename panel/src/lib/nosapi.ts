// A nos host's API (docs/api.md), as the panel's server reaches it: TLS 1.3
// with the panel's own client certificate, the host's key pinned -- the
// SHA-256 of its SubjectPublicKeyInfo, as `api status` prints it -- and
// checked before a byte of HTTP is sent. No CA vouches for a host; a host
// showing another key is refused. Then HTTP/1.1, kept alive: a few
// connections a host, each one request at a time, as the API serves them --
// and the stream, a WebSocket of RFC 6455's over a connection of its own.
//
// Node's own `https` sends what it was asked to as soon as the handshake is
// done, and checks a key only through a CA's chain; so the request is
// written here, after the pin is checked, on the socket the check was made
// on.

import crypto from 'node:crypto';
import tls from 'node:tls';

/** A host as the panel keeps it: where its API is, its key, and the
 * panel's identity there. */
export interface HostLink {
  id: string;
  /** `host:port`, or `https://host:port`. */
  url: string;
  /** The host's key: the hex SHA-256 of its certificate's SPKI. */
  pin: string;
  /** The panel's certificate and key there, PEM. */
  cert: string;
  key: string;
}

/** What the host answered, or why it did not. */
export interface Answer {
  status: number;
  headers: Record<string, string>;
  body: Buffer;
}

export class HostError extends Error {
  constructor(message: string, readonly kind: 'pin' | 'connect' | 'protocol' | 'timeout') {
    super(message);
  }
}

/** How long a request waits for its answer, and a connection for its
 * handshake: a host under TCG is slow, a dead one is not waited on. */
const ANSWER_MS = 30_000;
const CONNECT_MS = 10_000;
/** Connections a host, at most: the API lets a key have four. */
const POOL = 3;
/** The most of an answer: the API's own. */
const ANSWER_MAX = 512 * 1024;

/** The SHA-256 of a certificate's SubjectPublicKeyInfo, hex. */
export function spkiPin(der: Buffer): string {
  const spki = new crypto.X509Certificate(der).publicKey.export({ type: 'spki', format: 'der' });
  return crypto.createHash('sha256').update(spki).digest('hex');
}

function address(url: string): { host: string; port: number } {
  const u = new URL(url.includes('://') ? url : 'https://' + url);
  return { host: u.hostname, port: Number(u.port || 39234) };
}

/** A TLS connection to the host, its key checked: resolved only once it is
 * the pinned one. */
export function connect(link: HostLink): Promise<tls.TLSSocket> {
  const { host, port } = address(link.url);
  return new Promise((resolve, reject) => {
    const socket = tls.connect({
      host, port, cert: link.cert, key: link.key, minVersion: 'TLSv1.3', ALPNProtocols: ['nos-api/1', 'http/1.1'],
      /* No CA: the key is what is checked, below */
      rejectUnauthorized: false, servername: /^[\d.]+$/.test(host) ? undefined : host,
    });
    const timer = setTimeout(() => {
      socket.destroy();
      reject(new HostError(`no answer from ${host}:${port} in ${CONNECT_MS / 1000} s`, 'timeout'));
    }, CONNECT_MS);
    socket.once('secureConnect', () => {
      clearTimeout(timer);
      const cert = socket.getPeerCertificate(true);
      const got = cert && cert.raw ? spkiPin(cert.raw) : '';
      if (got !== link.pin.toLowerCase()) {
        socket.destroy();
        reject(new HostError(`the host shows the key ${got || 'none'}, not the one pinned (${link.pin})`, 'pin'));
        return;
      }
      resolve(socket);
    });
    socket.once('error', (e) => {
      clearTimeout(timer);
      reject(new HostError(`${host}:${port}: ${e.message}`, 'connect'));
    });
  });
}

/** An HTTP/1.1 answer read off `socket`, `head` true for a HEAD's. */
function readAnswer(socket: tls.TLSSocket, head: boolean): Promise<Answer & { keep: boolean }> {
  return new Promise((resolve, reject) => {
    let buf = Buffer.alloc(0);
    let parsed: { status: number; headers: Record<string, string>; at: number; len: number } | null = null;
    const done = (e?: Error, a?: Answer & { keep: boolean }) => {
      clearTimeout(timer);
      socket.off('data', onData);
      socket.off('close', onClose);
      socket.off('error', onError);
      if (e) reject(e);
      else resolve(a!);
    };
    const onData = (chunk: Buffer) => {
      buf = Buffer.concat([buf, chunk]);
      if (buf.length > ANSWER_MAX) {
        return done(new HostError('an answer past what the API sends', 'protocol'));
      }
      if (!parsed) {
        const end = buf.indexOf('\r\n\r\n');
        if (end < 0) return;
        const lines = buf.subarray(0, end).toString('latin1').split('\r\n');
        const status = Number((lines[0] ?? '').split(' ')[1]);
        const headers: Record<string, string> = {};
        for (const line of lines.slice(1)) {
          const i = line.indexOf(':');
          if (i > 0) headers[line.slice(0, i).trim().toLowerCase()] = line.slice(i + 1).trim();
        }
        if (!Number.isInteger(status)) {
          return done(new HostError('what the host sent is no HTTP', 'protocol'));
        }
        const bodiless = head || status === 204 || status === 304 || status === 101;
        parsed = { status, headers, at: end + 4, len: bodiless ? 0 : Number(headers['content-length'] || 0) };
      }
      if (buf.length >= parsed.at + parsed.len) {
        const body = buf.subarray(parsed.at, parsed.at + parsed.len);
        const keep = (parsed.headers['connection'] || '').toLowerCase() !== 'close';
        done(undefined, { status: parsed.status, headers: parsed.headers, body: Buffer.from(body), keep });
      }
    };
    const onClose = () => done(new HostError('the host closed the connection', 'protocol'));
    const onError = (e: Error) => done(new HostError(e.message, 'connect'));
    const timer = setTimeout(() => done(new HostError(`no answer in ${ANSWER_MS / 1000} s`, 'timeout')), ANSWER_MS);
    socket.on('data', onData);
    socket.once('close', onClose);
    socket.once('error', onError);
  });
}

function requestBytes(link: HostLink, method: string, path: string, body?: Buffer, headers?: Record<string, string>) {
  const { host } = address(link.url);
  const lines = [`${method} ${path} HTTP/1.1`, `Host: ${host}`, `Content-Length: ${body ? body.length : 0}`];
  if (body && body.length) lines.push('Content-Type: application/json');
  for (const [k, v] of Object.entries(headers || {})) lines.push(`${k}: ${v}`);
  return Buffer.concat([Buffer.from(lines.join('\r\n') + '\r\n\r\n', 'latin1'), body || Buffer.alloc(0)]);
}

/** A host's connections, a request each at a time, kept alive. */
class Pool {
  private idle: tls.TLSSocket[] = [];
  private busy = 0;
  private waiting: Array<() => void> = [];

  constructor(private link: HostLink) {}

  private async take(): Promise<tls.TLSSocket> {
    while (true) {
      const s = this.idle.pop();
      if (s && !s.destroyed) {
        this.busy += 1;
        return s;
      }
      if (this.busy < POOL) {
        this.busy += 1;
        try {
          return await connect(this.link);
        } catch (e) {
          this.busy -= 1;
          this.wake();
          throw e;
        }
      }
      await new Promise<void>((r) => this.waiting.push(r));
    }
  }

  private give(s: tls.TLSSocket, keep: boolean) {
    this.busy -= 1;
    if (keep && !s.destroyed) {
      this.idle.push(s);
    } else {
      s.destroy();
    }
    this.wake();
  }

  private wake() {
    const w = this.waiting.shift();
    if (w) w();
  }

  async request(method: string, path: string, body?: Buffer, headers?: Record<string, string>): Promise<Answer> {
    /* A kept connection the host has let go of meanwhile: once more, on a
     * new one. */
    for (let attempt = 0; ; attempt++) {
      const fresh = this.idle.length === 0;
      const s = await this.take();
      try {
        s.write(requestBytes(this.link, method, path, body, headers));
        const a = await readAnswer(s, method === 'HEAD');
        this.give(s, a.keep);
        return a;
      } catch (e) {
        this.give(s, false);
        if (fresh || attempt > 0 || !(e instanceof HostError) || e.kind === 'pin') throw e;
      }
    }
  }

  close() {
    for (const s of this.idle) s.destroy();
    this.idle = [];
  }
}

const pools = new Map<string, { link: HostLink; pool: Pool }>();

function poolOf(link: HostLink): Pool {
  const had = pools.get(link.id);
  if (had && had.link.url === link.url && had.link.pin === link.pin && had.link.cert === link.cert) {
    return had.pool;
  }
  had?.pool.close();
  const pool = new Pool(link);
  pools.set(link.id, { link, pool });
  return pool;
}

/** A request to the host's API: its answer, the body parsed when it is JSON. */
export async function api(link: HostLink, method: string, path: string, body?: unknown,
                          headers?: Record<string, string>): Promise<{ status: number; json: unknown; headers: Record<string, string> }> {
  const bytes = body === undefined ? undefined : Buffer.from(typeof body === 'string' ? body : JSON.stringify(body));
  const a = await poolOf(link).request(method, path, bytes, headers);
  let json: unknown = null;
  if (a.body.length && (a.headers['content-type'] || '').startsWith('application/json')) {
    json = JSON.parse(a.body.toString('utf8'));
  }
  return { status: a.status, json, headers: a.headers };
}

/* ---- the stream ---- */

export interface Stream {
  /** A message of the client's: what the console channel types. */
  send(text: string): void;
  close(): void;
}

function clientFrame(opcode: number, payload: Buffer): Buffer {
  const mask = crypto.randomBytes(4);
  const n = payload.length;
  const head = n < 126 ? Buffer.from([0x80 | opcode, 0x80 | n])
    : n < 65536 ? Buffer.from([0x80 | opcode, 0x80 | 126, n >> 8, n & 0xff])
      : (() => {
        const h = Buffer.alloc(10);
        h[0] = 0x80 | opcode;
        h[1] = 0x80 | 127;
        h.writeBigUInt64BE(BigInt(n), 2);
        return h;
      })();
  const masked = Buffer.from(payload);
  for (let i = 0; i < masked.length; i++) masked[i] = (masked[i] ?? 0) ^ (mask[i % 4] ?? 0);
  return Buffer.concat([head, mask, masked]);
}

/** /v1/stream with `query` opened on a connection of its own: each text
 * message handed to `onMessage` parsed, pings answered, `onEnd` once it is
 * over -- the host closed it, or it broke. */
export async function stream(link: HostLink, query: string, onMessage: (m: unknown) => void,
                             onEnd: (why: string) => void): Promise<Stream> {
  const socket = await connect(link);
  const key = crypto.randomBytes(16).toString('base64');
  const { host } = address(link.url);
  socket.write(`GET /v1/stream${query} HTTP/1.1\r\nHost: ${host}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n`
    + `Sec-WebSocket-Key: ${key}\r\nSec-WebSocket-Version: 13\r\n\r\n`);
  const want = crypto.createHash('sha1').update(key + '258EAFA5-E914-47DA-95CA-C5AB0DC85B11').digest('base64');
  let buf = Buffer.alloc(0);
  let open = false;
  let ended = false;
  const end = (why: string) => {
    if (!ended) {
      ended = true;
      socket.destroy();
      onEnd(why);
    }
  };
  let message: Buffer[] = [];
  socket.on('data', (chunk: Buffer) => {
    buf = Buffer.concat([buf, chunk]);
    if (!open) {
      const at = buf.indexOf('\r\n\r\n');
      if (at < 0) return;
      const lines = buf.subarray(0, at).toString('latin1').split('\r\n');
      buf = buf.subarray(at + 4);
      const accept = lines.slice(1).map((l) => l.split(':')).find(([k]) => (k ?? '').trim().toLowerCase() === 'sec-websocket-accept');
      const status = lines[0] ?? '';
      if (!/^HTTP\/1\.1 101 /.test(status) || !accept || accept.slice(1).join(':').trim() !== want) {
        return end('no WebSocket: ' + status);
      }
      open = true;
    }
    while (buf.length >= 2) {
      const b0 = buf.readUInt8(0);
      let n = buf.readUInt8(1) & 0x7f;
      let at = 2;
      if (n === 126) {
        if (buf.length < 4) return;
        n = buf.readUInt16BE(2);
        at = 4;
      } else if (n === 127) {
        if (buf.length < 10) return;
        n = Number(buf.readBigUInt64BE(2));
        at = 10;
      }
      if (n > ANSWER_MAX) return end('a frame past what the API sends');
      if (buf.length < at + n) return;
      const payload = buf.subarray(at, at + n);
      buf = buf.subarray(at + n);
      const op = b0 & 0x0f;
      if (op === 0x9) {
        socket.write(clientFrame(0xa, payload));
      } else if (op === 0x8) {
        return end('the host closed the stream' + (payload.length >= 2 ? ` (${payload.readUInt16BE(0)})` : ''));
      } else if (op === 0x1 || op === 0x0) {
        message.push(Buffer.from(payload));
        if (b0 & 0x80) {
          const text = Buffer.concat(message).toString('utf8');
          message = [];
          try {
            onMessage(JSON.parse(text));
          } catch {
            return end('a message that is not JSON');
          }
        }
      }
    }
  });
  socket.on('close', () => end('the connection closed'));
  socket.on('error', (e) => end(e.message));
  return {
    send: (text: string) => {
      if (!ended) socket.write(clientFrame(0x1, Buffer.from(text, 'utf8')));
    },
    close: () => {
      if (!ended) {
        socket.write(clientFrame(0x8, Buffer.from([0x03, 0xe8])));
        end('closed');
      }
    },
  };
}
