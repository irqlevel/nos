import Link from 'next/link';
import { notFound } from 'next/navigation';

import { NavLink } from '@/components/NavLink';
import { catalog } from '@/lib/catalog';
import { Descriptor } from '@/lib/descriptor';
import { host as hostOf } from '@/lib/hosts';
import { isLocale, text, words } from '@/lib/i18n';

export default async function HostLayout({ children, params }: { children: React.ReactNode;
  params: Promise<{ locale: string; host: string }> }) {
  const { locale, host } = await params;
  if (!isLocale(locale)) notFound();
  const h = hostOf(host);
  if (!h) notFound();
  const w = words(locale);
  let all: Descriptor[] = [];
  let why = '';
  try {
    all = await catalog(host);
  } catch (e) {
    why = e instanceof Error ? e.message : String(e);
  }
  const base = `/${locale}/h/${host}`;
  return (
    <div className="mx-auto flex max-w-[1400px] gap-6 px-4 py-6">
      <aside className="w-52 shrink-0">
        <nav className="sticky top-20 space-y-1 text-sm">
          <NavLink href={base} exact>{w.dashboard}</NavLink>
          {all.some((d) => d.provider === 'vm') && <NavLink href={`${base}/vms`}>{w.vms}</NavLink>}
          {all.filter((d) => d.provider !== 'vm').map((d) => (
            <NavLink key={d.provider} href={`${base}/p/${d.provider}`}>{text(d.title, locale)}</NavLink>
          ))}
          <NavLink href={`${base}/jobs`}>{w.jobs}</NavLink>
        </nav>
      </aside>
      <main className="min-w-0 flex-1">
        {why && (
          <div role="alert" className="mb-4 rounded-lg border border-bad/40 bg-bg-soft px-4 py-3 text-sm text-bad">
            {h.name}: {w.offline} -- {why}{' '}
            <Link href={`/${locale}/hosts`} className="underline">{w.hosts}</Link>
          </div>
        )}
        {children}
      </main>
    </div>
  );
}
