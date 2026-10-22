import type { Metadata } from "next";
import { ArticleView } from "@/components/ArticleView";
import { JsonLd } from "@/components/JsonLd";
import { getEntry } from "@/lib/content";
import { getDictionary } from "@/lib/dictionaries";
import { pageLocale } from "@/lib/page-params";
import { articleLd, breadcrumbs, pageMetadata } from "@/lib/seo";

interface Props {
    params: Promise<{ locale: string }>;
}

export async function generateMetadata({ params }: Props): Promise<Metadata> {
    const locale = await pageLocale(params);
    const page = await getEntry("pages", "hypervisor", locale);
    return pageMetadata({ locale, path: "/hypervisor", title: page.title, description: page.description });
}

export default async function Page({ params }: Props) {
    const locale = await pageLocale(params);
    const dict = getDictionary(locale);
    const page = await getEntry("pages", "hypervisor", locale);
    return (
        <>
            <JsonLd
                data={articleLd({
                    type: "TechArticle",
                    locale,
                    path: "/hypervisor",
                    title: page.title,
                    description: page.description,
                    modified: page.updated,
                    image: `/${locale}/opengraph-image`,
                })}
            />
            <JsonLd
                data={breadcrumbs([
                    { name: dict.nav.home, path: `/${locale}` },
                    { name: page.title, path: `/${locale}/hypervisor` },
                ])}
            />
            <ArticleView locale={locale} dict={dict} eyebrow={dict.nav.hypervisor} title={page.title} description={page.description} html={page.html} toc={page.toc} />
        </>
    );
}
