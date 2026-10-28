import { notFound } from 'next/navigation';

import { LoginForm } from '@/components/LoginForm';
import { isLocale, words } from '@/lib/i18n';

export default async function Login({ params }: { params: Promise<{ locale: string }> }) {
  const { locale } = await params;
  if (!isLocale(locale)) notFound();
  const w = words(locale);
  return (
    <main className="flex min-h-screen items-center justify-center px-4">
      <div className="w-full max-w-sm rounded-xl border border-line bg-card p-6 shadow-sm">
        <h1 className="mb-6 font-mono text-xl font-semibold text-fg">
          nos<span className="text-accent">_</span> <span className="text-muted">panel</span>
        </h1>
        <LoginForm locale={locale} w={{ user: w.user, password: w.password, login: w.login, wrong: w.wrongPassword,
          tooMany: w.tooManyTries }} />
      </div>
    </main>
  );
}
