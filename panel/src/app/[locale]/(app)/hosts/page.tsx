import { notFound } from 'next/navigation';

import { HostsManager } from '@/components/HostsManager';
import { isLocale, words } from '@/lib/i18n';

export default async function HostsPage({ params }: { params: Promise<{ locale: string }> }) {
  const { locale } = await params;
  if (!isLocale(locale)) notFound();
  return <div className="mx-auto max-w-3xl px-4 py-6"><HostsManager locale={locale} w={words(locale)} /></div>;
}
