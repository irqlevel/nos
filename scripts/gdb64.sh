#!/bin/bash
cd "$(dirname "$0")/.."
gdb -x scripts/gdb-debug64
