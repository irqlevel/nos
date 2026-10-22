import type { Metadata } from "next";
import { PostCard } from "@/components/cards";
import { RssIcon } from "@/components/icons";
import { PageHeader } from "@/components/PageHeader";
import { getEntries } from "@/lib/content";
import { getDictionary } from "@/lib/dictionaries";
import { pageLocale } from "@/lib/page-params";
import { pageMetadata } from "@/lib/seo";

interface Props {
    params: Promise<{ locale: string }>;
}

export async function generateMetadata({ params }: Props): Promise<Metadata> {
    const locale = await pageLocale(params);
    const dict = getDictionary(locale);
    return pageMetadata({ locale, path: "/blog", title: dict.blog.title, description: dict.blog.description });
}

export default async function Blog({ params }: Props) {
    const locale = await pageLocale(params);
    const dict = getDictionary(locale);
    const posts = await getEntries("blog", locale);
    return (
        <>
            <PageHeader title={dict.blog.title} description={dict.blog.description}>
                <a href={`/${locale}/feed.xml`} className="mt-6 inline-flex items-center gap-1.5 text-sm font-medium text-accent hover:underline">
                    <RssIcon width={15} height={15} />
                    {dict.blog.rss}
                </a>
            </PageHeader>
            <div className="mx-auto max-w-7xl px-4 py-10 sm:px-6">
                <div className="grid gap-4 md:grid-cols-2 lg:grid-cols-3">
                    {posts.map((post) => (
                        <PostCard key={post.slug} post={post} locale={locale} dict={dict} headingLevel={2} />
                    ))}
                </div>
            </div>
        </>
    );
}
