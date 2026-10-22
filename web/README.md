# The nos website

What nos and its hypervisor are, the documentation, how-to guides and a blog,
in English and Russian. Next.js 16 (App Router), every page prerendered at
build time, served by Next's standalone Node server behind Caddy, which gets
and renews the TLS certificate from Let's Encrypt by itself.

```
web/
  content/
    blog/<slug>/{en,ru}.md      blog posts
    howto/<slug>/{en,ru}.md     how-to guides
    pages/{about,hypervisor}/   the two long pages
    docs-ru/<name>.md           optional: a Russian translation of docs/<name>.md
  src/
    app/[locale]/               every page, under /en and /ru
    app/sitemap.ts              sitemap.xml with hreflang alternates
    app/robots.txt/route.ts     robots.txt (with a Yandex group)
    proxy.ts                    / and unprefixed paths -> the visitor's language
    lib/content.ts              reads content/ and ../docs, checks it
    lib/markdown.ts             Markdown -> HTML: links, anchors, highlighting
    lib/dictionaries.ts         every interface string, in both languages
    lib/docs-manifest.ts        how docs/ is grouped, with Russian titles
  Dockerfile, docker-compose.yml, Caddyfile, .env.example
```

The documentation section is **the repository's own `docs/`**, rendered at
build time -- never a copy, so it cannot drift from the code. A file added to
`docs/` gets a page on the next build; to give it a group and a Russian title,
add it to `src/lib/docs-manifest.ts`. The Russian docs pages show the English
text under a note, and point search engines at the English page as canonical,
until a translation is put in `content/docs-ru/<name>.md` (written exactly as
docs/ writes its files, links relative to docs/).

## Develop

```sh
cd web
npm ci
npm run dev          # http://localhost:3000 -> /en or /ru
```

A production build needs the URL the site will live at -- it goes into every
canonical link, the sitemap and the feeds, and the build refuses to run
without it:

```sh
SITE_URL=https://nos.example.org npm run build
npm start
```

## Write

A blog post is `content/blog/<slug>/en.md` and `ru.md`:

```markdown
---
title: "Seven wrong theories and one register read"
description: "One or two sentences, up to ~160 characters: the meta description and the card."
date: 2026-09-05
tags: [hardware, networking, drivers]
---

Text in GitHub-flavoured Markdown. No H1 -- the title is rendered from the
frontmatter; sections start at ##.
```

A how-to is the same with `order: <n>` and `updated: YYYY-MM-DD` instead of
`date` and `tags`; a page has `title` and `description` (and may have `updated`).
The tags a post may carry are listed in `src/lib/content.ts` (`tagNames`) and
translated in `src/lib/dictionaries.ts`.

Links inside content are written **without** a language: `/docs/hypervisor#a-disk`,
`/howto/build`, `/blog/<slug>`, `/about`, `/hypervisor` -- the site adds `/en` or
`/ru`. Source files and commits link to GitHub with full URLs.

### What a build refuses

`npm run build` fails, naming the file, on:

- a link to a page that does not exist (`/docs/<name>`, `/blog/<slug>`, …), or a
  relative link in content; the dev server only warns, so drafts can be previewed
- a post, how-to or page that exists in one language only (the language
  switcher would lead to a 404)
- a missing or unknown frontmatter key, a date that is not `YYYY-MM-DD`, a tag
  that is not in the list
- no `SITE_URL`, or one that is not a bare `https://host` origin

## Languages

Every page lives under `/en/...` or `/ru/...`. A request for `/` -- or any path
without a language -- is redirected (307) by `src/proxy.ts` to the visitor's
language: the one they chose with the switcher (the `NEXT_LOCALE` cookie), else
the best match for the browser's `Accept-Language`, else English. Crawlers index
the prefixed pages, which never redirect; each page lists its translations as
hreflang alternates, with `x-default` on the English one.

## SEO

- `/sitemap.xml` -- every page in every language with `xhtml:link` alternates, which
  Google and Yandex both read; posts and guides carry `lastmod`
- `/robots.txt` -- everything allowed; a Yandex group with `Clean-param` for
  analytics tags; the sitemap's address
- `/en/feed.xml`, `/ru/feed.xml` -- RSS for each blog
- canonical links, Open Graph and Twitter cards (images generated per locale and per
  post), JSON-LD (`WebSite`, `SoftwareSourceCode`, `BlogPosting`, `TechArticle`,
  `BreadcrumbList`)
- `GOOGLE_SITE_VERIFICATION` / `YANDEX_VERIFICATION` put the verification meta tags
  in every page

After the first deploy: add the site in [Google Search Console](https://search.google.com/search-console)
and [Yandex Webmaster](https://webmaster.yandex.com), verify it with the meta tag
(set the code in `.env` and rebuild), and submit `https://<domain>/sitemap.xml` in both.

## Deploy

On a Linux server with Docker Engine and the compose plugin (2.17 or later, for
`additional_contexts`):

1. Point the domain's A (and AAAA) records **and `www`'s** at the server; open
   TCP 80 and 443 (and UDP 443 for HTTP/3). Without a `www` record, delete the
   `www.` block from the `Caddyfile`.
2. Get the repository. The site needs only `web/` and `docs/`, so a sparse clone
   keeps the vendored crates off the server:

   ```sh
   git clone --filter=blob:none --sparse https://github.com/irqlevel/nos.git
   cd nos && git sparse-checkout set web docs && cd web
   ```

3. `cp .env.example .env` and set `DOMAIN` and `ACME_EMAIL`.
4. `docker compose up -d --build`

Caddy obtains the certificate on the first request to the domain and renews it
by itself; HTTP is redirected to HTTPS and `www` to the bare domain. Watch it with
`docker compose logs -f caddy`. Keep the `caddy_data` volume -- it holds the
certificates and the ACME account.

Then check the live site end to end -- every page of the sitemap, its language,
canonical and hreflang links, the redirects, robots.txt, the feeds, the 404s;
the exit status is the verdict:

```sh
python3 scripts/verify-site.py https://<domain>
```

To publish new posts or the current docs: `git pull && docker compose up -d --build`.

The build runs on the server and wants about 1.5 GB of memory. On a smaller box,
add swap, or build the image elsewhere and load it:

```sh
docker compose build web && docker save nos-web | ssh server docker load
# then on the server
docker compose up -d --no-build
```

The web container runs as an unprivileged user on a read-only filesystem with
every capability dropped, and is reachable only through Caddy. The pages send
`X-Content-Type-Options`, `Referrer-Policy`, `X-Frame-Options` and a
`Content-Security-Policy` that allows only this origin (`next.config.ts`);
Caddy adds HSTS. Anything loaded from another origin -- an analytics script, say --
has to be added to that policy first.

Node and Caddy are pinned to a minor version (`node:24.22-alpine`,
`caddy:2.11-alpine`), so a rebuild picks up their patch releases; the npm
dependencies are exact versions with a lockfile.
