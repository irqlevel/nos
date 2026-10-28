import '@fontsource-variable/inter';
import '@fontsource-variable/jetbrains-mono';
import '../globals.css';

import type { Metadata } from 'next';
import { headers } from 'next/headers';
import { notFound } from 'next/navigation';

import { isLocale, words } from '@/lib/i18n';
import { themeScript } from '@/lib/theme';

export async function generateMetadata({ params }: { params: Promise<{ locale: string }> }): Promise<Metadata> {
  const { locale } = await params;
  return { title: isLocale(locale) ? words(locale).appName : 'nos', robots: { index: false, follow: false } };
}

export default async function RootLayout({ children, params }: { children: React.ReactNode; params: Promise<{ locale: string }> }) {
  const { locale } = await params;
  if (!isLocale(locale)) {
    notFound();
  }
  const nonce = (await headers()).get('x-nonce') ?? undefined;
  return (
    <html lang={locale} suppressHydrationWarning>
      <head>
        <script nonce={nonce} dangerouslySetInnerHTML={{ __html: themeScript }} />
      </head>
      <body className="min-h-screen antialiased">{children}</body>
    </html>
  );
}
