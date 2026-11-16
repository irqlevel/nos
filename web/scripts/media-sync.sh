#!/bin/sh
# The demo videos (and their posters and cards), out of the web image into the
# volume Caddy serves /video/ from: docker-compose.yml's `media` service runs
# this at every release, before `web` starts, then exits.
#
# A file is copied under a temporary name and renamed into place, so a
# download Caddy has open goes on reading the file it opened; one the image no
# longer carries is removed. `web` starts only once this is done, so no page
# names a file (?v= of its content) before the volume holds that content.
set -eu
src=/app/public/video
dst=/media/video
mkdir -p "$dst"
for f in "$src"/*; do
    [ -f "$f" ] || continue
    name=${f##*/}
    if ! cmp -s "$f" "$dst/$name" 2>/dev/null; then
        cp "$f" "$dst/.$name.tmp"
        mv -f "$dst/.$name.tmp" "$dst/$name"
        echo "media: $name"
    fi
done
for f in "$dst"/* "$dst"/.*.tmp; do
    [ -e "$f" ] || continue
    name=${f##*/}
    if [ ! -e "$src/$name" ]; then
        rm -f "$f"
        echo "media: $name removed"
    fi
done
