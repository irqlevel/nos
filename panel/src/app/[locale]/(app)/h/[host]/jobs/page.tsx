import { notFound } from 'next/navigation';

import { Jobs } from '@/components/Jobs';
import { isLocale, words } from '@/lib/i18n';

export default async function JobsPage({ params }: { params: Promise<{ locale: string; host: string }> }) {
  const { locale, host } = await params;
  if (!isLocale(locale)) notFound();
  return <Jobs host={host} w={words(locale)} />;
}
