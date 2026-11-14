#!/usr/bin/env python3
"""Every link into this repository that the site and the documents make,
and every path the tree names its gates and tools by, held to naming
something in the tree.

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

A gate, a tool or a fuzzer is named by its path far more often than it is
linked to -- in CI's steps, the Makefile, a usage line, a document's
command, a comment's "(tests/hv-test.py)" -- and moving the gates from
scripts/ to tests/, and the fuzzers from fuzz/ to tests/fuzz/, rewrote
hundreds of such names that no link is among. One left behind fails only
when it is read: a CI step at once, a gate run by hand the day it is next
run. So these are held as well:

  - anywhere in the tree, every path that starts at scripts/ or tests/
    (with ./, nos/, or a $(ROOT)/ that holds the top before it, or not),
    globs and {a,b} included: from the top, and in web/'s and panel/'s
    own files from the top of their project too -- and every one at
    fuzz/, which names nothing since the fuzzers moved;
  - in every Python file, every os.path.join of the directory the file is
    in -- __file__, and the HERE and ROOT a script makes of it with
    dirname and abspath -- that has nothing but literals after it, a glob
    as a glob, where it lands in the source: what a build makes (out/, a
    target/, the images at the top) is not looked for.

Run by `make check`. Exit code 22 if a link or a path names nothing, as
cppcheck's.
"""

import ast
import glob
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

# A path from the top to a gate, a tool or a fuzzer: what follows a word
# character, a dot, a slash, a dash, ~ or $ is the inside of another path
# (a URL, $(OUT)/..., ~/src/nos/...), not the start of one. fuzz/ is the
# fuzzers' old place, kept here so that a name of it left behind is found.
NAMED_PATH = r"((?:scripts|tests|fuzz)(?:/(?:[A-Za-z0-9_.*-]|\{[A-Za-z0-9_.,*/-]*\})+)+)"
NAMED = re.compile(r"(?<![A-Za-z0-9_./~$-])(?:\./|nos/)?" + NAMED_PATH)
# The same from the top a Makefile or a shell script holds in ROOT
# (tests/fuzz/cpp/Makefile's include path).
ROOTED = re.compile(r"\$(?:\(ROOT\)|\{ROOT\}|ROOT)/" + NAMED_PATH)
# Named where the reader is in another tree: Linux's own scripts/config, run
# in linux/ (web/content/howto/linux-guest).
ELSEWHERE = {"scripts/config"}
# The top's directories a script's path has to be found in; anything else a
# join reaches -- out/, bin/, an image or a kernel at the top -- a build makes.
# fuzz/ for a join left behind, as above.
SOURCE = {"bench", "build", "docs", "fuzz", "panel", "plans", "scripts", "src", "tests", "web"}
MADE = SKIP | {"out", "__pycache__"}
SOURCE_TEXT = TEXT + (".py", ".sh", ".rs", ".toml", ".yml", ".yaml", ".cpp", ".h", ".c", ".S", ".asm",
                      ".ld", ".json", ".txt", ".cfg", ".awk", ".css", ".html")


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


def sources():
    """Every text file of the source: the top's documents, Makefile and
    Dockerfile, CI's workflows, and the source directories, a file with no
    extension (an executable script, a Makefile) included."""
    for name in sorted(os.listdir(ROOT)):
        if name.endswith(".md") or name in ("Makefile", "Dockerfile"):
            yield name
    for top in [".github"] + sorted(d for d in os.listdir(ROOT) if d in SOURCE):
        for dirpath, dirnames, filenames in os.walk(os.path.join(ROOT, top)):
            dirnames[:] = sorted(d for d in dirnames if d not in MADE and not d.startswith("."))
            for name in sorted(filenames):
                if name.endswith(SOURCE_TEXT) or "." not in name:
                    yield os.path.relpath(os.path.join(dirpath, name), ROOT)


def exists(path):
    return os.path.exists(os.path.join(ROOT, urllib.parse.unquote(path).rstrip("/")))


def line_of(text, pos):
    return text.count("\n", 0, pos) + 1


def expand(path):
    """a/{b,c}/d is a/b/d and a/c/d."""
    m = re.search(r"\{([^{}]*)\}", path)
    if not m:
        return [path]
    return [p for alt in m.group(1).split(",") for p in expand(path[:m.start()] + alt + path[m.end():])]


def project_of(f):
    """The top of a project of its own the file is in -- web/ and panel/,
    by their package.json -- whose files name its scripts/ from there."""
    d = os.path.dirname(f)
    while d:
        if os.path.isfile(os.path.join(ROOT, d, "package.json")):
            return d
        d = os.path.dirname(d)
    return None


def names_something(path, f):
    tops = [ROOT]
    project = project_of(f)
    if project:
        tops.append(os.path.join(ROOT, project))
    return any(glob.glob(os.path.join(top, p), recursive=True) for p in expand(path) for top in tops)


def joins(f, text):
    """(line, path from the top) of every os.path.join in a Python file that
    starts at the file's own directory -- __file__, or a name the module binds
    to what dirname, abspath, normpath, realpath and join make of it -- and
    has only literals after that."""
    try:
        tree = ast.parse(text, f)
    except SyntaxError:
        return
    known = {}

    def value(node):
        if isinstance(node, ast.Name):
            return os.path.join(ROOT, f) if node.id == "__file__" else known.get(node.id)
        if not (isinstance(node, ast.Call) and isinstance(node.func, ast.Attribute)
                and isinstance(node.func.value, ast.Attribute) and node.func.value.attr == "path"
                and isinstance(node.func.value.value, ast.Name) and node.func.value.value.id == "os"
                and node.args and not node.keywords):
            return None
        head = value(node.args[0])
        if head is None:
            return None
        how = node.func.attr
        if how in ("abspath", "normpath", "realpath") and len(node.args) == 1:
            return os.path.normpath(head)
        if how == "dirname" and len(node.args) == 1:
            return os.path.dirname(head)
        if how == "join" and all(isinstance(a, ast.Constant) and isinstance(a.value, str) for a in node.args[1:]):
            return os.path.normpath(os.path.join(head, *(a.value for a in node.args[1:])))
        return None

    for stmt in tree.body:
        if isinstance(stmt, ast.Assign) and len(stmt.targets) == 1 and isinstance(stmt.targets[0], ast.Name):
            v = value(stmt.value)
            if v is not None:
                known[stmt.targets[0].id] = v
    for node in ast.walk(tree):
        if (isinstance(node, ast.Call) and isinstance(node.func, ast.Attribute) and node.func.attr == "join"
                and len(node.args) > 1):
            p = value(node)
            if p is not None:
                yield node.lineno, os.path.relpath(p, ROOT)


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
    paths = 0
    for f in sources():
        try:
            text = open(os.path.join(ROOT, f), encoding="utf-8").read()
        except UnicodeDecodeError:
            continue
        for m in list(NAMED.finditer(text)) + list(ROOTED.finditer(text)):
            path = m.group(1).rstrip(".,;:")
            if path in ELSEWHERE:
                continue
            paths += 1
            if not names_something(path, f):
                wrong.append("%s:%d: %s names nothing in the tree" % (f, line_of(text, m.start()), path))
        if f.endswith(".py"):
            for line, path in joins(f, text):
                parts = path.split(os.sep)
                if parts[0] not in SOURCE or MADE & set(parts):
                    continue
                paths += 1
                if not glob.glob(os.path.join(ROOT, path), recursive=True):
                    wrong.append("%s:%d: os.path.join to %s names nothing in the tree" % (f, line, path))
    for w in wrong:
        print("check-links: " + w)
    if not wrong:
        print("check-links: %d links and %d paths, every one in the tree" % (checked, paths))
    sys.exit(22 if wrong else 0)


if __name__ == "__main__":
    main()
