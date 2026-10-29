'use client';

import { useParams, useRouter } from 'next/navigation';

export function HostPicker({ locale, hosts, label }: { locale: string; hosts: Array<{ id: string; name: string }>; label: string }) {
  const router = useRouter();
  const params = useParams<{ host?: string }>();
  if (hosts.length < 2) {
    return <span className="text-sm text-muted">{hosts[0]?.name}</span>;
  }
  return (
    <select aria-label={label} value={params.host ?? ''} onChange={(e) => router.push(`/${locale}/h/${e.target.value}`)}
      className="rounded-lg border border-line bg-bg px-2 py-1 text-sm text-fg">
      {hosts.map((h) => <option key={h.id} value={h.id}>{h.name}</option>)}
    </select>
  );
}
