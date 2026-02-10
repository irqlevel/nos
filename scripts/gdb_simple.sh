#!/bin/bash
cd "$(dirname "$0")/.."
gdb -ex "symbol-file bin/kernel64.elf" -ex "set architecture i386:x86-64" -ex "target remote :1234"
