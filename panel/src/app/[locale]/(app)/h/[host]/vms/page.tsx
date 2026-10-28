import Link from 'next/link';
import { notFound } from 'next/navigation';

import { VmTable } from '@/components/VmTable';
import { descriptor } from '@/lib/catalog';
import { isLocale, words } from '@/lib/i18n';

export default async function Vms({ params }: { params: Promise<{ locale: string; host: string }> }) {
  const { locale, host } = await params;
  if (!isLocale(locale)) notFound();
  const d = await descriptor(host, 'vm');
  if (!d) notFound();
  const w = words(locale);
  return (
    <div className="space-y-4">
      <div className="flex items-center justify-between">
        <h1 className="text-2xl font-semibold text-fg">{w.vms}</h1>
        <Link href={`/${locale}/h/${host}/vms/new`} className="rounded-lg bg-accent px-3 py-1.5 text-sm font-medium text-bg hover:bg-accent-strong">
          {w.createVm}
        </Link>
      </div>
      <VmTable host={host} d={d} locale={locale} w={w} />
    </div>
  );
}
