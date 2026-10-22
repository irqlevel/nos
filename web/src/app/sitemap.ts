import type { MetadataRoute } from "next";
import { assertTranslations, docSlugs, entrySlugs, getEntry, hasDocTranslation, type Kind } from "@/lib/content";
import { defaultLocale, locales, type Locale } from "@/lib/i18n";
import { absoluteUrl } from "@/lib/site";

type Entry = MetadataRoute.Sitemap[number];

/*
 * One <url> per page and locale, each listing every language the page exists
 * in (and x-default, the English one) -- the form both Google and Yandex read
 * hreflang from. A Russian docs page that only frames the English text is left
 * out: its canonical is the English page.
 */
function page(path: string, available: readonly Locale[], extra: Partial<Entry> = {}): Entry[] {
    const languages: Record<string, string> = {};
    for (const locale of available) {
        languages[locale] = absoluteUrl(`/${locale}${path}`);
    }
    if (available.includes(defaultLocale)) {
        languages["x-default"] = absoluteUrl(`/${defaultLocale}${path}`);
    }
    return available.map((locale) => ({
        url: absoluteUrl(`/${locale}${path}`),
        alternates: { languages },
        ...extra,
    }));
}

async function entries(kind: Kind, section: string, priority: number): Promise<Entry[]> {
    const out: Entry[] = [];
    for (const slug of entrySlugs(kind)) {
        const en = await getEntry(kind, slug, "en");
        const lastModified = en.updated ?? en.date;
        out.push(...page(`/${section}/${slug}`.replace(/\/$/, ""), locales, { priority, ...(lastModified ? { lastModified } : {}) }));
    }
    return out;
}

export default async function sitemap(): Promise<MetadataRoute.Sitemap> {
    assertTranslations();
    const docs: Entry[] = [];
    for (const slug of docSlugs()) {
        docs.push(...page(`/docs/${slug}`, locales.filter((l) => hasDocTranslation(slug, l)), { priority: 0.6 }));
    }
    return [
        ...page("", locales, { priority: 1 }),
        ...page("/about", locales, { priority: 0.9 }),
        ...page("/hypervisor", locales, { priority: 0.9 }),
        ...page("/docs", locales, { priority: 0.8 }),
        ...page("/howto", locales, { priority: 0.8 }),
        ...page("/blog", locales, { priority: 0.8 }),
        ...(await entries("howto", "howto", 0.7)),
        ...(await entries("blog", "blog", 0.7)),
        ...docs,
    ];
}
