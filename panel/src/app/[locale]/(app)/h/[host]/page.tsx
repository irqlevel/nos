import { notFound } from 'next/navigation';

import { Dashboard } from '@/components/Dashboard';
import { host as hostOf } from '@/lib/hosts';
import { isLocale, words } from '@/lib/i18n';

export default async function HostDashboard({ params }: { params: Promise<{ locale: string; host: string }> }) {
  const { locale, host } = await params;
  if (!isLocale(locale)) notFound();
  const h = hostOf(host);
  if (!h) notFound();
  return <Dashboard locale={locale} host={host} name={h.name} w={words(locale)} />;
}
