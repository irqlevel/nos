#!/usr/bin/env python3
"""No vector instruction in the kernel or a module outside the code the
vector registers are lent to.

The kernel builds nothing to use the CPU's vector registers -- its C++ with
-mno-sse -mno-80387 (x86-64) or -mgeneral-regs-only (arm64), its Rust
soft-float -- and saves none of them on a context switch: it lends them to
one SIMD section at a time (src/base/kernel/simd.h), in which the crypto
crate's CRYPTOGAMS assembly runs. An instruction that uses them anywhere
else works on whatever they hold -- a guest's registers between its exit
and its next entry, which hv puts back only where a section said it took
them -- and nothing at run time catches it: the CR4.OSFXSR that once made
SSE fault is set at boot, for the sections. So each binary is held to it
as it is linked: its .text disassembled, and every instruction that names
a vector, x87, MMX or mask register -- or changes their state without
naming one -- looked for outside the functions allowed them. The link
fails on one.

Allowed them: the functions of the crypto crate's assembly (every label
src/crates/base/crypto/src/asm/ defines but its local ones) and
hal::simd_leave, which zeroes the registers as a section closes. Allowed
everywhere: the instructions that move a CPU's whole register state to
memory and back -- hvarch's fxsave and xsave, fxrstor and xrstor, which
switch a guest's and name no register.

Usage: check-simd.py ELF...
Exit code 22 on a finding, as cppcheck's.
"""

import os
import re
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.normpath(os.path.join(HERE, ".."))
ASM = os.path.join(ROOT, "src", "crates", "base", "crypto", "src", "asm")
OBJDUMP = os.environ.get("OBJDUMP", "llvm-objdump")
EXIT_FINDING = 22

EM_X86_64 = 62
EM_AARCH64 = 183

# The C++ that zeroes the registers as a section closes, demangled.
KERNEL_ALLOWED = {"hal::simd_leave(unsigned int)"}

# x86-64: an operand that is an XMM, YMM or ZMM register, an MMX one, the
# x87 stack or an AVX-512 mask.
X86_REGISTER = re.compile(r"%(?:[xyz]mm[0-9]+|mm[0-7]|st(?:\([0-7]\))?|k[0-7])\b")
# x86-64: x87's instructions -- every mnemonic that starts with an f, but
# the state switches -- and those that change vector state naming no
# register.
X86_MNEMONIC = re.compile(r"^(?:f(?!xsave|xrstor)[a-z0-9]*|vzeroall|vzeroupper|emms|ldmxcsr|stmxcsr)$")
# arm64: an operand that is an Advanced SIMD or FP register at any width,
# an SVE vector or predicate, SME's array, or FP's control and status.
A64_REGISTER = re.compile(r"(?<![A-Za-z0-9_])(?:[vqdshbz][0-9]+|p[0-9]+|za|fpcr|fpsr)(?![A-Za-z0-9_])")

HEADER = re.compile(r"^[0-9a-f]+ <(.+)>:$")
INSN = re.compile(r"^\s*[0-9a-f]+:\s+(\S+)\s*(.*)$")
SYMBOL_REF = re.compile(r"<[^>]*>")
# Data in the code -- a literal pool, padding -- which objdump prints as a
# directive with its bytes in front, and arm64's mapping symbols ($x code,
# $d data), which mark where it is and name no function.
DATA = re.compile(r"\s\.(?:byte|short|hword|word|long|quad|xword|inst)\b")
MAPPING = re.compile(r"^\$[xd](?:\.[0-9]+)?$")
LABEL = re.compile(r"^([A-Za-z_][A-Za-z0-9_]*):", re.M)


def assembly_functions():
    """Every label the ELF assembly defines that is not a local one: the
    functions -- and the helpers between them -- its code is in."""
    names = set()
    for name in sorted(os.listdir(ASM)):
        if name.endswith("-elf.s") or name.endswith("-linux64.s"):
            with open(os.path.join(ASM, name)) as f:
                names.update(LABEL.findall(f.read()))
    if not names:
        sys.exit("check-simd: no functions found in %s" % ASM)
    return names


def machine(path):
    with open(path, "rb") as f:
        head = f.read(20)
    if len(head) < 20 or head[:4] != b"\x7fELF":
        sys.exit("check-simd: %s is not an ELF file" % path)
    return int.from_bytes(head[18:20], "little")


def findings(path, allowed):
    arch = machine(path)
    if arch not in (EM_X86_64, EM_AARCH64):
        sys.exit("check-simd: %s is for a machine this does not know (%d)" % (path, arch))
    out = subprocess.run([OBJDUMP, "-d", "--no-show-raw-insn", "--demangle", "--section=.text", path],
                         stdout=subprocess.PIPE, stderr=subprocess.PIPE, universal_newlines=True)
    if out.returncode != 0:
        sys.exit("check-simd: %s -d %s: %s" % (OBJDUMP, path, out.stderr.strip()))
    function = None
    seen = 0
    for line in out.stdout.splitlines():
        header = HEADER.match(line)
        if header:
            if not MAPPING.match(header.group(1)):
                function = header.group(1)
            continue
        insn = INSN.match(line)
        if not insn or DATA.search(line):
            continue
        seen += 1
        if function in allowed:
            continue
        mnemonic, operands = insn.group(1), SYMBOL_REF.sub("", insn.group(2))
        if arch == EM_X86_64:
            bad = X86_REGISTER.search(operands) or X86_MNEMONIC.match(mnemonic)
        else:
            bad = A64_REGISTER.search(operands)
        if bad:
            yield "%s: in %s: %s" % (path, function, line.strip())
    if seen == 0:
        sys.exit("check-simd: no instruction in %s's .text: nothing was checked" % path)


def main():
    if len(sys.argv) < 2:
        sys.exit("usage: check-simd.py ELF...")
    allowed = assembly_functions() | KERNEL_ALLOWED
    bad = []
    for path in sys.argv[1:]:
        bad.extend(findings(path, allowed))
    for b in bad:
        print(b)
    if bad:
        print("check-simd: %d vector instruction(s) outside a SIMD section's code: the registers they use are "
              "nobody's to use there (src/base/kernel/simd.h)" % len(bad))
        sys.exit(EXIT_FINDING)


if __name__ == "__main__":
    main()
