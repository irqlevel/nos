import Link from "next/link";
import type { Dictionary } from "@/lib/dictionaries";
import type { Locale } from "@/lib/i18n";
import { site } from "@/lib/site";
import { GitHubIcon } from "./icons";
import { LanguageSwitcher } from "./LanguageSwitcher";
import { DesktopNav, MobileNav, type NavItem } from "./SiteNav";
import { ThemeToggle } from "./ThemeToggle";

export function Wordmark({ className = "" }: { className?: string }) {
    return (
        <span className={`font-mono font-bold tracking-tight ${className}`}>
            nos<span className="cursor text-accent">_</span>
        </span>
    );
}

export function SiteHeader({ locale, dict }: { locale: Locale; dict: Dictionary }) {
    const items: NavItem[] = [
        { href: `/${locale}/about`, label: dict.nav.about },
        { href: `/${locale}/hypervisor`, label: dict.nav.hypervisor },
        { href: `/${locale}/docs`, label: dict.nav.docs },
        { href: `/${locale}/howto`, label: dict.nav.howto },
        { href: `/${locale}/blog`, label: dict.nav.blog },
    ];
    return (
        <header className="sticky top-0 z-40 border-b border-line bg-bg/85 backdrop-blur-md">
            <div className="relative mx-auto flex h-14 max-w-7xl items-center gap-5 px-4 sm:px-6">
                <Link href={`/${locale}`} aria-label={`nos — ${dict.nav.home}`} className="text-[1.1rem] text-fg">
                    <Wordmark />
                </Link>
                <DesktopNav items={items} label={dict.nav.menu} />
                <div className="ml-auto flex items-center gap-1.5">
                    <LanguageSwitcher locale={locale} label={dict.nav.language} />
                    <ThemeToggle label={dict.nav.theme} />
                    <a
                        href={site.github}
                        aria-label={dict.nav.github}
                        title={dict.nav.github}
                        className="hidden size-8 items-center justify-center rounded-lg text-muted transition-colors hover:bg-bg-soft hover:text-fg sm:inline-flex"
                    >
                        <GitHubIcon />
                    </a>
                    <MobileNav items={[{ href: `/${locale}`, label: dict.nav.home }, ...items, { href: site.github, label: dict.nav.github }]} label={dict.nav.menu} />
                </div>
            </div>
        </header>
    );
}
