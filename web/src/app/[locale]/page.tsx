import type { Metadata } from "next";
import Link from "next/link";
import { notFound } from "next/navigation";
import type { ReactNode } from "react";
import { BootTerminal } from "@/components/BootTerminal";
import { LinkCard, PostCard } from "@/components/cards";
import { ArrowRightIcon, GitHubIcon } from "@/components/icons";
import { JsonLd } from "@/components/JsonLd";
import { getEntries } from "@/lib/content";
import { getDictionary, type StageStatus } from "@/lib/dictionaries";
import { isLocale } from "@/lib/i18n";
import { organizationLd, pageMetadata } from "@/lib/seo";
import { absoluteUrl, site } from "@/lib/site";

interface Props {
    params: Promise<{ locale: string }>;
}

export async function generateMetadata({ params }: Props): Promise<Metadata> {
    const { locale } = await params;
    if (!isLocale(locale)) {
        return {};
    }
    const dict = getDictionary(locale);
    return {
        ...pageMetadata({ locale, path: "", description: dict.meta.description }),
        title: { absolute: dict.meta.title },
    };
}

function Section({ id, title, lead, children, aside }: { id: string; title: string; lead?: string; children: ReactNode; aside?: ReactNode }) {
    return (
        <section aria-labelledby={id} className="mx-auto max-w-7xl px-4 pt-20 sm:px-6">
            <div className="flex flex-wrap items-end justify-between gap-4">
                <div className="max-w-2xl">
                    <h2 id={id} className="text-2xl font-bold tracking-tight text-fg sm:text-3xl">
                        {title}
                    </h2>
                    {lead && <p className="mt-3 leading-relaxed text-muted">{lead}</p>}
                </div>
                {aside}
            </div>
            <div className="mt-8">{children}</div>
        </section>
    );
}

const statusStyle: Record<StageStatus, string> = {
    done: "border-ok/40 text-ok",
    ongoing: "border-info/40 text-info",
    progress: "border-accent/50 text-accent",
    planned: "border-line-strong text-muted",
};

export default async function Home({ params }: Props) {
    const { locale } = await params;
    if (!isLocale(locale)) {
        notFound();
    }
    const dict = getDictionary(locale);
    const home = dict.home;
    const posts = (await getEntries("blog", locale)).slice(0, 3);

    const structured = {
        "@context": "https://schema.org",
        "@graph": [
            organizationLd(),
            {
                "@type": "WebSite",
                "@id": absoluteUrl("/#website"),
                url: absoluteUrl(`/${locale}`),
                name: site.name,
                description: dict.meta.description,
                inLanguage: locale,
                publisher: { "@id": absoluteUrl("/#organization") },
            },
            {
                "@type": "SoftwareSourceCode",
                name: site.name,
                description: dict.meta.description,
                codeRepository: site.github,
                programmingLanguage: ["C++", "Rust", "Assembly"],
                license: "https://opensource.org/licenses/MIT",
                author: { "@type": "Person", name: site.author, url: "https://github.com/irqlevel" },
            },
        ],
    };

    return (
        <>
            <JsonLd data={structured} />

            <section className="relative overflow-hidden border-b border-line">
                <div className="grid-bg pointer-events-none absolute inset-0 opacity-70" aria-hidden="true" />
                <div className="relative mx-auto grid max-w-7xl items-center gap-12 px-4 py-14 sm:px-6 lg:grid-cols-[1.02fr_1fr] lg:py-24">
                    <div className="min-w-0">
                        <p className="font-mono text-xs font-medium tracking-wider text-accent">{home.eyebrow}</p>
                        <h1 className="mt-4 text-[2.35rem] font-bold leading-[1.08] tracking-tight text-fg sm:text-5xl lg:text-[3.3rem]">{home.title}</h1>
                        <p className="mt-6 max-w-xl text-[1.07rem] leading-relaxed text-fg-soft">{home.lead}</p>
                        <div className="mt-8 flex flex-wrap gap-3 text-sm font-medium">
                            <Link href={`/${locale}/about`} className="inline-flex items-center gap-2 rounded-lg bg-fg px-4 py-2.5 text-bg transition-opacity hover:opacity-90">
                                {home.ctaAbout}
                                <ArrowRightIcon width={16} height={16} />
                            </Link>
                            <Link
                                href={`/${locale}/howto/run-in-qemu`}
                                className="inline-flex items-center rounded-lg border border-line-strong bg-card px-4 py-2.5 text-fg transition-colors hover:border-fg"
                            >
                                {home.ctaRun}
                            </Link>
                            <a href={site.github} className="inline-flex items-center gap-2 rounded-lg px-3 py-2.5 text-muted transition-colors hover:text-fg">
                                <GitHubIcon width={16} height={16} />
                                {home.ctaGithub}
                            </a>
                        </div>
                    </div>
                    <BootTerminal caption={home.terminalCaption} />
                </div>
            </section>

            <Section id="inside" title={home.highlightsTitle} lead={home.highlightsLead}>
                <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-4">
                    {home.highlights.map((card) => (
                        <LinkCard key={card.title} href={`/${locale}${card.href}`} title={card.title} text={card.text} />
                    ))}
                </div>
            </Section>

            <Section id="platforms" title={home.platformsTitle} lead={home.platformsLead}>
                <div className="grid gap-4 md:grid-cols-2">
                    {[
                        { label: home.virtual, items: home.platformsVirtual },
                        { label: home.metal, items: home.platformsMetal },
                    ].map((group) => (
                        <div key={group.label} className="rounded-xl border border-line bg-card p-5">
                            <h3 className="font-mono text-xs font-semibold uppercase tracking-wider text-muted">{group.label}</h3>
                            <ul className="mt-4 divide-y divide-line">
                                {group.items.map((item) => (
                                    <li key={item.name} className="flex flex-wrap items-baseline justify-between gap-x-4 gap-y-1 py-2.5">
                                        <span className="font-medium text-fg">{item.name}</span>
                                        <span className="text-sm text-muted">{item.note}</span>
                                    </li>
                                ))}
                            </ul>
                        </div>
                    ))}
                </div>
            </Section>

            <Section id="roadmap" title={home.roadmapTitle} lead={home.roadmapLead}>
                <ol className="grid gap-3 sm:grid-cols-2 lg:grid-cols-3">
                    {home.stages.map((stage, i) => (
                        <li key={stage.title} className="rounded-xl border border-line bg-card p-5">
                            <div className="flex items-center justify-between gap-3">
                                <span className="font-mono text-sm font-semibold text-muted">{`0${i}`}</span>
                                <span className={`rounded-full border px-2 py-0.5 font-mono text-[0.68rem] font-medium ${statusStyle[stage.status]}`}>
                                    {home.status[stage.status]}
                                </span>
                            </div>
                            <h3 className="mt-3 font-semibold tracking-tight text-fg">{stage.title}</h3>
                            <p className="mt-1.5 text-sm leading-relaxed text-muted">{stage.text}</p>
                        </li>
                    ))}
                </ol>
            </Section>

            {posts.length > 0 && (
                <Section
                    id="blog"
                    title={home.blogTitle}
                    aside={
                        <Link href={`/${locale}/blog`} className="inline-flex items-center gap-1.5 text-sm font-medium text-accent hover:underline">
                            {home.blogAll}
                            <ArrowRightIcon width={15} height={15} />
                        </Link>
                    }
                >
                    <div className="grid gap-4 md:grid-cols-3">
                        {posts.map((post) => (
                            <PostCard key={post.slug} post={post} locale={locale} dict={dict} />
                        ))}
                    </div>
                </Section>
            )}

            <Section id="learn" title={home.learnTitle}>
                <div className="grid gap-4 md:grid-cols-2">
                    {home.learn.map((card) => (
                        <LinkCard key={card.title} href={`/${locale}${card.href}`} title={card.title} text={card.text} />
                    ))}
                </div>
            </Section>
        </>
    );
}
