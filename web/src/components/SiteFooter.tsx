import Link from "next/link";
import type { Dictionary } from "@/lib/dictionaries";
import { localeNames, locales, type Locale } from "@/lib/i18n";
import { site } from "@/lib/site";
import { Wordmark } from "./SiteHeader";

function Column({ title, links }: { title: string; links: { href: string; label: string }[] }) {
    return (
        <div>
            <h2 className="font-mono text-xs font-semibold uppercase tracking-wider text-muted">{title}</h2>
            <ul className="mt-3 space-y-2 text-sm">
                {links.map((link) => (
                    <li key={link.href}>
                        <Link href={link.href} className="text-fg-soft transition-colors hover:text-accent">
                            {link.label}
                        </Link>
                    </li>
                ))}
            </ul>
        </div>
    );
}

export function SiteFooter({ locale, dict }: { locale: Locale; dict: Dictionary }) {
    const year = new Date().getUTCFullYear();
    return (
        <footer className="mt-24 border-t border-line bg-bg-soft/50">
            <div className="mx-auto grid max-w-7xl gap-10 px-4 py-12 sm:px-6 md:grid-cols-[1.6fr_1fr_1fr_1fr]">
                <div>
                    <Link href={`/${locale}`} className="text-lg text-fg">
                        <Wordmark />
                    </Link>
                    <p className="mt-3 max-w-xs text-sm leading-relaxed text-muted">{dict.footer.tagline}</p>
                </div>
                <Column
                    title={dict.footer.project}
                    links={[
                        { href: `/${locale}/about`, label: dict.nav.about },
                        { href: `/${locale}/hypervisor`, label: dict.nav.hypervisor },
                    ]}
                />
                <Column
                    title={dict.footer.learn}
                    links={[
                        { href: `/${locale}/docs`, label: dict.nav.docs },
                        { href: `/${locale}/howto`, label: dict.nav.howto },
                        { href: `/${locale}/blog`, label: dict.nav.blog },
                    ]}
                />
                <Column
                    title={dict.footer.source}
                    links={[
                        { href: site.github, label: "GitHub" },
                        { href: site.releases, label: dict.footer.releases },
                        { href: `/${locale}/feed.xml`, label: dict.footer.rss },
                    ]}
                />
            </div>
            <div className="border-t border-line">
                <div className="mx-auto flex max-w-7xl flex-wrap items-center justify-between gap-3 px-4 py-5 text-xs text-muted sm:px-6">
                    <p>
                        © {site.since}–{year} {site.author}. {dict.footer.license}
                    </p>
                    <p className="flex gap-3">
                        {locales.map((l) => (
                            <Link key={l} href={`/${l}`} hrefLang={l} lang={l} className={l === locale ? "text-fg" : "hover:text-fg"}>
                                {localeNames[l]}
                            </Link>
                        ))}
                    </p>
                </div>
            </div>
        </footer>
    );
}
