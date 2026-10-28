import { cookies } from 'next/headers';
import Link from 'next/link';
import { notFound, redirect } from 'next/navigation';

import { HostPicker } from '@/components/HostPicker';
import { LogoutButton } from '@/components/LogoutButton';
import { ThemeToggle } from '@/components/ThemeToggle';
import { COOKIE, session } from '@/lib/auth';
import { hosts } from '@/lib/hosts';
import { isLocale, words } from '@/lib/i18n';

export default async function AppLayout({ children, params }: { children: React.ReactNode; params: Promise<{ locale: string }> }) {
  const { locale } = await params;
  if (!isLocale(locale)) notFound();
  const user = session((await cookies()).get(COOKIE)?.value);
  if (!user) redirect(`/${locale}/login`);
  const w = words(locale);
  const other = locale === 'en' ? 'ru' : 'en';
  return (
    <div className="min-h-screen">
      <header className="sticky top-0 z-20 flex h-14 items-center gap-4 border-b border-line bg-bg/90 px-4 backdrop-blur">
        <Link href={`/${locale}`} className="font-mono text-lg font-semibold text-fg">
          nos<span className="text-accent">_</span>
        </Link>
        <HostPicker locale={locale} hosts={hosts().map((h) => ({ id: h.id, name: h.name }))} label={w.hosts} />
        <nav className="ml-auto flex items-center gap-1 text-sm">
          <Link href={`/${locale}/hosts`} className="rounded-lg px-2 py-1 text-muted hover:bg-bg-soft hover:text-fg">{w.hosts}</Link>
          <Link href={`/${locale}/settings`} className="rounded-lg px-2 py-1 text-muted hover:bg-bg-soft hover:text-fg">{w.settings}</Link>
          <Link href={`/${other}`} hrefLang={other} className="rounded-lg px-2 py-1 font-mono text-muted hover:bg-bg-soft hover:text-fg">
            {other.toUpperCase()}
          </Link>
          <ThemeToggle label={w.theme} />
          <LogoutButton locale={locale} label={w.logout} />
        </nav>
      </header>
      {children}
    </div>
  );
}
