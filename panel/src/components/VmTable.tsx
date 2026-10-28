'use client';

import Link from 'next/link';

import { ResourceTable } from './ResourceTable';
import { Descriptor } from '@/lib/descriptor';
import { Locale, Words } from '@/lib/i18n';

/** The vm provider's VMs, each name the way into its page, looked at again
 * every few seconds. */
export function VmTable({ host, d, locale, w }: { host: string; d: Descriptor; locale: Locale; w: Words }) {
  const r = d.resources?.vms;
  if (!r) return null;
  return (
    <ResourceTable host={host} d={d} id="vms" r={r} locale={locale} w={w} every={3000}
      extra={(row) => (
        <Link href={`/${locale}/h/${host}/vms/${String(row.name)}`} className="font-medium text-accent hover:underline">
          {String(row.name ?? 'vm' + String(row.id))}
        </Link>
      )} />
  );
}
