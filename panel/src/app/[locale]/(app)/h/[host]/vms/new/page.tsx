import { notFound } from 'next/navigation';

import { CreateVm } from '@/components/CreateVm';
import { isLocale, words } from '@/lib/i18n';

export default async function NewVm({ params, searchParams }: { params: Promise<{ locale: string; host: string }>;
  searchParams: Promise<{ image?: string }> }) {
  const { locale, host } = await params;
  const { image } = await searchParams;
  if (!isLocale(locale)) notFound();
  return <CreateVm host={host} locale={locale} w={words(locale)} image={typeof image === 'string' ? image : undefined} />;
}
