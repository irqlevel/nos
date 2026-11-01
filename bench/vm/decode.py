#!/usr/bin/env python3
"""decode.py <console log> <dir>: the suite's archive out of a guest's
console -- the base64 lines between BENCH-ARCHIVE-BEGIN and -END, anything
else that came between left out -- held to the sha256 the guest printed,
and unpacked into <dir>. Prints the BENCH lines too."""
import base64
import hashlib
import io
import os
import re
import sys
import tarfile


def main():
    log, out = sys.argv[1], sys.argv[2]
    with open(log, "rb") as f:
        text = f.read().replace(b"\r", b"").decode("ascii", "replace")
    lines = text.split("\n")
    want = None
    for line in lines:
        if line.startswith("BENCH archive"):
            want = line.split()[5]
    begin = max(i for i, line in enumerate(lines) if line.startswith("BENCH-ARCHIVE-BEGIN"))
    end = next(i for i in range(begin, len(lines)) if lines[i].startswith("BENCH-ARCHIVE-END"))
    b64 = "".join(line for line in lines[begin + 1:end] if re.fullmatch(r"[A-Za-z0-9+/=]+", line))
    data = base64.b64decode(b64)
    got = hashlib.sha256(data).hexdigest()
    if got != want:
        sys.exit("sha256 %s, the guest said %s" % (got, want))
    os.makedirs(out, exist_ok=True)
    with open(out + ".tar.xz", "wb") as f:
        f.write(data)
    with tarfile.open(fileobj=io.BytesIO(data), mode="r:xz") as tar:
        if hasattr(tarfile, "data_filter"):
            tar.extractall(out, filter="data")
        else:
            # A Python without extraction filters (before 3.11.4): what the
            # data filter refuses refused here -- a path out of `out`, and
            # anything but files and directories.
            for m in tar.getmembers():
                name = os.path.normpath(m.name)
                if os.path.isabs(name) or name == ".." or name.startswith("../") or not (m.isfile() or m.isdir()):
                    sys.exit("archive member %r refused" % m.name)
            tar.extractall(out)
    print("archive ok: %d bytes, sha256 %s -> %s" % (len(data), got, out))


main()
