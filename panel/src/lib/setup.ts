import { call, jobRecord, startJob } from './client';
import { addressFor, ipOfUrl } from './fills';

export interface SetupHost {
  id: string;
  name: string;
  url: string;
  online: boolean;
}

export interface ClusterStatus {
  member: boolean;
  cluster?: string | null;
  stopped: string | null;
  this_host: { name: string; key: string; ip: string | null; port: number | null; serving: boolean } | null;
}

export interface NbsStatus { member: boolean; cluster?: string | null; stopped: string | null }

export interface SetupDevice {
  name: string;
  id: string | null;
  size_bytes: number;
  holds: string | null;
  in_use_by: string | null;
  boots_the_machine: string | null;
}

export interface Candidate {
  host: SetupHost;
  cluster: ClusterStatus;
  nbs: NbsStatus;
  devices: SetupDevice[];
}

export interface DeviceChoice { id: string; wipe: boolean }

export interface SetupRequest {
  candidates: Candidate[];
  leaderId: string;
  clusterName: string;
  nbsName: string;
  devices: Record<string, DeviceChoice>;
}

const NAME = /^[a-z0-9-]{1,63}$/;

function failed(where: string, message: string): Error {
  return new Error(where + ': ' + message);
}

async function ask(host: string, provider: string, method: string, params: unknown = {}): Promise<unknown> {
  const r = await call(host, provider, method, params);
  if (!r.ok) throw failed(`${host} ${provider}.${method}`, r.error.message);
  return r.data;
}

async function job(host: string, provider: string, method: string, params: unknown): Promise<unknown> {
  const started = await startJob(host, provider, method, params);
  if (!started.ok) throw failed(`${host} ${provider}.${method}`, started.error.message);
  for (let turn = 0; turn < 180; turn++) {
    const r = await jobRecord(host, started.id, 0);
    if (r?.state === 'done') {
      if (r.status !== 'ok') {
        const error = (r.result as { error?: { message?: string } } | null)?.error;
        throw failed(`${host} ${provider}.${method}`, error?.message ?? r.status ?? 'failed');
      }
      return r.result;
    }
    await new Promise((resolve) => setTimeout(resolve, 500));
  }
  throw failed(`${host} ${provider}.${method}`, 'the job did not finish in time');
}

export function usableDevice(d: SetupDevice): boolean {
  return d.id !== null && d.in_use_by === null && d.boots_the_machine === null;
}

export function validateSetup(req: SetupRequest): void {
  if (req.candidates.length < 3) throw new Error('choose at least three hosts: NBS places every chunk on three');
  if (!NAME.test(req.clusterName) || !NAME.test(req.nbsName)) {
    throw new Error('the cluster and NBS names are lowercase letters, digits and -, up to 63 characters');
  }
  if (!req.candidates.some((c) => c.host.id === req.leaderId)) throw new Error('the first host is not among those chosen');
  const leader = req.candidates.find((c) => c.host.id === req.leaderId)!;
  if (!leader.cluster.member && req.candidates.some((c) => c.cluster.member)) {
    throw new Error('the first host is in no cluster, but another chosen host is: choose an existing member first');
  }
  if (!leader.nbs.member && req.candidates.some((c) => c.nbs.member)) {
    throw new Error('the first host is in no NBS cluster, but another chosen host is: choose an existing member first');
  }
  const names = new Set<string>();
  const keys = new Set<string>();
  for (const c of req.candidates) {
    const me = c.cluster.this_host;
    if (!c.host.online || !me?.serving || !me.ip) throw new Error(`${c.host.name}: ipcnet is not serving a reachable address`);
    if (c.cluster.stopped || c.nbs.stopped) throw new Error(`${c.host.name}: a member is stopped`);
    if (c.cluster.member && c.cluster.cluster !== req.clusterName) {
      throw new Error(`${c.host.name}: it is in cluster ${c.cluster.cluster ?? '(unnamed)'}, not ${req.clusterName}`);
    }
    if (c.nbs.member && c.nbs.cluster !== req.nbsName) {
      throw new Error(`${c.host.name}: it is in NBS ${c.nbs.cluster ?? '(unnamed)'}, not ${req.nbsName}`);
    }
    if (names.has(me.name) || keys.has(me.key)) throw new Error(`${c.host.name}: another chosen host has its name or ipcnet key`);
    names.add(me.name);
    keys.add(me.key);
    const choice = req.devices[c.host.id];
    const device = c.devices.find((d) => d.id === choice?.id);
    if (!choice || !device || !usableDevice(device)) throw new Error(`${c.host.name}: choose a device nothing holds`);
    if (device.holds && !choice.wipe) throw new Error(`${c.host.name}: ${device.name} holds ${device.holds}; confirm wipe`);
  }
}

export async function runSetup(req: SetupRequest, step: (text: string) => void): Promise<void> {
  validateSetup(req);
  const leader = req.candidates.find((c) => c.host.id === req.leaderId)!;
  const others = req.candidates.filter((c) => c.host.id !== req.leaderId);
  if (!leader.cluster.member) {
    step(`cluster.init ${req.clusterName} on ${leader.host.name}`);
    await ask(leader.host.id, 'cluster', 'init', { name: req.clusterName });
  } else {
    step(`cluster ${req.clusterName} already exists on ${leader.host.name}`);
  }
  const registry = await ask(leader.host.id, 'cluster', 'hosts') as { hosts?: Array<{ name: string; key: string }> };
  const registered = new Set((registry.hosts ?? []).map((h) => h.key));
  for (const c of req.candidates) {
    const me = c.cluster.this_host!;
    if (c.cluster.member && !registered.has(me.key)) {
      throw new Error(`${c.host.name}: it is a member of another cluster, not the first host's one`);
    }
  }
  for (const c of others) {
    const me = c.cluster.this_host!;
    if (c.cluster.member) {
      step(`${me.name} is in the cluster already`);
      continue;
    }
    const at = addressFor(me.ip, ipOfUrl(c.host.url));
    step(`cluster.invite ${me.name}, then cluster.join on it`);
    const invited = await ask(leader.host.id, 'cluster', 'invite',
      { host: me.name, ip: at.ip, port: me.port ?? 0, key: me.key }) as { token?: string };
    if (!invited.token) throw new Error(`${leader.host.name}: the invitation has no token`);
    await job(c.host.id, 'cluster', 'join', { token: invited.token });
  }
  if (!leader.nbs.member) {
    step(`nbs.init ${req.nbsName} on ${leader.host.name}`);
    await ask(leader.host.id, 'nbs', 'init', { name: req.nbsName });
  } else {
    step(`NBS ${req.nbsName} already exists on ${leader.host.name}`);
  }
  const members = await ask(leader.host.id, 'nbs', 'hosts') as { hosts?: Array<{ host: string }> };
  const inNbs = new Set((members.hosts ?? []).map((h) => h.host));
  for (const c of req.candidates) {
    const me = c.cluster.this_host!;
    if (c.nbs.member && !inNbs.has(me.name)) throw new Error(`${c.host.name}: it is a member of another NBS cluster`);
  }
  for (const c of others) {
    const me = c.cluster.this_host!;
    if (c.nbs.member) {
      step(`${me.name} is in NBS already`);
      continue;
    }
    step(`nbs.host_add ${me.name}`);
    await job(leader.host.id, 'nbs', 'host_add', { host: me.name });
  }
  for (const c of req.candidates) {
    const choice = req.devices[c.host.id]!;
    step(`nbs.device_add ${choice.id} on ${c.cluster.this_host!.name}`);
    await ask(c.host.id, 'nbs', 'device_add', { device: choice.id, wipe: choice.wipe });
  }
  step('done');
}
