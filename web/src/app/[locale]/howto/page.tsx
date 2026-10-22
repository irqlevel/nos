import type { Metadata } from "next";
import Link from "next/link";
import { ArrowRightIcon } from "@/components/icons";
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
    return pageMetadata({ locale, path: "/howto", title: dict.howto.title, description: dict.howto.description });
}

export default async function HowTo({ params }: Props) {
    const locale = await pageLocale(params);
    const dict = getDictionary(locale);
    const guides = await getEntries("howto", locale);
    return (
        <>
            <PageHeader title={dict.howto.title} description={dict.howto.description} />
            <div className="mx-auto max-w-7xl px-4 py-10 sm:px-6">
                <ol className="grid gap-4 md:grid-cols-2">
                    {guides.map((guide, i) => (
                        <li key={guide.slug}>
                            <Link
                                href={`/${locale}/howto/${guide.slug}`}
                                className="group flex h-full gap-4 rounded-xl border border-line bg-card p-5 transition-colors hover:border-line-strong"
                            >
                                <span className="font-mono text-2xl font-bold text-line-strong group-hover:text-accent">{String(i + 1).padStart(2, "0")}</span>
                                <span className="flex flex-col">
                                    <span className="flex items-center gap-2 text-[1.05rem] font-semibold tracking-tight text-fg group-hover:text-accent">
                                        {guide.title}
                                        <ArrowRightIcon width={15} height={15} className="text-muted transition-transform group-hover:translate-x-0.5" />
                                    </span>
                                    <span className="mt-1.5 text-sm leading-relaxed text-muted">{guide.description}</span>
                                </span>
                            </Link>
                        </li>
                    ))}
                </ol>
            </div>
        </>
    );
}
