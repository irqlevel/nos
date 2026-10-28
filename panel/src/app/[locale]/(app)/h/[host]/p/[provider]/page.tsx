import { notFound } from 'next/navigation';

import { ProviderView } from '@/components/ProviderView';
import { descriptor } from '@/lib/catalog';
import { isLocale, words } from '@/lib/i18n';

export default async function ProviderPage({ params }: { params: Promise<{ locale: string; host: string; provider: string }> }) {
  const { locale, host, provider } = await params;
  if (!isLocale(locale) || !/^[a-z][a-z0-9_]{0,23}$/.test(provider)) notFound();
  const d = await descriptor(host, provider);
  if (!d) notFound();
  return <ProviderView host={host} d={d} locale={locale} w={words(locale)} />;
}
