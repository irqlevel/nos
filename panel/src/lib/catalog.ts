// What a host has: its providers, their methods as the panel's client --
// an admin -- is shown them, and each one's descriptor, kept by its ETag
// so a page asks the host only whether it changed.

import { api } from './nosapi';
import { host as hostOf, link } from './hosts';
import { Descriptor, sectionOf } from './descriptor';

export interface ProviderEntry {
  name: string;
  methods: Array<{ name: string; kind: string }>;
}

const g = globalThis as unknown as { __nosDescriptors?: Map<string, { tag: string; d: Descriptor }> };
const cache = g.__nosDescriptors ?? (g.__nosDescriptors = new Map());

export async function providers(hostId: string): Promise<ProviderEntry[]> {
  const h = hostOf(hostId);
  if (!h) return [];
  const a = await api(link(h), 'GET', '/v1/providers');
  return a.status === 200 ? ((a.json as { providers: ProviderEntry[] }).providers ?? []) : [];
}

export async function descriptor(hostId: string, name: string): Promise<Descriptor | null> {
  const h = hostOf(hostId);
  if (!h) return null;
  const key = hostId + '/' + name;
  const had = cache.get(key);
  const a = await api(link(h), 'GET', '/v1/providers/' + name, undefined, had ? { 'If-None-Match': had.tag } : {});
  if (a.status === 304 && had) return had.d;
  if (a.status !== 200) return null;
  const d = a.json as Descriptor;
  if (a.headers['etag']) cache.set(key, { tag: a.headers['etag'], d });
  return d;
}

/** Every provider's descriptor, the host's sections in their order. */
export async function catalog(hostId: string): Promise<Descriptor[]> {
  const list = await providers(hostId);
  const all = await Promise.all(list.map((p) => descriptor(hostId, p.name)));
  return all.filter((d): d is Descriptor => d !== null)
    .sort((a, b) => (a.ui?.order ?? 100) - (b.ui?.order ?? 100) || sectionOf(a).localeCompare(sectionOf(b)));
}
