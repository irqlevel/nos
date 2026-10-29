import { notFound, redirect } from 'next/navigation';

import { hosts } from '@/lib/hosts';
import { isLocale } from '@/lib/i18n';

export default async function Home({ params }: { params: Promise<{ locale: string }> }) {
  const { locale } = await params;
  if (!isLocale(locale)) notFound();
  const first = hosts()[0];
  redirect(first ? `/${locale}/h/${first.id}` : `/${locale}/hosts`);
}
