import type { Metadata } from "next";
import Link from "next/link";
import { ArticleView } from "@/components/ArticleView";
import { ArrowRightIcon } from "@/components/icons";
import { JsonLd } from "@/components/JsonLd";
import { entryLocales, entrySlugs, getEntries, getEntry } from "@/lib/content";
import { getDictionary, readingTime } from "@/lib/dictionaries";
import { formatDate, isLocale } from "@/lib/i18n";
import { pageLocale } from "@/lib/page-params";
import { articleLd, breadcrumbs, pageMetadata } from "@/lib/seo";

export const dynamicParams = false;

export function generateStaticParams({ params }: { params: { locale: string } }) {
    const locale = params.locale;
    if (!isLocale(locale)) {
        return [];
    }
    return entrySlugs("howto")
        .filter((slug) => entryLocales("howto", slug).includes(locale))
        .map((slug) => ({ slug }));
}

interface Props {
    params: Promise<{ locale: string; slug: string }>;
}

async function load(params: Props["params"]) {
    const locale = await pageLocale(params);
    const { slug } = await params;
    return { locale, slug, guide: await getEntry("howto", slug, locale) };
}

export async function generateMetadata({ params }: Props): Promise<Metadata> {
    const { locale, slug, guide } = await load(params);
    return pageMetadata({
        locale,
        path: `/howto/${slug}`,
        title: guide.title,
        description: guide.description,
        available: entryLocales("howto", slug),
    });
}

export default async function Guide({ params }: Props) {
    const { locale, slug, guide } = await load(params);
    const dict = getDictionary(locale);
    const guides = await getEntries("howto", locale);
    const index = guides.findIndex((g) => g.slug === slug);
    const next = index >= 0 ? guides[index + 1] : undefined;

    return (
        <>
            <JsonLd
                data={articleLd({
                    type: "TechArticle",
                    locale,
                    path: `/howto/${slug}`,
                    title: guide.title,
                    description: guide.description,
                    modified: guide.updated,
                    image: `/${locale}/opengraph-image`,
                })}
            />
            <JsonLd
                data={breadcrumbs([
                    { name: dict.nav.home, path: `/${locale}` },
                    { name: dict.howto.title, path: `/${locale}/howto` },
                    { name: guide.title, path: `/${locale}/howto/${slug}` },
                ])}
            />
            <ArticleView
                locale={locale}
                dict={dict}
                eyebrow={
                    <Link href={`/${locale}/howto`} className="hover:underline">
                        {dict.howto.title}
                    </Link>
                }
                title={guide.title}
                description={guide.description}
                html={guide.html}
                toc={guide.toc}
                meta={
                    <div className="mt-6 flex flex-wrap items-center gap-x-2 gap-y-1 font-mono text-xs text-muted">
                        <span>{readingTime(guide.minutes, locale)}</span>
                        {guide.updated && (
                            <>
                                <span aria-hidden="true">·</span>
                                <span>
                                    {dict.article.updated} <time dateTime={guide.updated}>{formatDate(guide.updated, locale)}</time>
                                </span>
                            </>
                        )}
                    </div>
                }
                footer={
                    next && (
                        <div className="mt-16 border-t border-line pt-8">
                            <Link href={`/${locale}/howto/${next.slug}`} className="group block rounded-xl border border-line p-4 hover:border-line-strong">
                                <span className="flex items-center gap-1.5 font-mono text-xs text-muted">
                                    {dict.howto.guide} {String(index + 2).padStart(2, "0")}
                                    <ArrowRightIcon width={14} height={14} />
                                </span>
                                <span className="mt-1.5 block font-medium group-hover:text-accent">{next.title}</span>
                            </Link>
                        </div>
                    )
                }
            />
        </>
    );
}
