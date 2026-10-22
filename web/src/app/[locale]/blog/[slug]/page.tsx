import type { Metadata } from "next";
import Link from "next/link";
import { ArticleView } from "@/components/ArticleView";
import { PostMeta, Tag } from "@/components/cards";
import { ArrowLeftIcon, ArrowRightIcon } from "@/components/icons";
import { JsonLd } from "@/components/JsonLd";
import { entryLocales, entrySlugs, getEntries, getEntry } from "@/lib/content";
import { getDictionary } from "@/lib/dictionaries";
import { isLocale, type Locale } from "@/lib/i18n";
import { pageLocale } from "@/lib/page-params";
import { articleLd, breadcrumbs, pageMetadata } from "@/lib/seo";

export const dynamicParams = false;

export function generateStaticParams({ params }: { params: { locale: string } }) {
    const locale = params.locale;
    if (!isLocale(locale)) {
        return [];
    }
    return entrySlugs("blog")
        .filter((slug) => entryLocales("blog", slug).includes(locale))
        .map((slug) => ({ slug }));
}

interface Props {
    params: Promise<{ locale: string; slug: string }>;
}

async function load(params: Props["params"]) {
    const locale = await pageLocale(params);
    const { slug } = await params;
    return { locale, slug, post: await getEntry("blog", slug, locale) };
}

export async function generateMetadata({ params }: Props): Promise<Metadata> {
    const { locale, slug, post } = await load(params);
    return pageMetadata({
        locale,
        path: `/blog/${slug}`,
        title: post.title,
        description: post.description,
        available: entryLocales("blog", slug),
        image: `/${locale}/blog/${slug}/opengraph-image`,
        article: { published: post.date, modified: post.updated, tags: post.tags },
    });
}

export default async function Post({ params }: Props) {
    const { locale, slug, post } = await load(params);
    const dict = getDictionary(locale);
    const posts = await getEntries("blog", locale);
    const index = posts.findIndex((p) => p.slug === slug);
    const newer = index > 0 ? posts[index - 1] : undefined;
    const older = index >= 0 ? posts[index + 1] : undefined;
    const other: Locale = locale === "en" ? "ru" : "en";
    const translated = entryLocales("blog", slug).includes(other);

    return (
        <>
            <JsonLd
                data={articleLd({
                    type: "BlogPosting",
                    locale,
                    path: `/blog/${slug}`,
                    title: post.title,
                    description: post.description,
                    published: post.date,
                    modified: post.updated,
                    tags: post.tags,
                    image: `/${locale}/blog/${slug}/opengraph-image`,
                })}
            />
            <JsonLd
                data={breadcrumbs([
                    { name: dict.nav.home, path: `/${locale}` },
                    { name: dict.blog.title, path: `/${locale}/blog` },
                    { name: post.title, path: `/${locale}/blog/${slug}` },
                ])}
            />
            <ArticleView
                locale={locale}
                dict={dict}
                eyebrow={
                    <Link href={`/${locale}/blog`} className="hover:underline">
                        {dict.blog.title}
                    </Link>
                }
                title={post.title}
                description={post.description}
                html={post.html}
                toc={post.toc}
                meta={
                    <div className="mt-6 flex flex-wrap items-center gap-x-4 gap-y-3">
                        <PostMeta post={post} locale={locale} />
                        <div className="flex flex-wrap gap-1.5">
                            {post.tags.map((tag) => (
                                <Tag key={tag} name={tag} dict={dict} />
                            ))}
                        </div>
                        {translated && (
                            <Link href={`/${other}/blog/${slug}`} hrefLang={other} lang={other} className="font-mono text-xs text-accent hover:underline">
                                {dict.article.otherLanguage}
                            </Link>
                        )}
                    </div>
                }
                footer={
                    <nav className="mt-16 grid gap-4 border-t border-line pt-8 sm:grid-cols-2" aria-label={dict.blog.allPosts}>
                        {newer ? (
                            <Link href={`/${locale}/blog/${newer.slug}`} className="group rounded-xl border border-line p-4 hover:border-line-strong">
                                <span className="flex items-center gap-1.5 font-mono text-xs text-muted">
                                    <ArrowLeftIcon width={14} height={14} />
                                    {dict.blog.newer}
                                </span>
                                <span className="mt-1.5 block font-medium leading-snug group-hover:text-accent">{newer.title}</span>
                            </Link>
                        ) : (
                            <span />
                        )}
                        {older && (
                            <Link href={`/${locale}/blog/${older.slug}`} className="group rounded-xl border border-line p-4 text-right hover:border-line-strong">
                                <span className="flex items-center justify-end gap-1.5 font-mono text-xs text-muted">
                                    {dict.blog.older}
                                    <ArrowRightIcon width={14} height={14} />
                                </span>
                                <span className="mt-1.5 block font-medium leading-snug group-hover:text-accent">{older.title}</span>
                            </Link>
                        )}
                    </nav>
                }
            />
        </>
    );
}
