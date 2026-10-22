export const locales = ["en", "ru"] as const;
export type Locale = (typeof locales)[number];

// English is the fallback for a visitor whose browser asks for neither, and x-default.
export const defaultLocale: Locale = "en";

// Set by the language switcher; outranks Accept-Language once the visitor has chosen.
export const localeCookie = "NEXT_LOCALE";
export const localeCookieMaxAge = 60 * 60 * 24 * 365;

export const localeNames: Record<Locale, string> = { en: "English", ru: "Русский" };
export const localeShort: Record<Locale, string> = { en: "EN", ru: "RU" };
export const ogLocale: Record<Locale, string> = { en: "en_US", ru: "ru_RU" };
export const bcp47: Record<Locale, string> = { en: "en-US", ru: "ru-RU" };

export function isLocale(value: string | undefined | null): value is Locale {
    return value === "en" || value === "ru";
}

interface Preference {
    tag: string;
    q: number;
    index: number;
}

/*
 * The best supported locale for an Accept-Language header (RFC 9110, 12.5.4):
 * ranges in order of their q-values, the header's own order breaking ties, and
 * a range matching on its primary subtag ("ru-RU" and "ru" are both Russian).
 * null when nothing in the header is supported, so the caller decides the default.
 */
export function negotiateLocale(header: string | null): Locale | null {
    if (!header) {
        return null;
    }
    const preferences: Preference[] = [];
    header.split(",").forEach((part, index) => {
        const [range, ...params] = part.trim().split(";");
        const tag = (range ?? "").trim().toLowerCase();
        if (!tag) {
            return;
        }
        let q = 1;
        for (const param of params) {
            const [key, value] = param.trim().split("=");
            if (key?.trim() === "q") {
                const parsed = Number(value);
                q = Number.isFinite(parsed) ? parsed : 0;
            }
        }
        if (q > 0) {
            preferences.push({ tag, q, index });
        }
    });
    preferences.sort((a, b) => b.q - a.q || a.index - b.index);
    for (const { tag } of preferences) {
        const primary = tag.split("-")[0];
        if (isLocale(primary)) {
            return primary;
        }
    }
    return null;
}

// The same path in another locale: "/ru/docs/boot" -> "/en/docs/boot".
export function switchLocalePath(pathname: string, to: Locale): string {
    const parts = pathname.split("/");
    if (isLocale(parts[1])) {
        parts[1] = to;
        return parts.join("/") || `/${to}`;
    }
    return `/${to}${pathname === "/" ? "" : pathname}`;
}

export function formatDate(date: string, locale: Locale): string {
    return new Intl.DateTimeFormat(bcp47[locale], {
        year: "numeric",
        month: "long",
        day: "numeric",
        timeZone: "UTC",
    }).format(new Date(`${date}T00:00:00Z`));
}
