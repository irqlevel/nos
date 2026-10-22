import fs from "node:fs";
import path from "node:path";
import { parse as parseYaml } from "yaml";
import { docGroups, docsManifest, type DocGroup } from "./docs-manifest";
import { locales, type Locale } from "./i18n";
import { renderMarkdown, type TocItem } from "./markdown";

/*
 * Everything the site shows is read from Markdown at build time: the blog,
 * the how-tos and the long pages from web/content/<kind>/<slug>/<locale>.md,
 * the documentation straight from the repository's docs/ -- so the site's docs
 * are the repository's, never a copy that drifts. In Docker the repository's
 * docs/ is copied next to web/, which keeps "../docs" true there as well.
 */
const root = process.cwd();
const contentDir = path.join(root, "content");
const docsDir = process.env.NOS_DOCS_DIR ? path.resolve(process.env.NOS_DOCS_DIR) : path.join(root, "..", "docs");
const docsRuDir = path.join(contentDir, "docs-ru");

export type Kind = "blog" | "howto" | "pages";

export const tagNames = [
    "hardware",
    "networking",
    "drivers",
    "hypervisor",
    "rust",
    "memory",
    "safety",
    "debugging",
    "testing",
    "fuzzing",
    "qemu",
    "arm64",
    "x86-64",
    "storage",
    "boot",
    "deployment",
    "performance",
    "modules",
] as const;

const allowedKeys: Record<Kind, readonly string[]> = {
    blog: ["title", "description", "date", "updated", "tags"],
    howto: ["title", "description", "order", "updated"],
    pages: ["title", "description", "updated"],
};

export interface Entry {
    kind: Kind;
    slug: string;
    locale: Locale;
    title: string;
    description: string;
    date?: string;
    updated?: string;
    tags: string[];
    order: number;
    minutes: number;
    html: string;
    toc: TocItem[];
}

export interface Doc {
    slug: string;
    locale: Locale;
    // A translation in content/docs-ru, rather than the English page under a Russian frame.
    translated: boolean;
    group: DocGroup;
    title: string;
    description: string;
    html: string;
    toc: TocItem[];
}

export interface DocSummary {
    slug: string;
    group: DocGroup;
    title: string;
    description: string;
}

// Memoised in a production build, where the files cannot change; re-read in dev.
const memo = new Map<string, unknown>();
const caching = process.env.NODE_ENV === "production";

function cached<T>(key: string, compute: () => T): T {
    if (caching && memo.has(key)) {
        return memo.get(key) as T;
    }
    const value = compute();
    if (caching) {
        memo.set(key, value);
    }
    return value;
}

function listDir(dir: string): string[] {
    try {
        return fs.readdirSync(dir).sort();
    } catch (err) {
        if ((err as NodeJS.ErrnoException).code === "ENOENT") {
            return [];
        }
        throw err;
    }
}

const slugRe = /^[a-z0-9][a-z0-9-]*$/;
const dateRe = /^\d{4}-\d{2}-\d{2}$/;

function isDate(value: unknown): value is string {
    return typeof value === "string" && dateRe.test(value) && !Number.isNaN(Date.parse(`${value}T00:00:00Z`));
}

export function entrySlugs(kind: Kind): string[] {
    return cached(`slugs:${kind}`, () =>
        listDir(path.join(contentDir, kind)).filter(
            (name) => slugRe.test(name) && fs.statSync(path.join(contentDir, kind, name)).isDirectory()
        )
    );
}

export function entryLocales(kind: Kind, slug: string): Locale[] {
    return locales.filter((locale) => fs.existsSync(path.join(contentDir, kind, slug, `${locale}.md`)));
}

export function docSlugs(): string[] {
    return cached("docs:slugs", () =>
        listDir(docsDir)
            .filter((name) => name.endsWith(".md") && name !== "README.md")
            .map((name) => name.slice(0, -3))
    );
}

/*
 * The site paths (without a locale) a link in content may point at -- the
 * check behind the build failing on a broken internal link.
 */
function routeExists(sitePath: string, locale: Locale): boolean {
    const p = sitePath.replace(/\/+$/, "") || "/";
    if (["/", "/about", "/hypervisor", "/docs", "/howto", "/blog", "/feed.xml"].includes(p)) {
        return true;
    }
    const m = /^\/(docs|howto|blog)\/([^/]+)$/.exec(p);
    if (!m) {
        return false;
    }
    const [, section, slug] = m as unknown as [string, string, string];
    if (section === "docs") {
        return docSlugs().includes(slug);
    }
    return entryLocales(section as Kind, slug).includes(locale);
}

function splitFrontmatter(source: string, file: string): { data: Record<string, unknown>; body: string } {
    const m = /^---\r?\n([\s\S]*?)\r?\n---[ \t]*(?:\r?\n|$)/.exec(source);
    if (!m) {
        throw new Error(`${file}: no frontmatter (a --- block at the top)`);
    }
    const data: unknown = parseYaml(m[1] ?? "");
    if (typeof data !== "object" || data === null || Array.isArray(data)) {
        throw new Error(`${file}: the frontmatter is not a mapping`);
    }
    return { data: data as Record<string, unknown>, body: source.slice(m[0].length) };
}

function requireString(data: Record<string, unknown>, key: string, file: string): string {
    const value = data[key];
    if (typeof value !== "string" || value.trim() === "") {
        throw new Error(`${file}: "${key}" must be a non-empty string`);
    }
    return value.trim();
}

function wordsPerMinute(locale: Locale): number {
    // Russian words run longer; both rates are the usual ones for technical prose.
    return locale === "ru" ? 180 : 220;
}

async function loadEntry(kind: Kind, slug: string, locale: Locale): Promise<Entry> {
    const file = path.join(contentDir, kind, slug, `${locale}.md`);
    const rel = path.relative(root, file);
    const { data, body } = splitFrontmatter(fs.readFileSync(file, "utf8"), rel);

    for (const key of Object.keys(data)) {
        if (!allowedKeys[kind].includes(key)) {
            throw new Error(`${rel}: unknown frontmatter key "${key}" (allowed: ${allowedKeys[kind].join(", ")})`);
        }
    }
    const title = requireString(data, "title", rel);
    const description = requireString(data, "description", rel);
    let date: string | undefined;
    let tags: string[] = [];
    let order = 0;
    if (kind === "blog") {
        if (!isDate(data.date)) {
            throw new Error(`${rel}: "date" must be YYYY-MM-DD`);
        }
        date = data.date;
        if (!Array.isArray(data.tags) || data.tags.length === 0) {
            throw new Error(`${rel}: "tags" must be a non-empty list`);
        }
        tags = data.tags.map((tag) => String(tag));
        const unknown = tags.filter((tag) => !(tagNames as readonly string[]).includes(tag));
        if (unknown.length > 0) {
            throw new Error(`${rel}: unknown tags ${unknown.join(", ")} (allowed: ${tagNames.join(", ")})`);
        }
    }
    if (kind === "howto") {
        if (typeof data.order !== "number" || !Number.isInteger(data.order)) {
            throw new Error(`${rel}: "order" must be an integer`);
        }
        order = data.order;
    }
    if (data.updated !== undefined && !isDate(data.updated)) {
        throw new Error(`${rel}: "updated" must be YYYY-MM-DD`);
    }
    const updated = data.updated as string | undefined;

    const rendered = await renderMarkdown(body, {
        locale,
        file: rel,
        repoDir: null,
        extractTitle: false,
        docSlugs: new Set(docSlugs()),
        routeExists: (p) => routeExists(p, locale),
    });
    return {
        kind,
        slug,
        locale,
        title,
        description,
        date,
        updated,
        tags,
        order,
        minutes: Math.max(1, Math.round(rendered.words / wordsPerMinute(locale))),
        html: rendered.html,
        toc: rendered.toc,
    };
}

export function getEntry(kind: Kind, slug: string, locale: Locale): Promise<Entry> {
    return cached(`entry:${kind}:${slug}:${locale}`, () => loadEntry(kind, slug, locale));
}

export async function getEntries(kind: Kind, locale: Locale): Promise<Entry[]> {
    const slugs = entrySlugs(kind).filter((slug) => entryLocales(kind, slug).includes(locale));
    const entries = await Promise.all(slugs.map((slug) => getEntry(kind, slug, locale)));
    if (kind === "blog") {
        // Newest first; the slug keeps two posts of one day in a stable order.
        entries.sort((a, b) => (b.date ?? "").localeCompare(a.date ?? "") || a.slug.localeCompare(b.slug));
    } else {
        entries.sort((a, b) => a.order - b.order || a.slug.localeCompare(b.slug));
    }
    return entries;
}

/*
 * The site is bilingual all the way down: a post, a how-to or a page that
 * exists in one language only would leave the language switcher pointing at
 * a 404. The sitemap (built once per deploy) calls this, so such a build fails.
 */
export function assertTranslations(): void {
    const missing: string[] = [];
    for (const kind of ["blog", "howto", "pages"] as const) {
        for (const slug of entrySlugs(kind)) {
            for (const locale of locales) {
                if (!fs.existsSync(path.join(contentDir, kind, slug, `${locale}.md`))) {
                    missing.push(`content/${kind}/${slug}/${locale}.md`);
                }
            }
        }
    }
    if (missing.length > 0) {
        throw new Error(`missing translations:\n  ${missing.join("\n  ")}`);
    }
}

function docInfo(slug: string) {
    return docsManifest[slug];
}

async function loadDoc(slug: string, locale: Locale): Promise<Doc> {
    const translation = locale === "ru" ? path.join(docsRuDir, `${slug}.md`) : null;
    const translated = translation !== null && fs.existsSync(translation);
    const file = translated ? translation : path.join(docsDir, `${slug}.md`);
    const rel = path.relative(root, file);
    const rendered = await renderMarkdown(fs.readFileSync(file, "utf8"), {
        locale,
        file: rel,
        // A translation's links are written as docs/ writes them, relative to docs/.
        repoDir: "docs",
        extractTitle: true,
        docSlugs: new Set(docSlugs()),
        routeExists: (p) => routeExists(p, locale),
    });
    const info = docInfo(slug);
    return {
        slug,
        locale,
        translated,
        group: info?.group ?? "other",
        title: info?.title[locale] ?? rendered.title ?? slug,
        description: info?.description[locale] ?? rendered.lead?.slice(0, 200) ?? "",
        html: rendered.html,
        toc: rendered.toc,
    };
}

export function getDoc(slug: string, locale: Locale): Promise<Doc> {
    return cached(`doc:${slug}:${locale}`, () => loadDoc(slug, locale));
}

export function hasDocTranslation(slug: string, locale: Locale): boolean {
    return locale === "en" || fs.existsSync(path.join(docsRuDir, `${slug}.md`));
}

// The docs grouped as the index and the sidebar show them, without rendering any.
export function docSummaries(locale: Locale): { group: DocGroup; docs: DocSummary[] }[] {
    const order = Object.keys(docsManifest);
    const summaries = docSlugs().map((slug): DocSummary & { rank: number } => {
        const info = docInfo(slug);
        let title = info?.title[locale];
        if (!title) {
            const heading = /^#\s+(.+)$/m.exec(fs.readFileSync(path.join(docsDir, `${slug}.md`), "utf8"));
            title = heading?.[1]?.trim() ?? slug;
        }
        const rank = order.indexOf(slug);
        return {
            slug,
            group: info?.group ?? "other",
            title,
            description: info?.description[locale] ?? "",
            rank: rank === -1 ? order.length : rank,
        };
    });
    return docGroups
        .map((group) => ({
            group,
            docs: summaries
                .filter((doc) => doc.group === group)
                .sort((a, b) => a.rank - b.rank || a.title.localeCompare(b.title))
                .map(({ rank: _rank, ...doc }) => doc),
        }))
        .filter((g) => g.docs.length > 0);
}
