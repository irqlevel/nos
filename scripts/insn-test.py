#!/usr/bin/env python3
"""insn test: the hypervisor's MMIO decoder (src/rust/hv/src/insn.rs) and its
guest page walker (src/rust/hv/src/walk.rs), checked on the host.

The decoder turns the bytes of a guest's instruction -- what the guest put
in its own memory -- into the access the hypervisor performs for it: which
register, how many bytes, loaded or stored, and how long the instruction is.
Getting one wrong looks like success: the guest runs on, a value in the
wrong register or RIP stepped into the middle of the next instruction. So
it is checked against the assembler's own encodings: every form of `mov`
Linux's MMIO accessors are -- to and from registers of each width, REX's
and the high bytes, immediates of each size, `movzx` and `movsx` -- over
sixteen addressing forms, assembled by clang and read back by objdump; and
instructions that must be refused (no memory operand, another opcode, a
string move, a lock, `movabs`). Each is also cut short at every length,
which must be refused rather than read as a shorter instruction, and two
million random byte strings must decode without a panic and never to a
length past what was there. The walker is checked against page tables built
by hand: 4 KiB, 2 MiB and 1 GiB pages, five levels, non-canonical and
unmapped addresses, and a fetch across a page boundary.

Needs clang, an LLVM objdump and cargo on the host; the check itself has no
dependencies and builds offline. Exit code 0 = every check passed.
"""

import os
import re
import shutil
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))

R = {'rax': 0, 'rcx': 1, 'rdx': 2, 'rbx': 3, 'rsp': 4, 'rbp': 5, 'rsi': 6, 'rdi': 7}
E32 = {'eax': 0, 'ecx': 1, 'edx': 2, 'ebx': 3, 'esp': 4, 'ebp': 5, 'esi': 6, 'edi': 7}
W16 = {'ax': 0, 'cx': 1, 'dx': 2, 'bx': 3, 'sp': 4, 'bp': 5, 'si': 6, 'di': 7}
B8 = {'al': 0, 'cl': 1, 'dl': 2, 'bl': 3, 'spl': 4, 'bpl': 5, 'sil': 6, 'dil': 7}
for i in range(8, 16):
    R['r%d' % i] = i
    E32['r%dd' % i] = i
    W16['r%dw' % i] = i
    B8['r%db' % i] = i
HIGH = {'ah': 0, 'ch': 1, 'dh': 2, 'bh': 3}

# The memory operands: base, base and displacement both ways, an index and a
# scale, R12 and R13 (which need a SIB byte and a displacement of their
# own), an absolute address as a fixmap's is, RIP-relative, RSP, segment
# overrides, and 32-bit addressing (0x67).
MEMS = ['(%rdi)', '0xb0(%rbx)', '-8(%rbp)', '0x12345(%r12)', '(%rsi,%rcx,4)', '0x10(%r13,%r14,8)',
        '0xffffffffff5fc0b0', '0x1234(%rip)', '(%rsp)', '0x40(%rsp)', '%fs:0x28', '%gs:(%rax)',
        '(%r13)', '(%r12)', '(%eax)', '0x20(%ecx,%edx,2)']

# What the decoder must refuse.
REFUSED = ['movl %eax, %ebx', 'movq %rax, %r9', 'movzbl %al, %ecx', 'movl $1, %eax', 'addl %eax, (%rdi)',
           'orl $1, (%rdi)', 'xchgl %eax, (%rdi)', 'movsl', 'rep movsb', 'stosl', 'lock orl $1, (%rdi)',
           'testl $1, (%rdi)', 'cmpl $0, (%rdi)', 'movnti %eax, (%rdi)', 'movd (%rdi), %xmm0', 'nop',
           'movabsq 0xffffffffff5fc0b0, %rax', 'movabsl %eax, 0xffffffffff5fc0b0']


def cases():
    """(AT&T source, the decode expected): "L size dest extend reg high" for
    a load, "S size reg high" for a store, "I size value" for an immediate
    stored, "-" for one to refuse."""
    out = []
    for m in MEMS:
        # AH to BH have no encoding beside a REX prefix, which R8-R15 and
        # RIP-relative addressing do not need but 64-bit operands do.
        no_high = re.search(r'%r(8|9|1[0-5])|%rip', m) is not None
        for r in ['eax', 'ecx', 'ebx', 'esi', 'edi', 'r8d', 'r15d']:
            out.append(('movl %s, %%%s' % (m, r), 'L 4 4 N %d 0' % E32[r]))
            out.append(('movl %%%s, %s' % (r, m), 'S 4 %d 0' % E32[r]))
        for r in ['rax', 'rdx', 'r9', 'r15']:
            out.append(('movq %s, %%%s' % (m, r), 'L 8 8 N %d 0' % R[r]))
            out.append(('movq %%%s, %s' % (r, m), 'S 8 %d 0' % R[r]))
        for r in ['ax', 'si', 'r10w']:
            out.append(('movw %s, %%%s' % (m, r), 'L 2 2 N %d 0' % W16[r]))
            out.append(('movw %%%s, %s' % (r, m), 'S 2 %d 0' % W16[r]))
        for r in ['al', 'cl', 'dil', 'sil', 'r11b']:
            out.append(('movb %s, %%%s' % (m, r), 'L 1 1 N %d 0' % B8[r]))
            out.append(('movb %%%s, %s' % (r, m), 'S 1 %d 0' % B8[r]))
        if not no_high:
            for r in ['ah', 'bh']:
                out.append(('movb %s, %%%s' % (m, r), 'L 1 1 N %d 1' % HIGH[r]))
                out.append(('movb %%%s, %s' % (r, m), 'S 1 %d 1' % HIGH[r]))
        out.append(('movl $0x12345678, %s' % m, 'I 4 305419896'))
        out.append(('movl $0, %s' % m, 'I 4 0'))
        out.append(('movw $0xbeef, %s' % m, 'I 2 48879'))
        out.append(('movb $0x7f, %s' % m, 'I 1 127'))
        out.append(('movq $-1, %s' % m, 'I 8 18446744073709551615'))
        out.append(('movq $0x7fffffff, %s' % m, 'I 8 2147483647'))
        for r in ['eax', 'r8d']:
            out.append(('movzbl %s, %%%s' % (m, r), 'L 1 4 Z %d 0' % E32[r]))
            out.append(('movzwl %s, %%%s' % (m, r), 'L 2 4 Z %d 0' % E32[r]))
            out.append(('movsbl %s, %%%s' % (m, r), 'L 1 4 S %d 0' % E32[r]))
            out.append(('movswl %s, %%%s' % (m, r), 'L 2 4 S %d 0' % E32[r]))
        for r in ['rcx', 'r14']:
            out.append(('movzbq %s, %%%s' % (m, r), 'L 1 8 Z %d 0' % R[r]))
            out.append(('movzwq %s, %%%s' % (m, r), 'L 2 8 Z %d 0' % R[r]))
            out.append(('movsbq %s, %%%s' % (m, r), 'L 1 8 S %d 0' % R[r]))
            out.append(('movswq %s, %%%s' % (m, r), 'L 2 8 S %d 0' % R[r]))
        out.append(('movzbw %s, %%dx' % m, 'L 1 2 Z 2 0'))
        out.append(('movsbw %s, %%dx' % m, 'L 1 2 S 2 0'))
    out += [(bad, '-') for bad in REFUSED]
    return out


def llvm_objdump():
    """An objdump that is LLVM's -- `llvm-objdump`, or macOS's `objdump`."""
    for name in ('llvm-objdump', 'objdump'):
        path = shutil.which(name)
        if path is None:
            continue
        v = subprocess.run([path, '--version'], capture_output=True, text=True)
        if 'LLVM' in v.stdout:
            return path
    sys.exit('insn-test: needs an LLVM objdump (llvm-objdump)')


def encodings(tmp, source):
    """Each instruction's bytes, as clang assembles and objdump reads them."""
    s = os.path.join(tmp, 't.s')
    o = os.path.join(tmp, 't.o')
    with open(s, 'w') as f:
        f.write('.text\n' + ''.join('  %s\n' % a for a in source))
    subprocess.run(['clang', '-target', 'x86_64-unknown-linux-gnu', '-c', s, '-o', o], check=True)
    dump = subprocess.run([llvm_objdump(), '-d', o], capture_output=True, text=True, check=True).stdout
    insns, pending = [], []
    for line in dump.splitlines():
        m = re.match(r'\s*[0-9a-f]+:\s+((?:[0-9a-f]{2}[ \t])+)\s*(\S*)', line)
        if not m:
            continue
        # A lock prefix is a line of its own.
        if m.group(2) == 'lock':
            pending = m.group(1).split()
            continue
        insns.append(pending + m.group(1).split())
        pending = []
    return insns


def main():
    for tool in ('clang', 'cargo'):
        if shutil.which(tool) is None:
            sys.exit('insn-test: needs %s' % tool)
    tmp = tempfile.mkdtemp(prefix='nos-insn-')
    try:
        listed = cases()
        insns = encodings(tmp, [a for a, _ in listed])
        if len(insns) != len(listed):
            sys.exit('insn-test: objdump gave %d instructions for %d' % (len(insns), len(listed)))
        path = os.path.join(tmp, 'cases.txt')
        with open(path, 'w') as f:
            for (a, e), b in zip(listed, insns):
                f.write('%s|%s|%s\n' % (''.join(b), e, a))
        print('%d reference instructions' % len(listed), flush=True)
        r = subprocess.run(['cargo', 'run', '--release', '--offline', '--quiet',
                            '--manifest-path', os.path.join(HERE, 'insn-check', 'Cargo.toml'),
                            '--target-dir', os.path.join(tmp, 'target'), '--', path])
        sys.exit(r.returncode)
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


if __name__ == '__main__':
    main()
