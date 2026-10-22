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
  VERSION                       the version a release is tagged with
  scripts/release.sh            build the image; --upload ships it to the server over ssh
  scripts/verify-site.py        check a running site end to end, from its sitemap
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
- `YANDEX_METRIKA_ID` puts a Yandex Metrica counter in every page, early in
  `<head>`, and reports the pages reached without a reload as hits
  (`src/components/MetrikaHits.tsx`); the Content-Security-Policy then lets
  Metrica's hosts in, and its sites frame the pages for Session Replay. Without
  it a build has no counter and a policy of this origin alone

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

`HTTP_PORT` and `HTTPS_PORT` in `.env` (80 and 443 by default) are the host's
side of Caddy's bindings: a port, or `address:port` to bind one interface
(`127.0.0.1:8443`). Caddy stays on 80 and 443 inside its container, so its
redirects and its certificate are those of the public 80 and 443 -- the only
ports Let's Encrypt checks a domain on. Move them when 80 and 443 still reach
them: a port forward, or a proxy in front that passes TLS through untouched
(nginx `stream` with `ssl_preread`, HAProxy in TCP mode). A proxy that
terminates TLS itself needs no Caddy: publish the web container's port
instead (`ports: ["127.0.0.1:3000:3000"]` under `web`) and proxy to that.

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
add swap, or release from your own machine (below): then the server builds
nothing and needs no checkout at all.

The web container runs as an unprivileged user on a read-only filesystem with
every capability dropped, and is reachable only through Caddy. The pages send
`X-Content-Type-Options`, `Referrer-Policy`, `X-Frame-Options` and a
`Content-Security-Policy` that allows only this origin (`next.config.ts`);
Caddy adds HSTS. Anything loaded from another origin has to be added to that
policy first, as Yandex Metrica's hosts are when the build has a counter.

Node and Caddy are pinned to a minor version (`node:24.22-alpine`,
`caddy:2.11-alpine`), so a rebuild picks up their patch releases; the npm
dependencies are exact versions with a lockfile.

## Release from your machine

`scripts/release.sh` builds the image tagged with the site's version and, with
`--upload`, ships it to the server over ssh and starts it there:

```sh
scripts/release.sh                       # build nos-web:<version> here
scripts/release.sh --upload              # build it for the server, ship it, start it there
scripts/release.sh --upload root@host    # the same, to another destination
scripts/release.sh --up                  # build it and start it on this machine
scripts/release.sh --help
```

- **The version** is `VERSION`; `IMAGE_VERSION` in `.env` (or the environment)
  overrides it for a release, and `IMAGE_NAME` names the image (`nos-web`).
  Bump `VERSION` for each release: `latest` is refused, it is what a plain
  `docker compose up --build` calls its build.
- **Where it goes**: `DEPLOY_HOST` (`user@host`, or a `Host` of `~/.ssh/config`),
  `DEPLOY_DIR` (`nos-web`, under the remote home unless absolute) and
  `DEPLOY_SSH_OPTS` (`-p 2222 -i ~/.ssh/deploy_key`) in `.env`. The server needs
  Docker with its compose plugin, and a user that may run them.
- **The image carries `SITE_URL`**, made from `DOMAIN`: release with the `.env` of
  the deployment the image is for.

What `--upload` does, and what it refuses to skip:

1. Asks the server for its platform and builds for it -- x86-64 for a Hetzner box
   when releasing from an arm64 Mac. The build stages run natively and only the
   image's last stage is the server's platform, so that takes seconds, not an
   emulated build; the Dockerfile fails the build should native code ever turn up
   in that stage.
2. Checks the Caddyfile with the Caddy image the compose file names. A Caddyfile
   Caddy refuses stops the release before anything on the server changes.
3. Sends the image -- `docker save | gzip | ssh docker load` -- unless the server
   already has that very build.
4. Puts `docker-compose.yml`, the `Caddyfile` and this `.env` into `DEPLOY_DIR`,
   with `IMAGE_NAME` and `IMAGE_VERSION` pinned in that `.env`, so the server runs
   exactly the image it was sent.
5. Runs `docker compose up -d` there, waits until the web container is healthy
   and runs that image, then reloads Caddy so it takes the new Caddyfile, or
   starts Caddy afresh if it is not running. Anything short of that is reported
   with the containers' last log lines, and the script exits 1.

A release does not take the site down: Caddy stays up, and holds requests for up
to 15 seconds while the web container is replaced (`lb_try_duration` in the
`Caddyfile`). Measured on a test server: a hundred requests during a release,
every one answered.

Every version sent stays on the server. To roll back, set `IMAGE_VERSION` in its
`.env` to an earlier one (`docker image ls nos-web` lists them) and run
`docker compose up -d --no-build` in `DEPLOY_DIR`. Old ones go with
`docker image rm nos-web:<version>`.
