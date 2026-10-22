import { entryLocales, entrySlugs, getEntry } from "@/lib/content";
import { getDictionary } from "@/lib/dictionaries";
import { formatDate, isLocale, locales } from "@/lib/i18n";
import { ogImage, ogSize } from "@/lib/og";

export const size = ogSize;
export const contentType = "image/png";
export const dynamicParams = false;
export const alt = "nos blog";

// Drawn at build time, one per post and locale: the server never reads a font or a post for it.
export function generateStaticParams() {
    return locales.flatMap((locale) =>
        entrySlugs("blog")
            .filter((slug) => entryLocales("blog", slug).includes(locale))
            .map((slug) => ({ locale, slug }))
    );
}

export default async function Image({ params }: { params: Promise<{ locale: string; slug: string }> }) {
    const { locale: value, slug } = await params;
    const locale = isLocale(value) ? value : "en";
    const dict = getDictionary(locale);
    const post = await getEntry("blog", slug, locale);
    return ogImage({
        eyebrow: dict.blog.title,
        title: post.title,
        footer: post.date ? formatDate(post.date, locale) : dict.footer.tagline,
    });
}
