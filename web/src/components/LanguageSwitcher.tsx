"use client";

import Link from "next/link";
import { usePathname } from "next/navigation";
import {
    localeCookie,
    localeCookieMaxAge,
    localeNames,
    locales,
    localeShort,
    switchLocalePath,
    type Locale,
} from "@/lib/i18n";

// The choice outlives the visit: the proxy reads it before Accept-Language.
function remember(locale: Locale) {
    document.cookie = `${localeCookie}=${locale}; path=/; max-age=${localeCookieMaxAge}; samesite=lax`;
}

export function LanguageSwitcher({ locale, label }: { locale: Locale; label: string }) {
    const pathname = usePathname() || `/${locale}`;
    return (
        <div role="group" aria-label={label} className="flex items-center rounded-lg border border-line p-0.5 font-mono text-[0.72rem]">
            {locales.map((l) =>
                l === locale ? (
                    <span key={l} aria-current="true" title={localeNames[l]} className="rounded-md bg-fg px-1.5 py-0.5 font-semibold text-bg">
                        {localeShort[l]}
                    </span>
                ) : (
                    <Link
                        key={l}
                        href={switchLocalePath(pathname, l)}
                        hrefLang={l}
                        lang={l}
                        title={localeNames[l]}
                        onClick={() => remember(l)}
                        className="rounded-md px-1.5 py-0.5 text-muted transition-colors hover:text-fg"
                    >
                        {localeShort[l]}
                    </Link>
                )
            )}
        </div>
    );
}
