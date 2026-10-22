import "@fontsource-variable/inter";
import "@fontsource-variable/jetbrains-mono";
import "./globals.css";
import Script from "next/script";
import type { Metadata } from "next";
import { themeScript } from "@/lib/theme";

export const metadata: Metadata = {
    title: "404 — nos",
    robots: { index: false },
};

// A URL outside both locales that no route matches: there is no locale to render it in, so both.
export default function GlobalNotFound() {
    return (
        <html lang="en" suppressHydrationWarning>
            <head>
                <Script id="theme" strategy="beforeInteractive">
                    {themeScript}
                </Script>
            </head>
            <body className="flex min-h-dvh items-center justify-center antialiased">
                <main className="mx-auto max-w-xl px-6 py-24">
                    <p className="font-mono text-lg font-bold">
                        nos<span className="cursor text-accent">_</span>
                    </p>
                    <h1 className="mt-6 text-3xl font-bold tracking-tight">Page not found</h1>
                    <p className="mt-2 text-muted" lang="ru">
                        Страница не найдена
                    </p>
                    <div className="mt-8 flex gap-3 text-sm">
                        <a href="/en" hrefLang="en" className="rounded-lg bg-fg px-4 py-2 font-medium text-bg">
                            English
                        </a>
                        <a href="/ru" hrefLang="ru" lang="ru" className="rounded-lg border border-line px-4 py-2 font-medium">
                            Русский
                        </a>
                    </div>
                </main>
            </body>
        </html>
    );
}
