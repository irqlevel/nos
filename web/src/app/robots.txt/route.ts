import { site } from "@/lib/site";

export const dynamic = "force-static";

/*
 * Everything is open to crawlers; there is nothing private on the site. The
 * Yandex group repeats the rules because Yandex reads only the group naming
 * it, and adds Clean-param so links tagged for analytics are not indexed as
 * pages of their own. (Host: is gone from Yandex since 2018; the canonical
 * links and the www redirect do its job.)
 */
export function GET() {
    const body = [
        "User-agent: *",
        "Allow: /",
        "",
        "User-agent: Yandex",
        "Allow: /",
        "Clean-param: utm_source&utm_medium&utm_campaign&utm_content&utm_term&yclid&gclid&fbclid",
        "",
        `Sitemap: ${site.url}/sitemap.xml`,
        "",
    ].join("\n");
    return new Response(body, {
        headers: { "Content-Type": "text/plain; charset=utf-8" },
    });
}
