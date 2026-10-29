'use client';

import { useEffect, useState } from 'react';

import { JobWatch } from './JobWatch';
import type { Words } from '@/lib/i18n';

interface Job { id: number; provider: string; method: string; state: string; status: string | null; done: number; total: number;
  queued_ms: number; ended_ms: number }

/** The host's jobs, as they change; one opened, its log and result. */
export function Jobs({ host, w }: { host: string; w: Words }) {
  const [jobs, setJobs] = useState<Job[]>([]);
  const [open, setOpen] = useState<number | null>(null);
  useEffect(() => {
    const es = new EventSource(`/api/events/${host}`);
    es.onmessage = (m) => {
      const e = JSON.parse(m.data);
      if (e.type === 'jobs') setJobs(e.jobs);
    };
    return () => es.close();
  }, [host]);
  return (
    <div className="space-y-4">
      <h1 className="text-2xl font-semibold text-fg">{w.jobs}</h1>
      {jobs.length === 0 && <p className="text-sm text-muted">{w.nothingYet}</p>}
      <ul className="space-y-2">
        {jobs.slice().reverse().map((j) => (
          <li key={j.id} className="rounded-xl border border-line bg-card">
            <button type="button" onClick={() => setOpen(open === j.id ? null : j.id)}
              className="flex w-full items-center justify-between px-4 py-2 text-left text-sm">
              <span className="font-mono">#{j.id} {j.provider}.{j.method}</span>
              <span className={j.status === 'ok' ? 'text-ok' : j.status ? 'text-bad' : 'text-muted'}>
                {j.status ?? (j.total ? `${j.done}/${j.total}` : j.state)}
              </span>
            </button>
            {open === j.id && <div className="px-4 pb-4"><JobWatch host={host} id={j.id} w={w} /></div>}
          </li>
        ))}
      </ul>
    </div>
  );
}
