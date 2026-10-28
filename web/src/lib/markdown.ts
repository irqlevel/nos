import fs from "node:fs";
import path from "node:path";
import type { Element, ElementContent, Root as HastRoot } from "hast";
import type { Html, Paragraph, Root as MdastRoot, Text } from "mdast";
import rehypeShiki from "@shikijs/rehype";
import { toString } from "hast-util-to-string";
import rehypeAutolinkHeadings from "rehype-autolink-headings";
import rehypeSlug from "rehype-slug";
import rehypeStringify from "rehype-stringify";
import remarkGfm from "remark-gfm";
import remarkParse from "remark-parse";
import remarkRehype from "remark-rehype";
import { unified } from "unified";
import { SKIP, visit } from "unist-util-visit";
import { VFile } from "vfile";
import { isLocale, type Locale } from "./i18n";
import { site } from "./site";

export interface TocItem {
    id: string;
    text: string;
    depth: 2 | 3;
}

export interface Rendered {
    html: string;
    toc: TocItem[];
    // The first level-1 heading, taken out of the body (docs/ pages carry their title that way).
    title?: string;
    // The first paragraph as plain text: a doc's description when the manifest has none.
    lead?: string;
    // Words outside code blocks, for the reading time.
    words: number;
}

export interface RenderContext {
    locale: Locale;
    // The file, for error messages.
    file: string;
    /*
     * The repository directory a relative link resolves from ("docs" for the
     * repository's own docs); null for the site's content, which links to
     * site paths and full URLs only.
     */
    repoDir: string | null;
    extractTitle: boolean;
    docSlugs: ReadonlySet<string>;
    // Is this site path (no locale prefix, e.g. "/blog/x") a page in this locale?
    routeExists: (sitePath: string) => boolean;
}

interface FileData {
    ctx: RenderContext;
    title?: string;
    lead?: string;
    toc: TocItem[];
    words: number;
}

function data(file: VFile): FileData {
    return file.data as unknown as FileData;
}

/*
 * Raw HTML in a Markdown source is shown as the text it is rather than dropped
 * or rendered: none of these pages means to embed markup, and docs/ writes
 * "<pid>" and "<dev>" outside code spans now and then.
 */
function remarkHtmlAsText() {
    return (tree: MdastRoot) => {
        visit(tree, "html", (node: Html, index, parent) => {
            if (!parent || index === undefined) {
                return;
            }
            const text: Text = { type: "text", value: node.value };
            if (parent.type === "root") {
                const paragraph: Paragraph = { type: "paragraph", children: [text] };
                parent.children[index] = paragraph;
            } else {
                parent.children[index] = text;
            }
        });
    };
}

function isElement(node: unknown, tagName?: string): node is Element {
    const el = node as Element;
    return el?.type === "element" && (tagName === undefined || el.tagName === tagName);
}

function rehypeExtract() {
    return (tree: HastRoot, file: VFile) => {
        const d = data(file);
        if (d.ctx.extractTitle) {
            const index = tree.children.findIndex((child) => isElement(child, "h1"));
            if (index !== -1) {
                d.title = toString(tree.children[index] as Element).trim();
                tree.children.splice(index, 1);
            }
        }
        const lead = tree.children.find((child) => isElement(child, "p"));
        if (lead) {
            d.lead = toString(lead as Element).replace(/\s+/g, " ").trim();
        }
        d.toc = [];
        visit(tree, "element", (node: Element) => {
            if ((node.tagName === "h2" || node.tagName === "h3") && typeof node.properties.id === "string") {
                d.toc.push({
                    id: node.properties.id,
                    text: toString(node).trim(),
                    depth: node.tagName === "h2" ? 2 : 3,
                });
            }
        });
        let words = 0;
        visit(tree, (node) => {
            if (isElement(node, "pre")) {
                return SKIP;
            }
            if (node.type === "text") {
                words += (node as Text).value.split(/\s+/).filter(Boolean).length;
            }
            return undefined;
        });
        d.words = words;
    };
}

const schemeRe = /^[a-z][a-z0-9+.-]*:/i;

/* The files the site serves as they are -- a post's images -- from public/ */
const publicDir = path.join(process.cwd(), "public");

/** Whether a site path (no locale, e.g. "/images/x/a.webp") is a file of public/. */
function isPublicFile(sitePath: string): boolean {
    const file = path.join(publicDir, decodeURI(sitePath));
    return file.startsWith(publicDir + path.sep) && fs.existsSync(file) && fs.statSync(file).isFile();
}

function splitHash(href: string): [string, string] {
    const at = href.indexOf("#");
    return at === -1 ? [href, ""] : [href.slice(0, at), href.slice(at)];
}

/*
 * Every link is rewritten to where it leads on the site, or the build fails:
 * a site path gets the page's locale, a link between files of docs/ becomes a
 * link between doc pages, and one that leaves docs/ goes to the file on GitHub.
 * A link to a page that does not exist is collected and reported, so a typo is
 * a failed build rather than a 404 somebody finds later. A link to a file of
 * public/ -- an image's full size -- is left as it is.
 */
function rewriteHref(href: string, ctx: RenderContext, errors: string[]): string {
    if (href.startsWith("#") || href.startsWith("//") || schemeRe.test(href)) {
        return href;
    }
    const [target, hash] = splitHash(href);
    if (target.startsWith("/") && isPublicFile(target)) {
        return href;
    }
    if (target.startsWith("/")) {
        const segments = target.split("/").filter(Boolean);
        const prefixed = isLocale(segments[0]);
        const sitePath = "/" + (prefixed ? segments.slice(1) : segments).join("/");
        if (!ctx.routeExists(sitePath)) {
            errors.push(`${href}: no such page`);
        }
        return prefixed ? href : `/${ctx.locale}${sitePath === "/" ? "" : sitePath}${hash}`;
    }
    if (ctx.repoDir === null) {
        errors.push(`${href}: a relative link; write /docs/<name>, /blog/<slug>, /howto/<slug> or a full URL`);
        return href;
    }
    const resolved = path.posix.normalize(path.posix.join(ctx.repoDir, decodeURI(target)));
    if (resolved.startsWith("..")) {
        errors.push(`${href}: leaves the repository`);
        return href;
    }
    const doc = /^docs\/([^/]+)\.md$/.exec(resolved);
    if (doc) {
        const slug = doc[1] as string;
        if (slug === "README") {
            return `/${ctx.locale}/docs${hash}`;
        }
        if (!ctx.docSlugs.has(slug)) {
            errors.push(`${href}: no such document`);
        }
        return `/${ctx.locale}/docs/${slug}${hash}`;
    }
    return `${site.repoBlob}${resolved}${hash}`;
}

function rehypeLinks() {
    return (tree: HastRoot, file: VFile) => {
        const ctx = data(file).ctx;
        const errors: string[] = [];
        visit(tree, "element", (node: Element) => {
            if (node.tagName === "a" && typeof node.properties.href === "string") {
                node.properties.href = rewriteHref(node.properties.href, ctx, errors);
            } else if (node.tagName === "img" && typeof node.properties.src === "string") {
                const src = node.properties.src;
                if (!schemeRe.test(src) && !src.startsWith("/") && ctx.repoDir !== null) {
                    node.properties.src = site.repoRaw + path.posix.normalize(path.posix.join(ctx.repoDir, src));
                } else if (src.startsWith("/") && !src.startsWith("//") && !isPublicFile(src)) {
                    errors.push(`${src}: no such file in public/`);
                }
                node.properties.loading = "lazy";
                node.properties.decoding = "async";
            }
        });
        if (errors.length > 0) {
            const message = `${ctx.file}: broken links:\n  ${errors.join("\n  ")}`;
            // A build fails on one; the dev server only warns, so half-written content can be previewed.
            if (process.env.NODE_ENV === "production") {
                throw new Error(message);
            }
            console.warn(message);
        }
    };
}

// Wide tables scroll inside their own box instead of widening the page on a phone.
function rehypeWrapTables() {
    return (tree: HastRoot) => {
        visit(tree, "element", (node: Element, index, parent) => {
            if (node.tagName !== "table" || !parent || index === undefined) {
                return undefined;
            }
            const wrapper: Element = {
                type: "element",
                tagName: "div",
                properties: { className: ["table-wrap"] },
                children: [node],
            };
            parent.children[index] = wrapper as ElementContent;
            return SKIP;
        });
    };
}

const processor = unified()
    .use(remarkParse)
    .use(remarkGfm)
    .use(remarkHtmlAsText)
    .use(remarkRehype)
    .use(rehypeSlug)
    .use(rehypeExtract)
    .use(rehypeLinks)
    .use(rehypeWrapTables)
    .use(rehypeAutolinkHeadings, {
        behavior: "append",
        test: ["h2", "h3", "h4"],
        properties: { className: ["anchor"], ariaHidden: "true", tabIndex: -1 },
        content: { type: "text", value: "#" },
    })
    .use(rehypeShiki, {
        themes: { light: "github-light", dark: "github-dark" },
        defaultColor: false,
        langs: [
            "shellscript",
            "shellsession",
            "c",
            "cpp",
            "rust",
            "asm",
            "python",
            "toml",
            "makefile",
            "diff",
            "json",
            "yaml",
            "ini",
            "dockerfile",
            "typescript",
            "javascript",
            "tsx",
            "html",
            "xml",
        ],
        langAlias: { nasm: "asm", gas: "asm" },
        defaultLanguage: "text",
        fallbackLanguage: "text",
    })
    .use(rehypeStringify);

export async function renderMarkdown(source: string, ctx: RenderContext): Promise<Rendered> {
    const file = new VFile({ value: source, path: ctx.file });
    file.data = { ctx, toc: [], words: 0 } as unknown as VFile["data"];
    const out = await processor.process(file);
    const d = data(out);
    return { html: String(out), toc: d.toc, title: d.title, lead: d.lead, words: d.words };
}
