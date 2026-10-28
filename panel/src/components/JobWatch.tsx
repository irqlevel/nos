'use client';

import { useEffect, useState } from 'react';

import { cancelJob, jobRecord, JobRecord } from '@/lib/client';
import type { Words } from '@/lib/i18n';

/** A job followed to its end: its log as it comes, how far it has got, and
 * its result -- or why it failed. */
export function JobWatch({ host, id, w, onDone }: { host: string; id: number; w: Words;
  onDone?: (r: JobRecord) => void }) {
  const [rec, setRec] = useState<JobRecord | null>(null);
  const [notes, setNotes] = useState<Array<{ seq: number; text: string }>>([]);

  useEffect(() => {
    let since = 0;
    let stop = false;
    const tick = async () => {
      const r = await jobRecord(host, id, since);
      if (stop) return;
      if (r) {
        setRec(r);
        if (r.notes.length) setNotes((n) => [...n, ...r.notes]);
        since = r.notes_next;
        if (r.state === 'done') {
          onDone?.(r);
          return;
        }
      }
      setTimeout(tick, 700);
    };
    void tick();
    return () => {
      stop = true;
    };
  }, [host, id, onDone]);

  const pct = rec && rec.total ? Math.min(100, Math.round((rec.done * 100) / rec.total)) : null;
  const error = (rec?.result as { error?: { message?: string } } | null)?.error;
  return (
    <div className="space-y-2 rounded-lg border border-line bg-bg-soft p-3 text-sm" data-testid="job">
      <div className="flex items-center justify-between gap-2">
        <span className="font-mono">{w.job} #{id}</span>
        <span className={rec?.status === 'ok' ? 'text-ok' : rec?.status ? 'text-bad' : 'text-muted'} data-testid="job-status">
          {rec?.status ?? rec?.state ?? '…'}
        </span>
      </div>
      {rec?.state !== 'done' && (
        <div className="flex items-center gap-2">
          <div className="h-1.5 flex-1 overflow-hidden rounded bg-line">
            <div className="h-full bg-accent transition-all" style={{ width: `${pct ?? 30}%` }} />
          </div>
          <button type="button" onClick={() => void cancelJob(host, id)} className="text-xs text-muted hover:text-bad">
            {w.cancel}
          </button>
        </div>
      )}
      {notes.length > 0 && (
        <pre className="max-h-48 overflow-auto font-mono text-xs leading-5 text-fg-soft">{notes.map((n) => n.text).join('\n')}</pre>
      )}
      {error?.message && <p className="text-bad">{error.message}</p>}
    </div>
  );
}
