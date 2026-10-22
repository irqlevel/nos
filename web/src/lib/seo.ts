import type { Metadata } from "next";
import { defaultLocale, locales, ogLocale, type Locale } from "./i18n";
import { absoluteUrl, site } from "./site";

export interface PageSeo {
    locale: Locale;
    // The path after the locale: "" for the home page, "/blog/x" for a post.
    path: string;
    title?: string;
    description: string;
    // The locales this page really exists in; a hreflang alternate is listed for each.
    available?: readonly Locale[];
    /*
     * Where the canonical copy lives. Only a Russian docs page that shows the
     * English text sets it, to the English page, so search engines index the
     * text once and in the language it is written in.
     */
    canonicalLocale?: Locale;
    // The Open Graph card's path, when the page has one of its own (a blog post does).
    image?: string;
    article?: {
        published?: string;
        modified?: string;
        tags?: string[];
    };
}

export function localePath(locale: Locale, path: string): string {
    return `/${locale}${path}`;
}

/*
 * Every page's metadata comes from here, so canonical, hreflang (with
 * x-default on the English page) and Open Graph stay consistent across the
 * site. metadataBase in the layout makes these paths absolute.
 */
export function pageMetadata(seo: PageSeo): Metadata {
    const available = seo.available ?? locales;
    const canonical = localePath(seo.canonicalLocale ?? seo.locale, seo.path);
    const own = seo.canonicalLocale === undefined || seo.canonicalLocale === seo.locale;

    const languages: Record<string, string> = {};
    if (own) {
        for (const locale of available) {
            languages[locale] = localePath(locale, seo.path);
        }
        if (available.includes(defaultLocale)) {
            languages["x-default"] = localePath(defaultLocale, seo.path);
        }
    }

    // A page without a card of its own shares the locale's.
    const images = [{ url: seo.image ?? localePath(seo.locale, "/opengraph-image"), width: 1200, height: 630, alt: seo.title ?? site.name }];

    const openGraph: Metadata["openGraph"] = {
        siteName: site.name,
        images,
        url: localePath(seo.locale, seo.path),
        title: seo.title,
        description: seo.description,
        locale: ogLocale[seo.locale],
        alternateLocale: available.filter((l) => l !== seo.locale).map((l) => ogLocale[l]),
        ...(seo.article
            ? {
                  type: "article",
                  publishedTime: seo.article.published,
                  modifiedTime: seo.article.modified ?? seo.article.published,
                  authors: [site.author],
                  tags: seo.article.tags,
              }
            : { type: "website" }),
    };

    return {
        title: seo.title,
        description: seo.description,
        alternates: {
            canonical,
            languages,
            types: { "application/rss+xml": localePath(seo.locale, "/feed.xml") },
        },
        openGraph,
        twitter: {
            card: "summary_large_image",
            title: seo.title,
            description: seo.description,
            images,
        },
    };
}

export function breadcrumbs(items: { name: string; path: string }[]) {
    return {
        "@context": "https://schema.org",
        "@type": "BreadcrumbList",
        itemListElement: items.map((item, i) => ({
            "@type": "ListItem",
            position: i + 1,
            name: item.name,
            item: absoluteUrl(item.path),
        })),
    };
}

export function articleLd(opts: {
    type: "BlogPosting" | "TechArticle";
    locale: Locale;
    path: string;
    title: string;
    description: string;
    published?: string;
    modified?: string;
    tags?: string[];
    // The Open Graph image's path: a post has its own, the rest share the locale's.
    image: string;
}) {
    const url = absoluteUrl(localePath(opts.locale, opts.path));
    return {
        "@context": "https://schema.org",
        "@type": opts.type,
        headline: opts.title,
        description: opts.description,
        inLanguage: opts.locale,
        url,
        mainEntityOfPage: url,
        image: absoluteUrl(opts.image),
        datePublished: opts.published,
        dateModified: opts.modified ?? opts.published,
        keywords: opts.tags?.join(", "),
        author: { "@type": "Person", name: site.author, url: site.github.replace(/\/nos$/, "") },
        publisher: { "@type": "Organization", name: site.name, url: absoluteUrl("/") },
        isPartOf: { "@type": "WebSite", name: site.name, url: absoluteUrl("/") },
    };
}
