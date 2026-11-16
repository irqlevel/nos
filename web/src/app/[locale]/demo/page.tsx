import type { Metadata } from "next";
import { ArticleView } from "@/components/ArticleView";
import { JsonLd } from "@/components/JsonLd";
import { getEntry } from "@/lib/content";
import { getDictionary } from "@/lib/dictionaries";
import type { Locale } from "@/lib/i18n";
import { pageLocale } from "@/lib/page-params";
import { breadcrumbs, pageMetadata } from "@/lib/seo";
import { absoluteUrl } from "@/lib/site";

interface Props {
    params: Promise<{ locale: string }>;
}

// Each language's video: its voice and its subtitles are that language's, and so is the panel in it.
const video = (locale: Locale) => ({
    src: `/video/nos-demo-${locale}.mp4`,
    poster: `/video/nos-demo-${locale}.jpg`,
    card: `/video/nos-demo-${locale}-card.png`,
    seconds: SECONDS[locale],
});
// The videos' lengths, for the structured data: ffprobe's, rounded.
const SECONDS: Record<Locale, number> = { en: 178, ru: 176 };
const PUBLISHED = "2026-11-16";

export async function generateMetadata({ params }: Props): Promise<Metadata> {
    const locale = await pageLocale(params);
    const page = await getEntry("pages", "demo", locale);
    return pageMetadata({ locale, path: "/demo", title: page.title, description: page.description, image: video(locale).card });
}

export default async function Page({ params }: Props) {
    const locale = await pageLocale(params);
    const dict = getDictionary(locale);
    const page = await getEntry("pages", "demo", locale);
    const v = video(locale);
    return (
        <>
            <JsonLd
                data={{
                    "@context": "https://schema.org",
                    "@type": "VideoObject",
                    name: page.title,
                    description: page.description,
                    thumbnailUrl: absoluteUrl(v.poster),
                    contentUrl: absoluteUrl(v.src),
                    uploadDate: PUBLISHED,
                    duration: `PT${Math.floor(v.seconds / 60)}M${v.seconds % 60}S`,
                    inLanguage: locale,
                    publisher: { "@id": absoluteUrl("/#organization") },
                }}
            />
            <JsonLd
                data={breadcrumbs([
                    { name: dict.nav.home, path: `/${locale}` },
                    { name: page.title, path: `/${locale}/demo` },
                ])}
            />
            <ArticleView
                locale={locale}
                dict={dict}
                eyebrow={dict.nav.demo}
                title={page.title}
                description={page.description}
                html={page.html}
                toc={page.toc}
                media={
                    <video
                        className="aspect-video w-full rounded-xl border border-line bg-black"
                        width={1920}
                        height={1080}
                        controls
                        playsInline
                        preload="metadata"
                        poster={v.poster}
                    >
                        <source src={v.src} type="video/mp4" />
                        <a href={v.src}>{page.title}</a>
                    </video>
                }
            />
        </>
    );
}
