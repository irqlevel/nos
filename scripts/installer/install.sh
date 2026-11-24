#!/bin/sh
# install.sh: nos onto this server's disk, from the Linux it runs now -- a
# provider's rescue system (docs/install.md, "From a provider's rescue
# system"). It fetches the system image of the release it comes with,
# checks it against that release's SHA256SUMS, mounts it or unpacks it with
# unsquashfs, and runs the installer in it: the dialog (nos-setup), or nos-install with the arguments
# given. https://nos-os.tech/install.sh runs the newest release's:
#
#   curl -fsSL https://nos-os.tech/install.sh | sh
#   curl -fsSL https://nos-os.tech/install.sh | sh -s -- disk /dev/sdb --wipe \
#       --network-here --ssh-key /root/.ssh/authorized_keys
#
# What the install runs and this system lacks -- GRUB for BIOS and UEFI,
# mkfs, efibootmgr, the dialog's whiptail -- is installed first where
# apt-get is (Debian, Ubuntu: the rescue systems of most providers), and
# named elsewhere. Environment:
#
#   NOS_VERSION=vX.Y.Z    that release's image, not this script's
#   NOS_RELEASE_URL=URL   a directory holding nos-x86_64.squashfs and its
#                         SHA256SUMS (https://, file://), not a release's
#   NOS_NO_PACKAGES=1     install nothing: name what is missing
#
# Plain sh, for curl | sh: the whole of it is functions, the last line calls
# them, and a download cut short runs nothing.

NOS_REPO=https://github.com/irqlevel/nos
# The release this script comes with: release.yml writes its tag here
NOS_RELEASE=@NOS_RELEASE@
NOS_IMAGE=nos-x86_64.squashfs

nos_say() {
    echo "nos: $*"
}

nos_die() {
    echo "nos: $*" >&2
    exit 1
}

# URL into FILE
nos_fetch() {
    case "$1" in
        file://*) cp "${1#file://}" "$2" ;;
        *)
            if command -v curl >/dev/null 2>&1; then
                curl -fsSL --retry 3 -o "$2" "$1"
            elif command -v wget >/dev/null 2>&1; then
                wget -q -O "$2" "$1"
            else
                nos_die "this needs curl or wget"
            fi ;;
    esac
}

# What nos-install runs that this system lacks, as the Debian and Ubuntu
# packages that have it: for $1 "other" -- update, boot, status, rollback --
# e2fsprogs, GRUB's grub-editenv and efibootmgr; for "disk" also what makes
# a disk, GRUB for BIOS and UEFI among it; for "dialog" also the dialog's
# tools. $2 is 1 for --panel, whose password's hash needs python3.
nos_lacks() {
    nos_p=
    nos_tools="lsblk:util-linux blkid:util-linux e2fsck:e2fsprogs debugfs:e2fsprogs grub-editenv:grub2-common ip:iproute2 unsquashfs:squashfs-tools"
    if [ "$1" != other ]; then
        nos_tools="$nos_tools sfdisk:fdisk mkfs.ext4:e2fsprogs mkfs.vfat:dosfstools grub-install:grub2-common udevadm:udev"
        [ -d /usr/lib/grub/i386-pc ] || nos_p="$nos_p grub-pc-bin"
        [ -d /usr/lib/grub/x86_64-efi ] || nos_p="$nos_p grub-efi-amd64-bin"
    fi
    [ "$1" != dialog ] || nos_tools="$nos_tools whiptail:whiptail ssh-keygen:openssh-client curl:curl"
    if [ "$1" = dialog ] || [ "${2:-0}" = 1 ]; then
        nos_tools="$nos_tools python3:python3"
    fi
    for nos_t in $nos_tools; do
        command -v "${nos_t%%:*}" >/dev/null 2>&1 || nos_p="$nos_p ${nos_t#*:}"
    done
    if [ -d /sys/firmware/efi ] && ! command -v efibootmgr >/dev/null 2>&1; then
        nos_p="$nos_p efibootmgr"
    fi
    for nos_t in $nos_p; do
        echo "$nos_t"
    done | sort -u | tr '\n' ' ' | sed 's/ $//'
}

nos_packages() {
    nos_lack=$(nos_lacks "$1" "${2:-0}")
    [ -n "$nos_lack" ] || return 0
    if [ "${NOS_NO_PACKAGES:-0}" = 1 ] || ! command -v apt-get >/dev/null 2>&1; then
        nos_say "this system lacks $nos_lack -- nos-install says what it cannot do without"
        return 0
    fi
    nos_say "installing what the install needs: $nos_lack"
    DEBIAN_FRONTEND=noninteractive apt-get -qq update >/dev/null 2>&1 || true
    # shellcheck disable=SC2086 # a word a package
    if ! DEBIAN_FRONTEND=noninteractive apt-get -y -qq install --no-install-recommends $nos_lack >/dev/null; then
        nos_say "apt-get did not install all of it -- nos-install says what it cannot do without"
    fi
}

nos_cleanup() {
    [ -n "${nos_dir:-}" ] || return 0
    if grep -q " $nos_dir/image " /proc/mounts 2>/dev/null; then
        umount "$nos_dir/image" 2>/dev/null || umount -l "$nos_dir/image" 2>/dev/null || true
    fi
    rm -rf "$nos_dir"
}

nos_main() {
    set -eu
    [ "$(id -u)" = 0 ] || nos_die "run it as root"
    [ "$(uname -m)" = x86_64 ] || nos_die "nos's system image is x86-64's, and this machine is $(uname -m)"
    nos_release=${NOS_VERSION:-$NOS_RELEASE}
    case "$nos_release" in
        @*|'') nos_release=latest ;;
    esac
    if [ -n "${NOS_RELEASE_URL:-}" ]; then
        nos_base=${NOS_RELEASE_URL%/}
    elif [ "$nos_release" = latest ]; then
        nos_base=$NOS_REPO/releases/latest/download
    else
        nos_base=$NOS_REPO/releases/download/$nos_release
    fi

    # No arguments: the dialog, on the terminal -- not on stdin, which curl | sh
    # gives this script itself
    nos_dialog=0
    nos_panel=0
    for nos_arg in "$@"; do
        [ "$nos_arg" != --panel ] || nos_panel=1
    done
    if [ $# = 0 ]; then
        nos_dialog=1
        # true, not ":": a redirection that fails ends a non-interactive sh
        # at a special builtin, before the || is reached
        { true < /dev/tty; } 2>/dev/null ||
            nos_die "the dialog needs a terminal: in an SSH session with one (ssh -t), or give nos-install's arguments (docs/install.md)"
        nos_packages dialog "$nos_panel"
    else
        case "$1" in
            disk) nos_packages disk "$nos_panel" ;;
            update|boot|status|rollback) nos_packages other "$nos_panel" ;;
        esac
    fi

    nos_dir=$(mktemp -d /tmp/nos-install.XXXXXX)
    trap nos_cleanup EXIT
    trap 'exit 1' HUP INT TERM
    nos_say "fetching $nos_base/$NOS_IMAGE"
    nos_fetch "$nos_base/$NOS_IMAGE" "$nos_dir/$NOS_IMAGE" || nos_die "$nos_base/$NOS_IMAGE cannot be fetched"
    nos_fetch "$nos_base/SHA256SUMS" "$nos_dir/SHA256SUMS" || nos_die "$nos_base/SHA256SUMS cannot be fetched"
    nos_want=$(awk -v f="$NOS_IMAGE" '$2 == f || $2 == "*" f { print $1; exit }' "$nos_dir/SHA256SUMS")
    nos_got=$(sha256sum "$nos_dir/$NOS_IMAGE" | awk '{ print $1 }')
    [ -n "$nos_want" ] || nos_die "the release's SHA256SUMS does not have $NOS_IMAGE"
    [ "$nos_got" = "$nos_want" ] || nos_die "$NOS_IMAGE is not the release's: its sha256 is $nos_got, the release's $nos_want -- run this again"
    mkdir "$nos_dir/image"
    if ! mount -t squashfs -o loop,ro "$nos_dir/$NOS_IMAGE" "$nos_dir/image" 2>/dev/null; then
        modprobe -q squashfs 2>/dev/null || true
        modprobe -q loop 2>/dev/null || true
        if ! mount -t squashfs -o loop,ro "$nos_dir/$NOS_IMAGE" "$nos_dir/image" 2>/dev/null; then
            nos_say "this Linux cannot mount squashfs; unpacking the image with unsquashfs"
            rmdir "$nos_dir/image"
            unsquashfs -no-progress -d "$nos_dir/image" "$nos_dir/$NOS_IMAGE" >/dev/null ||
                nos_die "this Linux cannot mount the image, and unsquashfs cannot unpack it"
        fi
    fi
    nos_say "nos $(sed -n 's/^version=//p' "$nos_dir/image/VERSION") ($(sed -n 's/^revision=//p' "$nos_dir/image/VERSION")), sha256 $nos_got"

    if [ "$nos_dialog" = 1 ]; then
        [ -x "$nos_dir/image/nos-setup" ] ||
            nos_die "this release's image has no dialog: give nos-install's arguments (docs/install.md)"
        "$nos_dir/image/nos-setup" < /dev/tty
    else
        "$nos_dir/image/nos-install" "$@"
    fi
}

nos_main "$@"
