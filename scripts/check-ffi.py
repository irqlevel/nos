#!/usr/bin/env python3
"""Every spelling of a function that crosses a language or a crate, held to
one signature.

A function the kernel's two halves -- or a module and the kernel, or a
fuzzer and the code it stands in for -- call each other by is written down
several times: declared in the `ffi` crate (src/crates/base/ffi), defined
`#[no_mangle]` in a layer or in rust_ffi.cpp, declared again in the C++ that
calls a Rust export, and stood in for by the host's fuzzers (fuzz/). Nothing
else compares those: rustc compares extern declarations with each other, the
linker compares names, the C++ compiler sees one translation unit at a time,
and the ABI digest a module carries hashes the declarations alone. A
definition that drifts -- an argument added, a u32 widened to a u64 -- builds,
links and loads, and the callee reads a register its caller never wrote.

So every function named kernel_* or rust_*, and every one the ffi crate
declares, is looked for in all of those places, and every spelling found has
to come to the same signature as the ABI sees it: an integer by its width and
sign, every raw pointer and reference as a pointer, a function pointer (an
`Option` of one included) by its own parameters and result, a type alias or
a typedef by what it names, a struct passed by value by its name.

Two more things no compiler holds, held here too. The Makefile takes the
functions a module may import from the ffi crate's extern blocks by reading
them as text (`ffi_functions`): a block it does not see -- indented, an ABI
other than "C", on one line -- exports nothing, and a comment line naming a
function in one exports a name with no declaration; so what that reading
finds is held to a parse of every extern block, and a `static` in one, which
is never exported, is refused. And the structs both halves lay out -- a task
or a module listed, the memory's counters, a PCI function, a terminal, a
random draw, a module's own header -- are held field by field, in order, by
name and width: a size assert passes two fields of one width swapped, or one
renamed on one side.

Run by `make check`. Exit code 22 when two spellings disagree, each listed
with where it is, as check-crates.py's.
"""

import os
import re
import sys

ROOT = os.path.normpath(os.path.join(os.path.dirname(os.path.abspath(__file__)), ".."))
FFI = os.path.join(ROOT, "src", "crates", "base", "ffi", "src")
RUST_TREES = [os.path.join(ROOT, "src", "crates"), os.path.join(ROOT, "fuzz")]
CPP_TREES = [os.path.join(ROOT, "src", "base"), os.path.join(ROOT, "fuzz", "cpp")]
SKIP_DIRS = {"vendor", "target", ".git", "node_modules"}
CROSSING = re.compile(r"^(kernel|rust)_\w+$")


def strip(text, rust):
    """`text` with its comments and the insides of its string and character
    literals blanked, every newline kept so that a position still has its
    line. Rust block comments nest; a Rust lifetime is not a literal."""
    out = []
    i, n = 0, len(text)
    while i < n:
        c = text[i]
        if text.startswith("//", i):
            while i < n and text[i] != "\n":
                i += 1
            continue
        if text.startswith("/*", i):
            depth, i = 1, i + 2
            while i < n and depth:
                if rust and text.startswith("/*", i):
                    depth, i = depth + 1, i + 2
                elif text.startswith("*/", i):
                    depth, i = depth - 1, i + 2
                else:
                    if text[i] == "\n":
                        out.append("\n")
                    i += 1
            out.append(" ")
            continue
        if rust and c == "r" and re.match(r'r#*"', text[i:i + 8]) and (i == 0 or not (text[i - 1].isalnum() or text[i - 1] == "_")):
            hashes = len(re.match(r"r(#*)", text[i:]).group(1))
            end = text.find('"' + "#" * hashes, i + 2 + hashes)
            end = n if end < 0 else end + 1 + hashes
            out.append('""' + "\n" * text.count("\n", i, end))
            i = end
            continue
        if c == '"':
            j = i + 1
            while j < n and text[j] != '"':
                j += 2 if text[j] == "\\" else 1
            out.append('""' + "\n" * text.count("\n", i, j))
            i = j + 1
            continue
        if c == "'":
            if rust:
                m = re.match(r"'(\\.[^']*|[^'\\])'", text[i:i + 12])
                if not m:
                    out.append(c)
                    i += 1
                    continue
                out.append("' '")
                i += len(m.group(0))
                continue
            j = i + 1
            while j < n and text[j] != "'":
                j += 2 if text[j] == "\\" else 1
            out.append("' '")
            i = j + 1
            continue
        out.append(c)
        i += 1
    return "".join(out)


def matching(text, open_at):
    """Where the bracket at `open_at` is closed."""
    pairs = {"(": ")", "<": ">", "[": "]", "{": "}"}
    stack = []
    for j in range(open_at, len(text)):
        ch = text[j]
        if ch in "([{" or (ch == "<" and stack and stack[-1] == ">"):
            stack.append(pairs[ch])
        elif ch == "<" and not stack:
            stack.append(">")
        elif stack and ch == stack[-1]:
            stack.pop()
            if not stack:
                return j
    return -1


def split_top(text, sep=","):
    """`text` split at the `sep`s outside every bracket."""
    parts, depth, cur = [], 0, ""
    for ch in text:
        if ch in "([{<":
            depth += 1
        elif ch in ")]}>" and depth:
            depth -= 1
        if ch == sep and depth == 0:
            parts.append(cur)
            cur = ""
        else:
            cur += ch
    if cur.strip():
        parts.append(cur)
    return [p.strip() for p in parts if p.strip()]


def line_of(text, pos):
    return text.count("\n", 0, pos) + 1


def walk(trees, suffixes):
    for tree in trees:
        for dirpath, dirnames, filenames in os.walk(tree):
            dirnames[:] = sorted(d for d in dirnames if d not in SKIP_DIRS and not d.startswith("."))
            for f in sorted(filenames):
                if f.endswith(suffixes):
                    yield os.path.join(dirpath, f)


# ---- Rust ----

RUST_INTS = {
    "usize": "u64", "isize": "i64", "u64": "u64", "i64": "i64", "u32": "u32", "i32": "i32",
    "u16": "u16", "i16": "i16", "u8": "u8", "i8": "i8", "bool": "bool", "f32": "f32", "f64": "f64",
    "c_int": "i32", "c_uint": "u32", "c_long": "i64", "c_ulong": "u64", "c_longlong": "i64",
    "c_ulonglong": "u64", "c_short": "i16", "c_ushort": "u16", "c_char": "i8", "c_schar": "i8",
    "c_uchar": "u8", "()": "()", "!": "()",
}
WRAPPERS = re.compile(r"^(Option|NonNull|core::ptr::NonNull|core::option::Option)\s*<(.*)>$", re.S)


def crate_of(path):
    d = os.path.dirname(path)
    while len(d) > len(ROOT):
        if os.path.exists(os.path.join(d, "Cargo.toml")):
            return d
        d = os.path.dirname(d)
    return ROOT


def rust_aliases(text):
    return {m.group(1): m.group(2).strip()
            for m in re.finditer(r"\btype\s+(\w+)\s*=\s*([^;]+);", text)}


def rust_type(t, aliases, depth=0):
    t = re.sub(r"\s+", " ", t.strip())
    if depth > 8:
        return t
    if t.startswith(("*const ", "*mut ", "&")):
        return "ptr"
    m = WRAPPERS.match(t)
    if m:
        inner = rust_type(m.group(2), aliases, depth + 1)
        return inner if inner == "ptr" or inner.startswith("fn(") else "Option<" + inner + ">"
    m = re.match(r"^(unsafe\s+)?(extern\s+\"C\"\s+)?fn\s*\(", t.replace('""', '"C"'))
    if m:
        open_at = t.index("(")
        close = matching(t, open_at)
        params = [rust_type(p.split(":", 1)[1] if re.match(r"^\s*(mut\s+)?\w+\s*:(?!:)", p) else p, aliases, depth + 1)
                  for p in split_top(t[open_at + 1:close])]
        rest = t[close + 1:].strip()
        ret = rust_type(rest[2:], aliases, depth + 1) if rest.startswith("->") else "()"
        return "fn(" + ",".join(params) + ")->" + ret
    last = t.split("::")[-1].strip()
    if last in RUST_INTS:
        return RUST_INTS[last]
    if last in aliases:
        return rust_type(aliases[last], aliases, depth + 1)
    return last


def rust_sig(params_text, ret_text, aliases):
    params = []
    for p in split_top(params_text):
        m = re.match(r"^\s*(mut\s+)?[\w]+\s*:(?!:)(.*)$", p, re.S)
        params.append(rust_type(m.group(2) if m else p, aliases))
    ret = rust_type(ret_text, aliases) if ret_text else "()"
    return "(" + ", ".join(params) + ") -> " + ret


FN_HEAD = re.compile(r"\bfn\s+(\w+)\s*\(")


def rust_fn_at(text, m, aliases):
    """The signature of the function whose `fn NAME(` `m` matched."""
    open_at = m.end() - 1
    close = matching(text, open_at)
    if close < 0:
        return None
    rest = text[close + 1:close + 400]
    r = re.match(r"\s*->\s*(.*?)\s*(\{|;|\bwhere\b)", rest, re.S)
    return rust_sig(text[open_at + 1:close], r.group(1) if r else "", aliases)


# An extern block, wherever it is and whatever its ABI string (blanked by
# `strip`): `extern "C" {`, `pub(crate) unsafe extern "C-unwind" {`, `extern {`.
EXTERN_BLOCK = re.compile(r'\bextern\s*(?:""\s*)?\{')


def ffi_declarations(spellings, ffi_names, statics):
    alias_ffi = {}
    texts = {}
    for path in walk([FFI], (".rs",)):
        texts[path] = strip(open(path, encoding="utf-8").read(), True)
        alias_ffi.update(rust_aliases(texts[path]))
    for path, text in texts.items():
        for block in EXTERN_BLOCK.finditer(text):
            open_at = text.index("{", block.start())
            end = matching(text, open_at)
            for m in FN_HEAD.finditer(text, open_at + 1, end):
                name = m.group(1)
                sig = rust_fn_at(text, m, alias_ffi)
                if sig:
                    ffi_names.add(name)
                    spellings.setdefault(name, []).append((sig, path, line_of(text, m.start()), "ffi declaration"))
            for m in re.finditer(r"\bstatic\s+(?:mut\s+)?(\w+)\s*:", text[open_at + 1:end]):
                statics.append((m.group(1), path, line_of(text, open_at + 1 + m.start())))
    return alias_ffi


def makefile_scan():
    """The names the Makefile's `ffi_functions` takes from the ffi crate, as
    its awk takes them: a block opens at a line beginning `extern "C" {` or
    `unsafe extern "C" {`, ends at a line beginning `}`, and every line in
    between that has `fn <name>` in it -- a comment's too -- gives a name."""
    names = {}
    for path in walk([FFI], (".rs",)):
        inblock = False
        for number, line in enumerate(open(path, encoding="utf-8").read().split("\n"), 1):
            if not inblock:
                if re.match(r'^(unsafe )?extern "C" \{', line):
                    inblock = True
                continue
            if line.startswith("}"):
                inblock = False
                continue
            m = re.search(r"fn [A-Za-z_][A-Za-z0-9_]*", line)
            if m:
                names.setdefault(m.group(0)[3:], (path, number))
    return names


# ---- the structs both halves lay out ----

# (Rust file, its struct, C++ file, its struct): each field by name, in
# order, of one width -- an array's element's; its length is the size
# asserts' on each side.
SHARED_STRUCTS = [
    ("src/crates/base/ffi/src/system.rs", "TaskInfo", "src/base/kernel/task.h", "TaskSnapshot"),
    ("src/crates/base/ffi/src/system.rs", "ModuleInfo", "src/base/kernel/module.h", "ModuleSnapshot"),
    ("src/crates/base/ffi/src/system.rs", "MemInfo", "src/base/kernel/rust_ffi.cpp", "RustMemInfo"),
    ("src/crates/base/ffi/src/pci.rs", "PciDeviceInfo", "src/base/kernel/rust_ffi.cpp", "RustPciDeviceInfo"),
    ("src/crates/base/ffi/src/cmd.rs", "CmdTerminal", "src/base/lib/printer.h", "TerminalInfo"),
    ("src/crates/base/ffi/src/random.rs", "HwRandomDraw", "src/base/kernel/rust_ffi.cpp", "HwRandomDraw"),
    ("src/crates/base/kmod/src/lib.rs", "ModuleInfo", "src/base/kernel/module.cpp", "ModuleInfo"),
]

WIDTH = {"u8": 1, "i8": 1, "bool": 1, "u16": 2, "i16": 2, "u32": 4, "i32": 4, "f32": 4,
         "u64": 8, "i64": 8, "f64": 8, "ptr": 8}


def kind(t):
    """A field's type, made comparable: a width, an array of a width, or a
    pointer -- a function's included."""
    if t == "ptr" or t.startswith("fn("):
        return "ptr"
    return str(WIDTH.get(t, t))


def rust_struct(text, name):
    m = re.search(r"\bstruct\s+%s\s*\{" % name, text)
    if not m:
        return None
    open_at = text.index("{", m.start())
    fields = []
    for part in split_top(text[open_at + 1:matching(text, open_at)]):
        f = re.match(r"^(?:#\[[^\]]*\]\s*)*(?:pub(?:\([^)]*\))?\s+)?(\w+)\s*:(.*)$", part, re.S)
        if not f:
            return None
        t = f.group(2).strip()
        a = re.match(r"^\[(.*);.*\]$", t, re.S)
        fields.append((f.group(1), "[" + kind(rust_type(a.group(1), {})) + "]" if a else kind(rust_type(t, {}))))
    return fields


def cpp_struct(text, name):
    m = re.search(r"\bstruct\s+%s\s*\{" % name, text)
    if not m:
        return None
    open_at = text.index("{", m.start())
    fields = []
    for decl in text[open_at + 1:matching(text, open_at)].split(";"):
        decl = re.sub(r"\s+", " ", decl).strip()
        if not decl or decl.startswith(("static ", "using ", "typedef ")):
            continue
        fp = re.match(r"^(.*?)\(\s*\*\s*(\w+)\s*\)\s*\(.*\)$", decl)
        if fp:
            fields.append((fp.group(2), "ptr"))
            continue
        a = re.match(r"^(.*?)\b(\w+)\s*\[[^\]]*\]$", decl)
        if a:
            fields.append((a.group(2), "[" + kind(cpp_type(a.group(1), {})) + "]"))
            continue
        f = re.match(r"^(.*?)\b(\w+)$", decl)
        if not f:
            return None
        fields.append((f.group(2), kind(cpp_type(f.group(1), {}))))
    return fields


def shared_structs():
    bad = []
    for rust_file, rust_name, cpp_file, cpp_name in SHARED_STRUCTS:
        r = rust_struct(strip(open(os.path.join(ROOT, rust_file), encoding="utf-8").read(), True), rust_name)
        c = cpp_struct(strip(open(os.path.join(ROOT, cpp_file), encoding="utf-8").read(), False), cpp_name)
        if r is None or c is None:
            bad.append("%s in %s or %s in %s not found, or not read" % (rust_name, rust_file, cpp_name, cpp_file))
        elif r != c:
            bad.append("%s (%s) and %s (%s) are laid out differently:\n        %s\n        %s"
                       % (rust_name, rust_file, cpp_name, cpp_file, r, c))
    return bad


def rust_definitions(spellings, ffi_names, alias_ffi):
    by_crate = {}
    files = [p for p in walk(RUST_TREES, (".rs",)) if not p.startswith(FFI + os.sep)]
    texts = {}
    for path in files:
        texts[path] = strip(open(path, encoding="utf-8", errors="replace").read(), True)
        by_crate.setdefault(crate_of(path), {}).update(rust_aliases(texts[path]))
    for path, text in texts.items():
        aliases = dict(alias_ffi)
        aliases.update(by_crate.get(crate_of(path), {}))
        for attr in re.finditer(r"#\[\s*(unsafe\s*\(\s*)?no_mangle\s*\)?\s*\]", text):
            m = FN_HEAD.search(text, attr.end(), attr.end() + 400)
            if not m:
                continue
            between = text[attr.end():m.start()]
            if not re.search(r'extern\s+""', between) or re.search(r"[;{}]", between):
                continue
            name = m.group(1)
            if not (CROSSING.match(name) or name in ffi_names):
                continue
            sig = rust_fn_at(text, m, aliases)
            if sig:
                what = "fuzz stand-in" if path.startswith(os.path.join(ROOT, "fuzz") + os.sep) else "Rust definition"
                spellings.setdefault(name, []).append((sig, path, line_of(text, m.start()), what))


# ---- C++ ----

CPP_INTS = {
    "void": "()", "bool": "bool", "char": "i8", "signed char": "i8", "unsigned char": "u8",
    "short": "i16", "short int": "i16", "unsigned short": "u16", "unsigned short int": "u16",
    "int": "i32", "signed": "i32", "signed int": "i32", "unsigned": "u32", "unsigned int": "u32",
    "long": "i64", "long int": "i64", "long long": "i64", "long long int": "i64",
    "unsigned long": "u64", "unsigned long int": "u64", "unsigned long long": "u64",
    "unsigned long long int": "u64",
    "u8": "u8", "u16": "u16", "u32": "u32", "u64": "u64", "s8": "i8", "s16": "i16", "s32": "i32",
    "s64": "i64", "ulong": "u64", "size_t": "u64", "ssize_t": "i64", "uintptr_t": "u64",
    "intptr_t": "i64", "ptrdiff_t": "i64", "uint8_t": "u8", "uint16_t": "u16", "uint32_t": "u32",
    "uint64_t": "u64", "int8_t": "i8", "int16_t": "i16", "int32_t": "i32", "int64_t": "i64",
    "float": "f32", "double": "f64",
}
CPP_WORDS = {"const", "volatile", "struct", "class", "enum", "union", "typename", "extern", "static",
             "inline", "noexcept", "constexpr", "register"}
CPP_TYPE_WORDS = {"void", "bool", "char", "short", "int", "long", "signed", "unsigned", "float", "double"}
CPP_NOT_TYPES = {"return", "else", "do", "case", "goto", "throw", "delete", "new", "co_return", "sizeof",
                 "typedef", "using", "operator", "static", "friend", "virtual"}


def cpp_aliases(text):
    aliases = {}
    for m in re.finditer(r"\btypedef\s+([^;]+?)\(\s*\*\s*(\w+)\s*\)\s*\(([^;]*)\)\s*;", text):
        aliases[m.group(2)] = ("fnptr", m.group(1), m.group(3))
    for m in re.finditer(r"\busing\s+(\w+)\s*=\s*([^;]+?)\(\s*\*\s*\)\s*\(([^;]*)\)\s*;", text):
        aliases[m.group(1)] = ("fnptr", m.group(2), m.group(3))
    for m in re.finditer(r"\btypedef\s+([^;(]+?)\s+(\w+)\s*;", text):
        aliases.setdefault(m.group(2), ("type", m.group(1)))
    for m in re.finditer(r"\busing\s+(\w+)\s*=\s*([^;(]+?)\s*;", text):
        aliases.setdefault(m.group(1), ("type", m.group(2)))
    return aliases


def cpp_type(t, aliases, depth=0):
    """A C++ parameter's or result's type, its name already gone."""
    t = re.sub(r"\[\[[^\]]*\]\]", " ", t)
    if "*" in t or "&" in t or "[" in t:
        return "ptr"
    words = [w for w in re.sub(r"\s+", " ", t).strip().split(" ") if w and w not in CPP_WORDS]
    words = [w.split("::")[-1] for w in words]
    key = " ".join(words)
    if key in CPP_INTS:
        return CPP_INTS[key]
    if len(words) == 1 and words[0] in aliases and depth < 8:
        a = aliases[words[0]]
        if a[0] == "fnptr":
            return "fn(" + ",".join(cpp_params(a[2], aliases, depth + 1)) + ")->" + cpp_type(a[1], aliases, depth + 1)
        return cpp_type(a[1], aliases, depth + 1)
    return key


def cpp_param(p, aliases, depth=0):
    p = re.sub(r"=.*$", "", p, flags=re.S).strip()
    m = re.match(r"^(.*?)\(\s*\*\s*(\w*)\s*\)\s*\((.*)\)\s*$", p, re.S)
    if m:
        return "fn(" + ",".join(cpp_params(m.group(3), aliases, depth + 1)) + ")->" + cpp_type(m.group(1), aliases, depth + 1)
    words = re.findall(r"[\w:]+|\*|&|\[|\]", p)
    # The last word is the parameter's name, unless it is part of the type.
    if len(words) >= 2 and re.match(r"^\w+$", words[-1]) and words[-1] not in CPP_TYPE_WORDS \
            and words[-1] not in CPP_INTS and not (words[-1] in aliases and words[-2] in CPP_WORDS):
        p = p[:p.rfind(words[-1])]
    return cpp_type(p, aliases, depth)


def cpp_params(text, aliases, depth=0):
    parts = split_top(text)
    if parts == ["void"] or not parts:
        return []
    return [cpp_param(p, aliases, depth) for p in parts]


CPP_HEAD = re.compile(r"(?:^|(?<=[;{}]))\s*((?:\[\[\w+\]\]\s*)?(?:extern\s+\"\"\s+)?)"
                      r"((?:[A-Za-z_][\w:]*(?:\s*[\*&])*\s+|[A-Za-z_][\w:]*(?:\s*[\*&])+\s*)+?)"
                      r"((?:kernel|rust)_\w+)\s*\(", re.M)


def cpp_spellings(spellings):
    texts = {}
    aliases = {}
    for path in walk(CPP_TREES, (".cpp", ".h", ".hpp", ".cc")):
        texts[path] = strip(open(path, encoding="utf-8", errors="replace").read(), False)
        aliases.update(cpp_aliases(texts[path]))
    for path, text in texts.items():
        for m in CPP_HEAD.finditer(text):
            ret_words = m.group(2).split()
            if not ret_words or ret_words[0] in CPP_NOT_TYPES or "static" in ret_words:
                continue
            open_at = m.end() - 1
            close = matching(text, open_at)
            if close < 0:
                continue
            after = text[close + 1:close + 64].lstrip()
            after = re.sub(r"^(noexcept|const|override)\b\s*", "", after)
            if after.startswith(";"):
                what = "C++ declaration"
            elif after.startswith("{"):
                what = "C++ definition"
            else:
                continue
            if path.startswith(os.path.join(ROOT, "fuzz") + os.sep):
                what += " (fuzz)"
            ret = cpp_type(m.group(2), aliases)
            if "[[noreturn]]" in m.group(1):
                ret = "()"
            params = cpp_params(text[open_at + 1:close], aliases)
            sig = "(" + ", ".join(params) + ") -> " + ret
            spellings.setdefault(m.group(3), []).append((sig, path, line_of(text, m.start(3)), what))


def main():
    spellings = {}
    ffi_names = set()
    statics = []
    alias_ffi = ffi_declarations(spellings, ffi_names, statics)
    rust_definitions(spellings, ffi_names, alias_ffi)
    cpp_spellings(spellings)

    failed = False
    scanned = makefile_scan()
    for name in sorted(ffi_names - set(scanned)):
        failed = True
        path, line, _ = next(s for s in spellings[name] if s[3] == "ffi declaration")[1:]
        print("check-ffi: %s (%s:%d) is declared in a block the Makefile's scan does not see -- indented, "
              "another ABI, or on one line: it is no export" % (name, os.path.relpath(path, ROOT), line))
    for name in sorted(set(scanned) - ffi_names):
        failed = True
        path, line = scanned[name]
        print("check-ffi: the Makefile's scan takes %s (%s:%d) for an export, which is no declaration -- a "
              "comment that names it?" % (name, os.path.relpath(path, ROOT), line))
    for name, path, line in statics:
        failed = True
        print("check-ffi: %s (%s:%d) is a static in an extern block: a module can import no data, and it "
              "is never exported" % (name, os.path.relpath(path, ROOT), line))
    for problem in shared_structs():
        failed = True
        print("check-ffi: " + problem)

    bad = 0
    compared = 0
    for name in sorted(spellings):
        found = spellings[name]
        if len(found) < 2:
            continue
        compared += 1
        if len({s[0] for s in found}) == 1:
            continue
        bad += 1
        print("check-ffi: %s is spelled %d ways:" % (name, len({s[0] for s in found})))
        for sig, path, line, what in found:
            print("    %s  %s:%d  (%s)" % (sig, os.path.relpath(path, ROOT), line, what))
    if bad:
        print("check-ffi: %d of %d functions disagree with themselves" % (bad, compared))
        return 22
    if failed:
        return 22
    print("check-ffi: %d functions, every spelling of each the same; %d exports as the Makefile scans them; "
          "%d structs laid out alike" % (compared, len(scanned), len(SHARED_STRUCTS)))
    return 0


if __name__ == "__main__":
    sys.exit(main())
