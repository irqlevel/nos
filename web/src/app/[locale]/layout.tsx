import "@fontsource-variable/inter";
import "@fontsource-variable/jetbrains-mono";
import "../globals.css";
import Script from "next/script";
import type { Metadata, Viewport } from "next";
import { notFound } from "next/navigation";
import type { ReactNode } from "react";
import { SiteFooter } from "@/components/SiteFooter";
import { SiteHeader } from "@/components/SiteHeader";
import { getDictionary } from "@/lib/dictionaries";
import { isLocale, locales } from "@/lib/i18n";
import { MetrikaHits } from "@/components/MetrikaHits";
import { metrikaScript } from "@/lib/metrika";
import { site } from "@/lib/site";
import { themeScript } from "@/lib/theme";

// Only /en and /ru exist; anything else in the first segment is a 404 before rendering.
export const dynamicParams = false;

export function generateStaticParams() {
    return locales.map((locale) => ({ locale }));
}

interface Props {
    children: ReactNode;
    params: Promise<{ locale: string }>;
}

export async function generateMetadata({ params }: Omit<Props, "children">): Promise<Metadata> {
    const { locale } = await params;
    if (!isLocale(locale)) {
        return {};
    }
    const dict = getDictionary(locale);
    return {
        metadataBase: new URL(site.url),
        title: { default: dict.meta.title, template: "%s — nos" },
        description: dict.meta.description,
        applicationName: site.name,
        authors: [{ name: site.author, url: "https://github.com/irqlevel" }],
        creator: site.author,
        robots: {
            index: true,
            follow: true,
            googleBot: { index: true, follow: true, "max-image-preview": "large", "max-snippet": -1 },
        },
        verification: {
            google: site.verification.google,
            yandex: site.verification.yandex,
        },
        formatDetection: { telephone: false, email: false, address: false },
    };
}

export const viewport: Viewport = {
    colorScheme: "light dark",
    themeColor: [
        { media: "(prefers-color-scheme: light)", color: "#fbfaf7" },
        { media: "(prefers-color-scheme: dark)", color: "#0d0e11" },
    ],
};

export default async function LocaleLayout({ children, params }: Props) {
    const { locale } = await params;
    if (!isLocale(locale)) {
        notFound();
    }
    const dict = getDictionary(locale);
    return (
        <html lang={locale} suppressHydrationWarning>
            <head>
                <Script id="theme" strategy="beforeInteractive">
                    {themeScript}
                </Script>
                {/* As early in the page as it can be, so a visit that ends at once is still counted. */}
                {site.metrika && (
                    <Script id="metrika" strategy="beforeInteractive">
                        {metrikaScript(site.metrika)}
                    </Script>
                )}
            </head>
            <body className="flex min-h-dvh flex-col antialiased">
                {site.metrika && (
                    <noscript>
                        <div>
                            <img src={`https://mc.yandex.ru/watch/${site.metrika}`} style={{ position: "absolute", left: "-9999px" }} alt="" />
                        </div>
                    </noscript>
                )}
                <a href="#main" className="skip-link">
                    {dict.nav.skip}
                </a>
                <SiteHeader locale={locale} dict={dict} />
                <main id="main" className="flex-1">
                    {children}
                </main>
                <SiteFooter locale={locale} dict={dict} />
                {site.metrika && <MetrikaHits id={site.metrika} />}
            </body>
        </html>
    );
}
