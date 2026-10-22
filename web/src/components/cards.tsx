import Link from "next/link";
import type { Entry } from "@/lib/content";
import { readingTime, type Dictionary } from "@/lib/dictionaries";
import { formatDate, type Locale } from "@/lib/i18n";
import { ArrowRightIcon } from "./icons";

export function Tag({ name, dict }: { name: string; dict: Dictionary }) {
    return <span className="rounded-md bg-bg-soft px-1.5 py-0.5 font-mono text-[0.7rem] text-muted">{dict.blog.tags[name] ?? name}</span>;
}

export function PostMeta({ post, locale }: { post: Entry; locale: Locale }) {
    return (
        <div className="flex flex-wrap items-center gap-x-2 gap-y-1 font-mono text-xs text-muted">
            {post.date && <time dateTime={post.date}>{formatDate(post.date, locale)}</time>}
            <span aria-hidden="true">·</span>
            <span>{readingTime(post.minutes, locale)}</span>
        </div>
    );
}

// A post in a list; the whole card is the link, through the title's stretched ::after.
export function PostCard({ post, locale, dict, headingLevel = 3 }: { post: Entry; locale: Locale; dict: Dictionary; headingLevel?: 2 | 3 }) {
    const Heading = headingLevel === 2 ? "h2" : "h3";
    return (
        <article className="group relative flex flex-col rounded-xl border border-line bg-card p-5 transition-colors hover:border-line-strong">
            <PostMeta post={post} locale={locale} />
            <Heading className="mt-3 text-[1.08rem] font-semibold leading-snug tracking-tight text-fg">
                <Link href={`/${locale}/blog/${post.slug}`} className="after:absolute after:inset-0 after:rounded-xl group-hover:text-accent">
                    {post.title}
                </Link>
            </Heading>
            <p className="mt-2 flex-1 text-sm leading-relaxed text-muted">{post.description}</p>
            <div className="mt-4 flex flex-wrap gap-1.5">
                {post.tags.map((tag) => (
                    <Tag key={tag} name={tag} dict={dict} />
                ))}
            </div>
        </article>
    );
}

export function LinkCard({ href, title, text, eyebrow }: { href: string; title: string; text: string; eyebrow?: string }) {
    return (
        <Link href={href} className="group flex flex-col rounded-xl border border-line bg-card p-5 transition-colors hover:border-line-strong">
            {eyebrow && <span className="font-mono text-[0.7rem] text-muted">{eyebrow}</span>}
            <span className="mt-1 flex items-center justify-between gap-3 font-semibold tracking-tight text-fg group-hover:text-accent">
                {title}
                <ArrowRightIcon className="shrink-0 text-muted transition-transform group-hover:translate-x-0.5 group-hover:text-accent" width={16} height={16} />
            </span>
            <span className="mt-2 text-sm leading-relaxed text-muted">{text}</span>
        </Link>
    );
}
