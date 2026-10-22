#!/usr/bin/env python3
"""Checks a running build of the site end to end, from its sitemap.

usage: scripts/verify-site.py <base-url> [<site-url>]
  base-url  where the server answers: https://nos.example.org after a deploy,
            or http://127.0.0.1:3000 for a local standalone build
  site-url  the SITE_URL it was built with, when that is not base-url

Every page in the sitemap must answer 200 in its own language, with its
canonical link, hreflang alternates that list each other, one h1 and an Open
Graph image that is a PNG; / must redirect by Accept-Language and the
language cookie; robots.txt, both feeds and the 404s must be what they should.
Exit status 0 only if every check passed -- gate on that, not on the output.
"""
import re
import sys
import urllib.error
import urllib.request
import xml.etree.ElementTree as ET
from html.parser import HTMLParser

if len(sys.argv) not in (2, 3):
    sys.exit(__doc__)
base = sys.argv[1].rstrip("/")
site = (sys.argv[2] if len(sys.argv) == 3 else sys.argv[1]).rstrip("/")
failures = []


def fail(msg):
    failures.append(msg)
    print("FAIL", msg)


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *a, **k):
        return None


opener = urllib.request.build_opener(NoRedirect)


def get(path, headers=None):
    req = urllib.request.Request(base + path, headers=headers or {})
    try:
        with opener.open(req, timeout=30) as r:
            return r.status, dict(r.headers), r.read()
    except urllib.error.HTTPError as e:
        return e.code, dict(e.headers), e.read()


def local(url):
    assert url.startswith(site), url
    return url[len(site):] or "/"


class Head(HTMLParser):
    def __init__(self):
        super().__init__()
        self.canonical = None
        self.alternates = {}
        self.lang = None
        self.title = None
        self.description = None
        self.og_image = None
        self.robots = None
        self._in_title = False
        self.h1 = 0

    def handle_starttag(self, tag, attrs):
        a = dict(attrs)
        if tag == "html":
            self.lang = a.get("lang")
        elif tag == "link" and a.get("rel") == "canonical":
            self.canonical = a.get("href")
        elif tag == "link" and a.get("rel") == "alternate" and a.get("hreflang"):
            self.alternates[a["hreflang"]] = a["href"]
        elif tag == "meta" and a.get("name") == "description":
            self.description = a.get("content")
        elif tag == "meta" and a.get("property") == "og:image":
            self.og_image = self.og_image or a.get("content")
        elif tag == "meta" and a.get("name") == "robots":
            self.robots = a.get("content")
        elif tag == "title":
            self._in_title = True
        elif tag == "h1":
            self.h1 += 1

    def handle_data(self, data):
        if self._in_title:
            self.title = (self.title or "") + data

    def handle_endtag(self, tag):
        if tag == "title":
            self._in_title = False


# Language detection at / and on an unprefixed path.
for headers, expect in [
    ({"Accept-Language": "ru-RU,ru;q=0.9,en;q=0.8"}, "/ru"),
    ({"Accept-Language": "en-US,en;q=0.9"}, "/en"),
    ({"Accept-Language": "de-DE,de;q=0.9"}, "/en"),
    ({"Accept-Language": "de;q=0.9,ru;q=0.5"}, "/ru"),
    ({"Accept-Language": "ru;q=0.2,en;q=0.8"}, "/en"),
    ({}, "/en"),
    ({"Accept-Language": "ru", "Cookie": "NEXT_LOCALE=en"}, "/en"),
    ({"Accept-Language": "en", "Cookie": "NEXT_LOCALE=ru"}, "/ru"),
]:
    status, h, _ = get("/", headers)
    loc = h.get("Location") or h.get("location")
    if status != 307 or not loc or not loc.endswith(expect):
        fail(f"/ with {headers}: {status} -> {loc}, expected 307 -> {expect}")
    vary = (h.get("Vary") or h.get("vary") or "").lower()
    if "accept-language" not in vary or "cookie" not in vary:
        fail(f"/ redirect without Vary: Accept-Language, Cookie ({vary})")
status, h, _ = get("/docs/boot", {"Accept-Language": "ru"})
if status != 307 or not (h.get("Location") or h.get("location") or "").endswith("/ru/docs/boot"):
    fail(f"/docs/boot did not redirect to /ru/docs/boot: {status} {h.get('Location')}")

# robots.txt
status, h, body = get("/robots.txt")
text = body.decode()
if status != 200 or f"Sitemap: {site}/sitemap.xml" not in text or "User-agent: Yandex" not in text:
    fail(f"robots.txt: {status}\n{text}")

# The sitemap, and every page in it.
status, h, body = get("/sitemap.xml")
if status != 200:
    fail(f"sitemap.xml: {status}")
    sys.exit(1)
ns = {"s": "http://www.sitemaps.org/schemas/sitemap/0.9", "x": "http://www.w3.org/1999/xhtml"}
root = ET.fromstring(body)
urls = {}
for u in root.findall("s:url", ns):
    loc = u.find("s:loc", ns).text
    alts = {l.get("hreflang"): l.get("href") for l in u.findall("x:link", ns)}
    urls[loc] = alts
print(f"sitemap: {len(urls)} urls")
# Six sections in two languages, the posts, the guides and the docs: far more than this.
if len(urls) < 40:
    fail(f"sitemap has only {len(urls)} urls")

pages = {}
for loc, alts in sorted(urls.items()):
    path = local(loc)
    status, h, body = get(path)
    if status != 200:
        fail(f"{path}: {status}")
        continue
    p = Head()
    p.feed(body.decode("utf-8", "replace"))
    pages[loc] = p
    want_lang = path.split("/")[1]
    if p.lang != want_lang:
        fail(f"{path}: <html lang={p.lang}>")
    if p.canonical != loc:
        fail(f"{path}: canonical {p.canonical}")
    if p.alternates != alts:
        fail(f"{path}: page alternates {p.alternates} != sitemap {alts}")
    if loc not in alts.values():
        fail(f"{path}: does not list itself among its alternates")
    if not p.title or not p.description:
        fail(f"{path}: title {p.title!r} description {p.description!r}")
    if p.h1 != 1:
        fail(f"{path}: {p.h1} h1 elements")
    if not p.og_image or not p.og_image.startswith(site):
        fail(f"{path}: og:image {p.og_image}")
    if p.robots and "noindex" in p.robots:
        fail(f"{path}: robots {p.robots}")
    # The dev default leaking into a page means SITE_URL did not reach the build.
    if b"http://localhost:3000" in body and "localhost:3000" not in site:
        fail(f"{path}: mentions http://localhost:3000")

# hreflang reciprocity: every alternate lists this page back.
for loc, alts in urls.items():
    for lang, href in alts.items():
        if href not in urls:
            fail(f"{local(loc)}: alternate {lang} -> {href} not in the sitemap")
        elif loc not in urls[href].values():
            fail(f"{local(loc)}: {local(href)} does not list it back")

# Each distinct og:image answers with a PNG.
for img in sorted({p.og_image for p in pages.values() if p.og_image}):
    status, h, body = get(local(img.split("?")[0]))
    if status != 200 or not body.startswith(b"\x89PNG"):
        fail(f"og:image {img}: {status}")

# The Russian frame around an English doc: canonical to the English page, not in the sitemap.
status, h, body = get("/ru/docs/boot")
p = Head()
p.feed(body.decode())
if status != 200 or p.canonical != f"{site}/en/docs/boot" or p.alternates:
    fail(f"/ru/docs/boot: {status} canonical {p.canonical} alternates {p.alternates}")
if f"{site}/ru/docs/boot" in urls:
    fail("/ru/docs/boot is in the sitemap")
if 'lang="en"' not in body.decode():
    fail("/ru/docs/boot does not mark the English text lang=en")

# Feeds.
for lang in ("en", "ru"):
    status, h, body = get(f"/{lang}/feed.xml")
    if status != 200 or "rss" not in (h.get("Content-Type") or h.get("content-type") or ""):
        fail(f"/{lang}/feed.xml: {status} {h.get('Content-Type')}")
        continue
    feed = ET.fromstring(body)
    items = feed.findall("channel/item")
    if not items:
        fail(f"/{lang}/feed.xml: no items")
    for item in items:
        link = item.find("link").text
        if link not in urls:
            fail(f"/{lang}/feed.xml: item {link} not in the sitemap")

# Not found: localized inside a locale, global outside; never a 200.
for path, code in [("/en/no-such-page", 404), ("/ru/blog/no-such-post", 404), ("/en/docs/no-such-doc", 404), ("/wp-login.php", 404)]:
    status, h, body = get(path)
    if status != code:
        fail(f"{path}: {status}, expected {code}")
status, h, body = get("/ru/no-such-page")
if "Страница не найдена" not in body.decode("utf-8", "replace"):
    fail("/ru/no-such-page is not the Russian 404")

# Security headers on a page.
status, h, body = get("/en")
lower = {k.lower(): v for k, v in h.items()}
for header in ("content-security-policy", "x-content-type-options", "referrer-policy", "x-frame-options"):
    if header not in lower:
        fail(f"/en lacks {header}")
if "x-powered-by" in lower:
    fail("/en sends X-Powered-By")

print(f"{len(pages)} pages checked, {len(failures)} failures")
sys.exit(1 if failures else 0)
