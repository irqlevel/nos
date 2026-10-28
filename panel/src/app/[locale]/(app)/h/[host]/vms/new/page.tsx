import { notFound } from 'next/navigation';

import { CreateVm } from '@/components/CreateVm';
import { isLocale, words } from '@/lib/i18n';

export default async function NewVm({ params }: { params: Promise<{ locale: string; host: string }> }) {
  const { locale, host } = await params;
  if (!isLocale(locale)) notFound();
  return <CreateVm host={host} locale={locale} w={words(locale)} />;
}
