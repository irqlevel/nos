import type { Metadata } from "next";
import Link from "next/link";
import { notFound } from "next/navigation";
import type { ReactNode } from "react";
import { BootTerminal } from "@/components/BootTerminal";
import { LinkCard, PostCard } from "@/components/cards";
import { Command } from "@/components/Command";
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

/*
 * How nos goes onto a server: the installer this site serves, which runs the
 * newest release's (public/install.sh), with the dialog or without it, and
 * the newest release's installer ISO.
 */
const installCommand = `curl -fsSL ${absoluteUrl("/install.sh")} | sh`;
const installQuietCommand = `${installCommand} -s -- disk /dev/sdX --wipe --network-here --ssh-key /root/.ssh/authorized_keys`;
const installerIso = `${site.releases}/latest/download/nos-installer-x86_64.iso`;

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
                            <a
                                href="#try"
                                className="inline-flex items-center rounded-lg border border-line-strong bg-card px-4 py-2.5 text-fg transition-colors hover:border-fg"
                            >
                                {home.ctaInstall}
                            </a>
                            <Link
                                href={`/${locale}/demo`}
                                className="inline-flex items-center gap-2 rounded-lg border border-line-strong bg-card px-4 py-2.5 text-fg transition-colors hover:border-fg"
                            >
                                <svg width="14" height="14" viewBox="0 0 24 24" fill="currentColor" aria-hidden="true">
                                    <path d="M7 4.5v15a1 1 0 0 0 1.53.85l12-7.5a1 1 0 0 0 0-1.7l-12-7.5A1 1 0 0 0 7 4.5z" />
                                </svg>
                                {home.ctaDemo}
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

            <Section id="try" title={home.try.title} lead={home.try.lead}>
                <div className="grid gap-4 lg:grid-cols-[1.4fr_1fr]">
                    <div className="min-w-0 rounded-xl border border-line bg-card p-5 sm:p-6">
                        <p className="font-mono text-xs font-semibold text-accent">{home.try.rescue.kicker}</p>
                        <h3 className="mt-2 text-lg font-semibold tracking-tight text-fg">{home.try.rescue.title}</h3>
                        <p className="mt-2 text-sm leading-relaxed text-fg-soft">{home.try.rescue.text}</p>
                        <div className="mt-3">
                            <Command command={installCommand} copy={home.try.copy} copied={home.try.copied} />
                        </div>
                        <p className="mt-4 text-sm leading-relaxed text-muted">{home.try.rescue.dialog}</p>
                        <p className="mt-4 text-sm leading-relaxed text-fg-soft">{home.try.rescue.quiet}</p>
                        <div className="mt-2">
                            <Command command={installQuietCommand} copy={home.try.copy} copied={home.try.copied} />
                        </div>
                        <p className="mt-4 text-sm leading-relaxed text-fg-soft">{home.try.rescue.then}</p>
                    </div>
                    <div className="grid min-w-0 content-start gap-4">
                        <div className="rounded-xl border border-line bg-card p-5 sm:p-6">
                            <p className="font-mono text-xs font-semibold text-accent">{home.try.iso.kicker}</p>
                            <h3 className="mt-2 text-lg font-semibold tracking-tight text-fg">{home.try.iso.title}</h3>
                            <p className="mt-2 text-sm leading-relaxed text-fg-soft">{home.try.iso.text}</p>
                            <a
                                href={installerIso}
                                className="mt-4 inline-flex max-w-full items-center gap-2 rounded-lg bg-fg px-4 py-2.5 text-sm font-medium text-bg transition-opacity hover:opacity-90"
                            >
                                <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
                                    <path d="M12 4v11m0 0-4.5-4.5M12 15l4.5-4.5M5 20h14" />
                                </svg>
                                {home.try.iso.link}
                            </a>
                            <p className="mt-2 break-all font-mono text-[0.72rem] text-muted">nos-installer-x86_64.iso</p>
                            <p className="mt-4 text-sm leading-relaxed text-muted">{home.try.iso.tryNos}</p>
                        </div>
                        <div className="rounded-xl border border-line bg-card p-5 sm:p-6">
                            <p className="font-mono text-xs font-semibold text-accent">{home.try.local.kicker}</p>
                            <h3 className="mt-2 text-lg font-semibold tracking-tight text-fg">{home.try.local.title}</h3>
                            <p className="mt-2 text-sm leading-relaxed text-fg-soft">{home.try.local.text}</p>
                            <div className="mt-4 flex flex-wrap gap-x-5 gap-y-2 text-sm font-medium">
                                <Link href={`/${locale}/howto/run-in-qemu`} className="inline-flex items-center gap-1.5 text-accent hover:underline">
                                    {home.ctaRun}
                                    <ArrowRightIcon width={15} height={15} />
                                </Link>
                                <Link href={`/${locale}/demo`} className="inline-flex items-center gap-1.5 text-accent hover:underline">
                                    {home.ctaDemo}
                                    <ArrowRightIcon width={15} height={15} />
                                </Link>
                            </div>
                        </div>
                    </div>
                </div>
                <p className="mt-6 max-w-3xl text-sm leading-relaxed text-muted">
                    {home.try.more}{" "}
                    <Link href={`/${locale}/howto/install-on-a-server`} className="inline-flex items-center gap-1 font-medium text-accent hover:underline">
                        {home.try.moreLink}
                        <ArrowRightIcon width={14} height={14} />
                    </Link>
                </p>
            </Section>

            <Section id="why" title={home.why.title} lead={home.why.lead}>
                <div className="grid gap-4 sm:grid-cols-3">
                    {home.why.stats.map((stat) => (
                        <div key={stat.value} className="rounded-xl border border-line bg-card p-5">
                            <div className="text-3xl font-bold tabular-nums tracking-tight text-fg">{stat.value}</div>
                            <p className="mt-2 text-sm leading-relaxed text-muted">{stat.label}</p>
                        </div>
                    ))}
                </div>
                <p className="mt-6 max-w-3xl leading-relaxed text-fg-soft">{home.why.ai}</p>
                <h3 className="mt-12 text-xl font-bold tracking-tight text-fg">{home.why.approachTitle}</h3>
                <div className="mt-5 grid gap-4 md:grid-cols-2">
                    {home.why.approach.map((item) => (
                        <div key={item.title} className="rounded-xl border border-line bg-card p-5">
                            <p className="font-mono text-xs font-semibold text-accent">{item.kicker}</p>
                            <h4 className="mt-2 font-semibold tracking-tight text-fg">{item.title}</h4>
                            <p className="mt-1.5 text-sm leading-relaxed text-muted">{item.text}</p>
                        </div>
                    ))}
                </div>
                <p className="mt-8 max-w-3xl text-lg font-medium leading-relaxed text-fg">{home.why.closing}</p>
                <p className="mt-6 text-xs leading-relaxed text-muted">
                    {home.why.sourcesTitle}:{" "}
                    {home.why.sources.map((source, i) => (
                        <span key={source.href}>
                            {i > 0 && " · "}
                            <a href={source.href} className="underline decoration-line-strong underline-offset-2 hover:text-accent">
                                {source.label}
                            </a>
                        </span>
                    ))}
                </p>
            </Section>

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
