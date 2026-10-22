import type { ReactNode } from "react";
import type { Dictionary } from "@/lib/dictionaries";
import type { Locale } from "@/lib/i18n";
import type { TocItem } from "@/lib/markdown";
import { PageHeader } from "./PageHeader";
import { Prose } from "./Prose";
import { Toc } from "./Toc";

// A long page: its header, the text, and the table of contents beside it on a wide screen.
export function ArticleView({
    locale,
    dict,
    eyebrow,
    title,
    description,
    meta,
    html,
    toc,
    footer,
}: {
    locale: Locale;
    dict: Dictionary;
    eyebrow?: ReactNode;
    title: string;
    description?: string;
    meta?: ReactNode;
    html: string;
    toc: TocItem[];
    footer?: ReactNode;
}) {
    return (
        <article>
            <PageHeader eyebrow={eyebrow} title={title} description={description}>
                {meta}
            </PageHeader>
            <div className="mx-auto grid max-w-7xl gap-12 px-4 py-10 sm:px-6 xl:grid-cols-[minmax(0,1fr)_14rem]">
                <div className="min-w-0 max-w-3xl">
                    <Prose html={html} locale={locale} />
                    {footer}
                </div>
                {toc.length > 0 && (
                    <aside className="hidden xl:block">
                        <div className="sticky top-20 max-h-[calc(100dvh-6rem)] overflow-y-auto pb-10">
                            <Toc items={toc} title={dict.article.onThisPage} />
                        </div>
                    </aside>
                )}
            </div>
        </article>
    );
}
