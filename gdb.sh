#!/bin/bash

gdb -ex "symbol-file kernel64.elf" -ex "set architecture i386:x86-64" -ex "target remote :1234"
