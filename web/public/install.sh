#!/bin/sh
# https://nos-os.tech/install.sh: nos onto this server's disk, from the Linux
# it runs now -- a provider's rescue system. It runs the installer of nos's
# newest release (install.sh at https://github.com/irqlevel/nos/releases),
# which fetches that release's system image, checks it against the
# release's SHA256SUMS and runs its dialog -- or nos-install, with the
# arguments given:
#
#   curl -fsSL https://nos-os.tech/install.sh | sh
#   curl -fsSL https://nos-os.tech/install.sh | sh -s -- disk /dev/sdb --wipe \
#       --network-here --ssh-key /root/.ssh/authorized_keys
#
# NOS_VERSION=vX.Y.Z runs that release's instead. The rest is in
# https://github.com/irqlevel/nos/blob/master/docs/install.md
#
# The whole of it is a function, called on the last line: a download cut
# short runs nothing.

nos_get() {
    set -eu
    if [ -n "${NOS_VERSION:-}" ]; then
        nos_url=https://github.com/irqlevel/nos/releases/download/$NOS_VERSION/install.sh
    else
        nos_url=https://github.com/irqlevel/nos/releases/latest/download/install.sh
    fi
    nos_script=$(mktemp /tmp/nos-install-sh.XXXXXX)
    trap 'rm -f "$nos_script"' EXIT
    trap 'exit 1' HUP INT TERM
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL --retry 3 -o "$nos_script" "$nos_url" || { echo "nos: $nos_url cannot be fetched" >&2; exit 1; }
    elif command -v wget >/dev/null 2>&1; then
        wget -q -O "$nos_script" "$nos_url" || { echo "nos: $nos_url cannot be fetched" >&2; exit 1; }
    else
        echo "nos: this needs curl or wget" >&2
        exit 1
    fi
    sh "$nos_script" "$@"
}

nos_get "$@"
