import { notFound } from 'next/navigation';

import { PasswordForm } from '@/components/PasswordForm';
import { isLocale, words } from '@/lib/i18n';

export default async function Settings({ params }: { params: Promise<{ locale: string }> }) {
  const { locale } = await params;
  if (!isLocale(locale)) notFound();
  const w = words(locale);
  return (
    <div className="mx-auto max-w-xl space-y-6 px-4 py-6">
      <h1 className="text-2xl font-semibold text-fg">{w.settings}</h1>
      <PasswordForm w={w} />
    </div>
  );
}
