import type { Locale } from "@/lib/i18n";
import { CodeCopy } from "./CodeCopy";

const copyLabels: Record<Locale, { label: string; done: string }> = {
    en: { label: "Copy", done: "Copied" },
    ru: { label: "Копировать", done: "Скопировано" },
};

// Rendered Markdown. The HTML comes from this repository's own files at build time.
export function Prose({ html, locale, id = "content" }: { html: string; locale: Locale; id?: string }) {
    return (
        <>
            <div id={id} className="prose prose-nos max-w-none" dangerouslySetInnerHTML={{ __html: html }} />
            <CodeCopy containerId={id} {...copyLabels[locale]} />
        </>
    );
}
