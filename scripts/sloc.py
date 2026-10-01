#!/usr/bin/env python3
"""Count the source lines of the kernel, component by component.

Every file under src/ is lexed just far enough to tell a line of code from a
comment from a blank one -- `//` and `/* */` (nested, in Rust), with string,
char and raw string literals skipped over so that a "//" inside one is not
taken for a comment -- and the counts are summed per component: a directory
of src/cpp (arch/ split by architecture), a crate of src/rust (drivers/ and
modules/ split by crate). A line holding both code and a comment is code.

The vendored crates (src/rust/vendor) and cargo's output (target/) are not
the kernel's own code and are not counted.

    ./scripts/sloc.py                  # the three tables: C++, Rust, asm
    ./scripts/sloc.py --lang rust      # one of them
    ./scripts/sloc.py --top 10         # and the ten largest files of each
    ./scripts/sloc.py --markdown       # tables a document can take as they are

Rust gets a second table, of what is `unsafe` in each component: the
`unsafe { }` blocks and the lines of code inside them (a line under two nested
blocks counted once), and the `unsafe fn`, `unsafe impl`/`unsafe trait` and
`unsafe extern` items. The word is looked for in the code alone, so an
"unsafe" in a comment or a string is not one.
"""

import argparse
import collections
import os
import re
import sys

ROOT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "src")

LANGS = {
    "cpp": ("C++", {".cpp", ".h", ".hpp", ".c", ".cc"}),
    "rust": ("Rust", {".rs"}),
    "asm": ("asm", {".asm", ".S", ".s"}),
}
SKIP_DIRS = {"vendor", "target"}

CODE, COMMENT, BLANK = 0, 1, 2


def classify_c_like(text, rust):
    """Classify each line of C, C++, Rust or preprocessed GNU as source.

    Returns the kind of each line and the text with its comments blanked and
    its literals emptied, line for line with the original.
    """
    kinds = []
    out = []
    code = comment = False
    depth = 0       # of block comments: Rust nests them, C does not
    i, n = 0, len(text)

    def skip_to(end, start):
        """Index just past `end`, searched for from `start`; lines in between are code."""
        j = text.find(end, start)
        j = n if j == -1 else j + len(end)
        out.append('""')
        for _ in range(text.count("\n", i, j)):
            kinds.append(CODE)
            out.append("\n")
        return j

    while i < n:
        c = text[i]
        nxt = text[i + 1] if i + 1 < n else ""
        if c == "\n":
            kinds.append(CODE if code else COMMENT if comment else BLANK)
            out.append("\n")
            code = False
            comment = depth > 0
            i += 1
        elif depth:
            if c == "*" and nxt == "/":
                depth -= 1
                i += 2
            elif rust and c == "/" and nxt == "*":
                depth += 1
                i += 2
            else:
                i += 1
        elif c == "/" and nxt == "/":
            comment = True
            while i < n and text[i] != "\n":
                i += 1
        elif c == "/" and nxt == "*":
            comment = True
            depth = 1
            out.append(" ")
            i += 2
        elif c == "r" and rust and nxt in "\"#" and not (i and (text[i - 1].isalnum() or text[i - 1] == "_")):
            # r"..." or r#"..."#: no escapes inside, closed by the same number of #
            j = i + 1
            while j < n and text[j] == "#":
                j += 1
            if j < n and text[j] == '"':
                code = True
                i = skip_to('"' + "#" * (j - i - 1), j + 1)
            else:
                code = True
                i += 1
        elif c == "R" and not rust and nxt == '"' and not (i and (text[i - 1].isalnum() or text[i - 1] == "_")):
            # R"delim( ... )delim"
            j = text.find("(", i + 2)
            code = True
            if j == -1:
                i += 2
            else:
                i = skip_to(")" + text[i + 2:j] + '"', j + 1)
        elif c == '"' or (c == "'" and not rust):
            code = True
            out.append('""')
            i += 1
            while i < n and text[i] != c:
                if text[i] == "\n":
                    kinds.append(CODE)
                    out.append("\n")
                i += 2 if text[i] == "\\" else 1
            i += 1
        elif c == "'":
            # a char literal ('x', '\n', '\u{1F600}') or a lifetime ('a)
            code = True
            if nxt == "\\":
                j = text.find("'", i + 3)
                out.append("' '")
                i = j + 1 if j != -1 else n
            elif i + 2 < n and text[i + 2] == "'":
                out.append("' '")
                i += 3
            else:
                out.append(c)
                i += 1
        else:
            if not c.isspace():
                code = True
            out.append(c)
            i += 1
    if code or comment:
        kinds.append(CODE if code else COMMENT)
    return kinds, "".join(out)


UNSAFE = re.compile(r"\bunsafe\s*(\{|fn\b|impl\b|trait\b|extern\b\s*(?:\"\"\s*)?(fn\b|\{)?)")


class Unsafe:
    """What is unsafe in a component: blocks, the code lines in them, items."""

    def __init__(self):
        self.code = self.blocks = self.lines = self.fns = self.impls = self.externs = 0

    def add(self, kinds, stripped):
        self.code += kinds.count(CODE)
        covered = set()
        for m in UNSAFE.finditer(stripped):
            what = m.group(1)
            if what == "{":
                self.blocks += 1
                covered.update(block_lines(stripped, m.end() - 1))
            elif what == "fn" or m.group(2) == "fn":
                self.fns += 1
            elif what.startswith("extern"):
                self.externs += 1
            else:
                self.impls += 1
        self.lines += sum(1 for ln in covered if kinds[ln] == CODE)

    def __iadd__(self, other):
        for field in vars(self):
            setattr(self, field, getattr(self, field) + getattr(other, field))
        return self

    def row(self, name):
        share = 100.0 * self.lines / self.code if self.code else 0.0
        return [name, str(self.code), str(self.blocks), str(self.lines), "%.1f%%" % share,
                str(self.fns), str(self.impls), str(self.externs)]


UNSAFE_HEADER = ["sloc", "unsafe {}", "lines in", "of sloc", "unsafe fn", "unsafe impl", "unsafe extern"]


def block_lines(stripped, open_brace):
    """The line numbers from the brace at `open_brace` to the one that closes it."""
    depth = 0
    j = open_brace
    while j < len(stripped):
        if stripped[j] == "{":
            depth += 1
        elif stripped[j] == "}":
            depth -= 1
            if depth == 0:
                break
        j += 1
    first = stripped.count("\n", 0, open_brace)
    return range(first, first + stripped.count("\n", open_brace, j) + 1)


def classify_nasm(text):
    kinds = []
    for line in text.splitlines():
        s = line.strip()
        kinds.append(BLANK if not s else COMMENT if s.startswith(";") else CODE)
    return kinds


def classify(path, lang, ext):
    with open(path, errors="replace") as f:
        text = f.read()
    if ext == ".asm":
        return classify_nasm(text), ""
    # .S goes through the C preprocessor, so its comments are C's
    return classify_c_like(text, rust=(lang == "rust"))


def component(rel):
    """src-relative path -> the component the file is counted under."""
    parts = rel.split(os.sep)
    top = parts[1] if len(parts) > 2 else "."
    if top in ("arch", "drivers", "modules") and parts[0] == "rust" and len(parts) > 3:
        return top + "/" + parts[2]
    if top == "arch" and len(parts) > 3:
        return top + "/" + parts[2]
    return top


class Count:
    def __init__(self):
        self.files = self.code = self.comment = self.blank = 0

    def add(self, kinds):
        self.files += 1
        self.code += kinds.count(CODE)
        self.comment += kinds.count(COMMENT)
        self.blank += kinds.count(BLANK)

    def __iadd__(self, other):
        self.files += other.files
        self.code += other.code
        self.comment += other.comment
        self.blank += other.blank
        return self

    def row(self, name, total_code):
        share = 100.0 * self.code / total_code if total_code else 0.0
        return [name, str(self.files), str(self.code), "%.1f%%" % share,
                str(self.comment), str(self.blank),
                str(self.code + self.comment + self.blank)]


HEADER = ["component", "files", "sloc", "share", "comment", "blank", "lines"]


def print_table(rows, markdown, total=True):
    """rows[0] is the header and, if `total`, rows[-1] the total; column 0 is text."""
    if markdown:
        print("| " + " | ".join(rows[0]) + " |")
        print("|---|" + "---:|" * (len(rows[0]) - 1))
        for r in rows[1:]:
            print("| " + " | ".join(r) + " |")
        return
    width = [max(len(r[k]) for r in rows) for k in range(len(rows[0]))]
    rule = "  ".join("-" * w for w in width)
    for idx, r in enumerate(rows):
        if idx == 1 or (total and idx == len(rows) - 1):
            print(rule)
        print("  ".join(r[k].ljust(width[k]) if k == 0 else r[k].rjust(width[k])
                        for k in range(len(r))))


def main():
    ap = argparse.ArgumentParser(description="Count source lines per component.")
    ap.add_argument("--lang", choices=sorted(LANGS), action="append",
                    help="count only this language (may be repeated)")
    ap.add_argument("--top", type=int, default=0, metavar="N",
                    help="also list the N largest files of each language")
    ap.add_argument("--markdown", action="store_true", help="print markdown tables")
    args = ap.parse_args()
    langs = args.lang or ["cpp", "rust", "asm"]

    by_ext = {ext: lang for lang in langs for ext in LANGS[lang][1]}
    comps = {lang: collections.defaultdict(Count) for lang in langs}
    largest = {lang: [] for lang in langs}
    unsafe = collections.defaultdict(Unsafe)

    for dirpath, dirnames, filenames in os.walk(ROOT):
        dirnames[:] = sorted(d for d in dirnames if d not in SKIP_DIRS)
        for name in filenames:
            ext = os.path.splitext(name)[1]
            lang = by_ext.get(ext)
            if lang is None:
                continue
            path = os.path.join(dirpath, name)
            rel = os.path.relpath(path, ROOT)
            kinds, stripped = classify(path, lang, ext)
            comps[lang][component(rel)].add(kinds)
            if lang == "rust":
                unsafe[component(rel)].add(kinds, stripped)
            largest[lang].append((kinds.count(CODE), rel))

    totals = {}
    for lang in langs:
        total = totals[lang] = Count()
        for c in comps[lang].values():
            total += c
        rows = [[LANGS[lang][0]] + HEADER[1:]]
        for name, c in sorted(comps[lang].items(), key=lambda kv: (-kv[1].code, kv[0])):
            rows.append(c.row(name, total.code))
        rows.append(total.row("total", total.code))
        print_table(rows, args.markdown)
        print()
        if lang == "rust":
            total = Unsafe()
            for u in unsafe.values():
                total += u
            rows = [["Rust unsafe"] + UNSAFE_HEADER]
            for name, u in sorted(unsafe.items(), key=lambda kv: (-kv[1].lines, -kv[1].blocks, kv[0])):
                rows.append(u.row(name))
            rows.append(total.row("total"))
            if not total.externs:
                rows = [r[:-1] for r in rows]       # a column of zeroes says nothing
            print_table(rows, args.markdown)
            print()
        if args.top:
            rows = [["largest " + LANGS[lang][0] + " files", "sloc"]]
            for n, rel in sorted(largest[lang], key=lambda t: (-t[0], t[1]))[:args.top]:
                rows.append([os.path.join("src", rel), str(n)])
            print_table(rows, args.markdown, total=False)
            print()

    if len(langs) > 1:
        grand = Count()
        for lang in langs:
            grand += totals[lang]
        rows = [["all"] + HEADER[1:]]
        rows += [totals[lang].row(LANGS[lang][0], grand.code) for lang in langs]
        rows.append(grand.row("total", grand.code))
        print_table(rows, args.markdown)
    return 0


if __name__ == "__main__":
    sys.exit(main())
