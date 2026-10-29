import { notFound } from 'next/navigation';

import { VmView } from '@/components/VmView';
import { descriptor } from '@/lib/catalog';
import { isLocale, words } from '@/lib/i18n';

export default async function VmPage({ params }: { params: Promise<{ locale: string; host: string; name: string }> }) {
  const { locale, host, name } = await params;
  if (!isLocale(locale) || !/^[A-Za-z0-9][A-Za-z0-9._-]{0,31}$/.test(name)) notFound();
  const d = await descriptor(host, 'vm');
  if (!d) notFound();
  return <VmView host={host} name={name} d={d} locale={locale} w={words(locale)} />;
}
