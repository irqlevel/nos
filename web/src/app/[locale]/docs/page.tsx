import type { Metadata } from "next";
import { LinkCard } from "@/components/cards";
import { PageHeader } from "@/components/PageHeader";
import { docSummaries } from "@/lib/content";
import { getDictionary } from "@/lib/dictionaries";
import { pageLocale } from "@/lib/page-params";
import { pageMetadata } from "@/lib/seo";

interface Props {
    params: Promise<{ locale: string }>;
}

export async function generateMetadata({ params }: Props): Promise<Metadata> {
    const locale = await pageLocale(params);
    const dict = getDictionary(locale);
    return pageMetadata({ locale, path: "/docs", title: dict.docs.title, description: dict.docs.description });
}

export default async function Docs({ params }: Props) {
    const locale = await pageLocale(params);
    const dict = getDictionary(locale);
    const groups = docSummaries(locale);
    return (
        <>
            <PageHeader title={dict.docs.title} description={dict.docs.description} />
            <div className="mx-auto max-w-7xl space-y-14 px-4 py-12 sm:px-6">
                {groups.map((g) => (
                    <section key={g.group} aria-labelledby={`group-${g.group}`}>
                        <h2 id={`group-${g.group}`} className="font-mono text-xs font-semibold uppercase tracking-wider text-muted">
                            {dict.docs.groups[g.group]}
                        </h2>
                        <div className="mt-4 grid gap-4 sm:grid-cols-2 lg:grid-cols-3">
                            {g.docs.map((doc) => (
                                <LinkCard key={doc.slug} href={`/${locale}/docs/${doc.slug}`} title={doc.title} text={doc.description} />
                            ))}
                        </div>
                    </section>
                ))}
            </div>
        </>
    );
}
