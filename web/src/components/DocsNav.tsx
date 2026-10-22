import Link from "next/link";
import type { DocSummary } from "@/lib/content";
import type { DocGroup } from "@/lib/docs-manifest";
import type { Dictionary } from "@/lib/dictionaries";
import type { Locale } from "@/lib/i18n";

export function DocsNav({
    groups,
    current,
    locale,
    dict,
}: {
    groups: { group: DocGroup; docs: DocSummary[] }[];
    current?: string;
    locale: Locale;
    dict: Dictionary;
}) {
    return (
        <nav aria-label={dict.docs.title} className="text-sm">
            <Link href={`/${locale}/docs`} className="font-semibold text-fg hover:text-accent">
                {dict.docs.allDocs}
            </Link>
            {groups.map((g) => (
                <div key={g.group} className="mt-6">
                    <p className="font-mono text-[0.7rem] font-semibold uppercase tracking-wider text-muted">{dict.docs.groups[g.group]}</p>
                    <ul className="mt-2 space-y-0.5 border-l border-line">
                        {g.docs.map((doc) => (
                            <li key={doc.slug}>
                                <Link
                                    href={`/${locale}/docs/${doc.slug}`}
                                    aria-current={doc.slug === current ? "page" : undefined}
                                    className="-ml-px block border-l border-transparent py-1 pl-3 leading-snug text-muted transition-colors hover:text-fg aria-[current=page]:border-accent aria-[current=page]:font-medium aria-[current=page]:text-accent"
                                >
                                    {doc.title}
                                </Link>
                            </li>
                        ))}
                    </ul>
                </div>
            ))}
        </nav>
    );
}
