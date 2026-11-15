// Forms the panel fills in from another host it manages: what no host's own
// page can do, the panel alone reaching more than one. Each is a method, the
// read of the same provider asked of the other host, and where in its answer
// each parameter is. An invitation is the one there is: the host invited
// says its own name, ipcnet key and port in its cluster's status
// (`this_host`); its address is the one every host reaches it at, which it
// may not know itself (`addressFor`).
//
// Free of imports: the tests load it as it is.

export interface Fill {
  /** The read asked of the other host: the same provider's. */
  read: string;
  /** Each parameter's place in that read's answer, its members by dots. */
  params: Record<string, string>;
}

const FILLS: Record<string, Fill> = {
  'cluster.invite': {
    read: 'status',
    params: { host: 'this_host.name', key: 'this_host.key', port: 'this_host.port', ip: 'this_host.ip' },
  },
};

export function fillOf(provider: string, method: string): Fill | undefined {
  return FILLS[`${provider}.${method}`];
}

/** The value at `path` in `answer`, members by dots: undefined where there
 * is none. */
export function at(answer: unknown, path: string): unknown {
  let v: unknown = answer;
  for (const k of path.split('.')) {
    if (v === null || typeof v !== 'object' || Array.isArray(v)) return undefined;
    v = (v as Record<string, unknown>)[k];
  }
  return v;
}

const IPV4 = /^(\d{1,3})\.(\d{1,3})\.(\d{1,3})\.(\d{1,3})$/;

function octets(ip: string): number[] | null {
  const m = IPV4.exec(ip);
  if (!m) return null;
  const o = m.slice(1).map(Number);
  return o.every((n) => n <= 255) ? o : null;
}

/** Whether an IPv4 address is one the internet routes to: not a private
 * network's (RFC 1918), a carrier's NAT's (RFC 6598), a loopback, a link's
 * own, the unspecified, a multicast or a reserved one. */
export function isPublic(ip: string): boolean {
  const o = octets(ip);
  if (!o) return false;
  const [a, b] = [o[0]!, o[1]!];
  return !(a === 0 || a === 10 || a === 127 || a >= 224 || (a === 100 && b >= 64 && b < 128)
    || (a === 169 && b === 254) || (a === 172 && b >= 16 && b < 32) || (a === 192 && b === 168));
}

/** The IPv4 address a host's API address names, as the panel keeps it
 * (`host:port`): null for a name, or an IPv6 address. */
export function ipOfUrl(url: string): string | null {
  const i = url.lastIndexOf(':');
  const h = i > 0 ? url.slice(0, i) : url;
  return octets(h) ? h : null;
}

/** The address every host is to reach another at, of what that host says of
 * itself (`own`) and the address the panel reaches its API at (`reached`):
 * a public one first -- its own, else the panel's, which behind a NAT is
 * the NAT's and the host's own page cannot know -- else its own, else the
 * panel's. Hosts of one private network, a NAT before none of them, have
 * their own. */
export function addressFor(own: string | null, reached: string | null):
    { ip: string; from: 'own' | 'reached' | 'none' } {
  if (own && isPublic(own)) return { ip: own, from: 'own' };
  if (reached && isPublic(reached)) return { ip: reached, from: 'reached' };
  if (own) return { ip: own, from: 'own' };
  if (reached) return { ip: reached, from: 'reached' };
  return { ip: '', from: 'none' };
}

/** What an answer has that is too long to select by eye -- a token, a key,
 * an id -- each string of `min` characters or more, by its path: what a
 * button copies. */
export function copyable(answer: unknown, min = 16, prefix = ''): Array<[string, string]> {
  if (answer === null || typeof answer !== 'object' || Array.isArray(answer)) return [];
  const out: Array<[string, string]> = [];
  for (const [k, v] of Object.entries(answer as Record<string, unknown>)) {
    const path = prefix + k;
    if (typeof v === 'string' && v.length >= min) out.push([path, v]);
    else out.push(...copyable(v, min, path + '.'));
  }
  return out;
}
