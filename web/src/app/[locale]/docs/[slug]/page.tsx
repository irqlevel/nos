import type { Metadata } from "next";
import Link from "next/link";
import { DocsNav } from "@/components/DocsNav";
import { ExternalIcon } from "@/components/icons";
import { JsonLd } from "@/components/JsonLd";
import { Prose } from "@/components/Prose";
import { Toc } from "@/components/Toc";
import { docSlugs, docSummaries, getDoc, hasDocTranslation } from "@/lib/content";
import { getDictionary } from "@/lib/dictionaries";
import { locales } from "@/lib/i18n";
import { pageLocale } from "@/lib/page-params";
import { articleLd, breadcrumbs, pageMetadata } from "@/lib/seo";
import { site } from "@/lib/site";

export const dynamicParams = false;

export function generateStaticParams() {
    return docSlugs().map((slug) => ({ slug }));
}

interface Props {
    params: Promise<{ locale: string; slug: string }>;
}

async function load(params: Props["params"]) {
    const locale = await pageLocale(params);
    const { slug } = await params;
    return { locale, slug, doc: await getDoc(slug, locale) };
}

export async function generateMetadata({ params }: Props): Promise<Metadata> {
    const { locale, slug, doc } = await load(params);
    const dict = getDictionary(locale);
    return pageMetadata({
        locale,
        path: `/docs/${slug}`,
        title: `${doc.title} · ${dict.docs.title}`,
        description: doc.description,
        available: locales.filter((l) => hasDocTranslation(slug, l)),
        // A Russian frame around the English text points search engines at the English page.
        canonicalLocale: hasDocTranslation(slug, locale) ? locale : "en",
    });
}

export default async function DocPage({ params }: Props) {
    const { locale, slug, doc } = await load(params);
    const dict = getDictionary(locale);
    const groups = docSummaries(locale);
    const english = locale !== "en" && !doc.translated;
    // The longest documents have dozens of subsections; beside them, the sections are enough.
    const toc = doc.toc.length > 40 ? doc.toc.filter((item) => item.depth === 2) : doc.toc;

    return (
        <>
            <JsonLd
                data={articleLd({
                    type: "TechArticle",
                    locale: english ? "en" : locale,
                    path: `/docs/${slug}`,
                    title: doc.title,
                    description: doc.description,
                    image: `/${locale}/opengraph-image`,
                })}
            />
            <JsonLd
                data={breadcrumbs([
                    { name: dict.nav.home, path: `/${locale}` },
                    { name: dict.docs.title, path: `/${locale}/docs` },
                    { name: doc.title, path: `/${locale}/docs/${slug}` },
                ])}
            />
            <div className="mx-auto grid max-w-7xl gap-10 px-4 sm:px-6 lg:grid-cols-[13rem_minmax(0,1fr)] xl:grid-cols-[13rem_minmax(0,1fr)_13rem]">
                <aside className="hidden lg:block">
                    <div className="sticky top-14 max-h-[calc(100dvh-3.5rem)] overflow-y-auto py-10 pr-2">
                        <DocsNav groups={groups} current={slug} locale={locale} dict={dict} />
                    </div>
                </aside>
                <article className="min-w-0 py-10">
                    <details className="mb-8 rounded-lg border border-line bg-card lg:hidden">
                        <summary className="cursor-pointer px-4 py-2.5 text-sm font-medium">{dict.docs.allDocs}</summary>
                        <div className="border-t border-line p-4">
                            <DocsNav groups={groups} current={slug} locale={locale} dict={dict} />
                        </div>
                    </details>
                    <p className="font-mono text-xs font-medium uppercase tracking-wider text-accent">{dict.docs.groups[doc.group]}</p>
                    <h1 className="mt-2 text-3xl font-bold tracking-tight text-fg sm:text-[2.4rem]">{doc.title}</h1>
                    <a
                        href={`${site.repoBlob}docs/${slug}.md`}
                        className="mt-3 inline-flex items-center gap-1.5 font-mono text-xs text-muted hover:text-accent"
                    >
                        docs/{slug}.md
                        <ExternalIcon width={13} height={13} />
                        <span className="sr-only">{dict.docs.viewSource}</span>
                    </a>
                    {english && (
                        <p className="mt-6 max-w-3xl rounded-lg border border-line bg-accent-soft px-4 py-3 text-sm leading-relaxed text-fg-soft">
                            {dict.docs.notice}{" "}
                            <Link href={`/${locale}/howto`} className="font-medium text-accent underline underline-offset-2">
                                {dict.docs.noticeLink}
                            </Link>
                            .
                        </p>
                    )}
                    <div className="mt-8 max-w-3xl" lang={english ? "en" : undefined}>
                        <Prose html={doc.html} locale={locale} />
                    </div>
                </article>
                {toc.length > 0 && (
                    <aside className="hidden xl:block">
                        <div className="sticky top-14 max-h-[calc(100dvh-3.5rem)] overflow-y-auto py-10">
                            <Toc items={toc} title={dict.article.onThisPage} />
                        </div>
                    </aside>
                )}
            </div>
        </>
    );
}
