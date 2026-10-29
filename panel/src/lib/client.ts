// The panel's own API, from a page: a host's call, a job started and
// watched. What a host refused comes back as its error's message.

export interface Refusal { code: string; message: string; field?: string }

export async function call(host: string, provider: string, method: string, params: unknown = {}):
    Promise<{ ok: true; data: unknown } | { ok: false; error: Refusal }> {
  const res = await fetch(`/api/h/${host}/call/${provider}/${method}`, {
    method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(params),
  });
  const body = await res.json().catch(() => ({}));
  return res.ok ? { ok: true, data: body } : { ok: false, error: body.error ?? { code: String(res.status), message: res.statusText } };
}

export async function startJob(host: string, provider: string, method: string, params: unknown = {}):
    Promise<{ ok: true; id: number } | { ok: false; error: Refusal }> {
  const res = await fetch(`/api/h/${host}/jobs/${provider}/${method}`, {
    method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(params),
  });
  const body = await res.json().catch(() => ({}));
  return res.status === 202 ? { ok: true, id: body.id } : { ok: false, error: body.error ?? { code: String(res.status), message: res.statusText } };
}

export interface JobRecord {
  id: number; provider: string; method: string; state: string; status: string | null; done: number; total: number;
  notes: Array<{ seq: number; text: string }>; notes_next: number; result: unknown;
}

export async function jobRecord(host: string, id: number, since: number): Promise<JobRecord | null> {
  const res = await fetch(`/api/h/${host}/jobs/${id}?notes_since=${since}`);
  return res.ok ? res.json() : null;
}

export async function cancelJob(host: string, id: number) {
  await fetch(`/api/h/${host}/jobs/${id}/cancel`, { method: 'POST' });
}
