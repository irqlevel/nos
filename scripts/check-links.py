#!/usr/bin/env python3
"""Every link into this repository that the site and the documents make,
held to naming something in the tree.

The site (web/) links to the source on GitHub's master -- a post's
`https://github.com/irqlevel/nos/blob/master/src/...` -- and turns every
link of docs/ that leaves docs/ into one; GitHub's own view of the
documents follows their relative links. A file moved or renamed leaves such
a link a 404 that nothing reports: src/base/kernel/trace.h, renamed klog.h
on 3 November 2026, stayed linked from the site's GDB how-to, and moving the
crates into their areas moved 38 files the site's posts link to. So each one
is held to the tree, which is what GitHub's master holds once it is pushed:

  - in web/content, web/src, docs/, plans/ and the top's *.md, every
    https://github.com/irqlevel/nos/{blob,tree}/master/<path> and
    https://raw.githubusercontent.com/irqlevel/nos/master/<path>, code
    blocks included -- a command a reader copies is a link too;
  - in docs/, plans/ and the top's *.md, every relative link and image,
    resolved from its file's directory (web/content may have none: the
    site's build refuses them).

Run by `make check`. Exit code 22 if a link names nothing, as cppcheck's.
"""

import os
import posixpath
import re
import sys
import urllib.parse

ROOT = os.path.normpath(os.path.join(os.path.dirname(os.path.abspath(__file__)), ".."))
SKIP = {"node_modules", ".next", "target", "vendor"}
TEXT = (".md", ".ts", ".tsx", ".mjs", ".js")

ABSOLUTE = re.compile(r"https://(?:github\.com/irqlevel/nos/(?:blob|tree)/master|"
                      r"raw\.githubusercontent\.com/irqlevel/nos/master)/([^\s)\]>\"'`#?]+)")
RELATIVE = re.compile(r"\]\(([^)\s]+)\)")
FENCE = re.compile(r"^```.*?^```", re.M | re.S)
CODE = re.compile(r"`[^`\n]*`")


def files(top, exts):
    base = os.path.join(ROOT, top)
    if os.path.isfile(base):
        yield top
        return
    for dirpath, dirnames, filenames in os.walk(base):
        dirnames[:] = sorted(d for d in dirnames if d not in SKIP and not d.startswith("."))
        for name in sorted(filenames):
            if name.endswith(exts):
                yield os.path.relpath(os.path.join(dirpath, name), ROOT)


def exists(path):
    return os.path.exists(os.path.join(ROOT, urllib.parse.unquote(path).rstrip("/")))


def line_of(text, pos):
    return text.count("\n", 0, pos) + 1


def main():
    tops = [f for f in sorted(os.listdir(ROOT)) if f.endswith(".md")]
    absolute_in = ["web/content", "web/src", "docs", "plans"] + tops
    relative_in = ["docs", "plans"] + tops
    wrong = []
    checked = 0
    for top in absolute_in:
        for f in files(top, TEXT):
            text = open(os.path.join(ROOT, f), encoding="utf-8").read()
            for m in ABSOLUTE.finditer(text):
                path = m.group(1).rstrip(".,;:")
                checked += 1
                if not exists(path):
                    wrong.append("%s:%d: %s names nothing in the tree" % (f, line_of(text, m.start()), m.group(0)))
    for top in relative_in:
        for f in files(top, (".md",)):
            text = open(os.path.join(ROOT, f), encoding="utf-8").read()
            # what is in code is not a link, but keep the lines where they were
            bare = FENCE.sub(lambda m: "\n" * m.group(0).count("\n"), text)
            bare = CODE.sub("", bare)
            for m in RELATIVE.finditer(bare):
                href = m.group(1)
                if href.startswith(("#", "/", "http:", "https:", "mailto:")):
                    continue
                target = href.split("#")[0]
                if not target:
                    continue
                resolved = posixpath.normpath(posixpath.join(posixpath.dirname(f), target))
                checked += 1
                if resolved.startswith("..") or not exists(resolved):
                    wrong.append("%s:%d: %s names nothing in the tree" % (f, line_of(bare, m.start()), href))
    for w in wrong:
        print("check-links: " + w)
    if not wrong:
        print("check-links: %d links, every one in the tree" % checked)
    sys.exit(22 if wrong else 0)


if __name__ == "__main__":
    main()
