import { getEntries } from "@/lib/content";
import { getDictionary } from "@/lib/dictionaries";
import { isLocale, locales } from "@/lib/i18n";
import { absoluteUrl } from "@/lib/site";

export const dynamic = "force-static";
export const dynamicParams = false;

export function generateStaticParams() {
    return locales.map((locale) => ({ locale }));
}

function escapeXml(text: string): string {
    return text.replace(/[<>&'"]/g, (c) => ({ "<": "&lt;", ">": "&gt;", "&": "&amp;", "'": "&apos;", '"': "&quot;" })[c] ?? c);
}

function rfc822(date: string): string {
    return new Date(`${date}T00:00:00Z`).toUTCString();
}

// The blog of one language as RSS 2.0, newest first.
export async function GET(_request: Request, { params }: { params: Promise<{ locale: string }> }) {
    const { locale } = await params;
    if (!isLocale(locale)) {
        return new Response("Not found", { status: 404 });
    }
    const dict = getDictionary(locale);
    const posts = await getEntries("blog", locale);
    const items = posts.map((post) => {
        const url = absoluteUrl(`/${locale}/blog/${post.slug}`);
        const categories = post.tags.map((tag) => `      <category>${escapeXml(dict.blog.tags[tag] ?? tag)}</category>`);
        return [
            "    <item>",
            `      <title>${escapeXml(post.title)}</title>`,
            `      <link>${url}</link>`,
            `      <guid isPermaLink="true">${url}</guid>`,
            `      <pubDate>${rfc822(post.date ?? "1970-01-01")}</pubDate>`,
            `      <description>${escapeXml(post.description)}</description>`,
            ...categories,
            "    </item>",
        ].join("\n");
    });
    const newest = posts[0]?.date;
    const xml = [
        '<?xml version="1.0" encoding="UTF-8"?>',
        '<rss version="2.0" xmlns:atom="http://www.w3.org/2005/Atom">',
        "  <channel>",
        `    <title>${escapeXml(`nos — ${dict.blog.title}`)}</title>`,
        `    <link>${absoluteUrl(`/${locale}/blog`)}</link>`,
        `    <description>${escapeXml(dict.blog.description)}</description>`,
        `    <language>${locale}</language>`,
        `    <atom:link href="${absoluteUrl(`/${locale}/feed.xml`)}" rel="self" type="application/rss+xml"/>`,
        ...(newest ? [`    <lastBuildDate>${rfc822(newest)}</lastBuildDate>`] : []),
        ...items,
        "  </channel>",
        "</rss>",
        "",
    ].join("\n");
    return new Response(xml, {
        headers: { "Content-Type": "application/rss+xml; charset=utf-8" },
    });
}
