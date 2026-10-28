'use client';

import Link from 'next/link';
import { usePathname } from 'next/navigation';

export function NavLink({ href, exact, children }: { href: string; exact?: boolean; children: React.ReactNode }) {
  const path = usePathname();
  const on = exact ? path === href : path === href || path.startsWith(href + '/');
  return (
    <Link href={href} aria-current={on ? 'page' : undefined}
      className={`block rounded-lg px-3 py-1.5 ${on ? 'bg-accent-soft font-medium text-fg' : 'text-muted hover:bg-bg-soft hover:text-fg'}`}>
      {children}
    </Link>
  );
}
