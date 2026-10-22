#!/usr/bin/env bash
#
# Builds the site's image tagged with its version; with --upload ships it to
# the server over ssh and starts it there, with --up starts it here.
#
#   scripts/release.sh                          build IMAGE_NAME:VERSION
#   scripts/release.sh --upload [user@host]     build for the server, ship it, start it there
#   scripts/release.sh --up                     build, start it on this machine
#
# The version is IMAGE_VERSION from .env, or else the VERSION file; the name is
# IMAGE_NAME (nos-web). The environment outranks .env, as it does for compose.
# The image carries SITE_URL, made from DOMAIN in .env, so build it with the
# .env of the deployment it is for.
#
# --upload checks that the destination has docker and its compose plugin,
# builds for the destination's platform (natively: only the image's last
# stage is that platform's), sends the image -- docker save | gzip | ssh
# docker load, skipped when the destination already has this very build --
# then docker-compose.yml, the Caddyfile and this .env with IMAGE_NAME and
# IMAGE_VERSION pinned into DEPLOY_DIR, runs `docker compose up -d` there,
# and waits until the web container is healthy and runs the image it was
# sent. Anything short of that is reported, with the containers' last log
# lines, and the exit status is 1.
#
# Options:
#   --upload [dest]     ship to dest (user@host, or a Host of ~/.ssh/config);
#                       DEPLOY_HOST from .env when not given
#   --up                start the image here: docker compose up -d, and wait for it
#   --no-build          ship or start the image already built under this tag
#   --platform os/arch  build for this platform: by default the destination's
#                       with --upload, this machine's otherwise
#   -h, --help          this text
#
# .env also gives DEPLOY_DIR (the directory on the destination, nos-web, under
# the remote home unless absolute) and DEPLOY_SSH_OPTS (extra ssh options,
# e.g. "-p 2222 -i ~/.ssh/deploy_key").

set -euo pipefail

usage() {
    sed -n '3,/^$/s/^# \{0,1\}//p' "$0"
}

die() {
    echo "release: $*" >&2
    exit 1
}

note() {
    echo "release: $*"
}

cd "$(dirname "$0")/.."
web=$(pwd)
env_file=$web/.env

# The value of KEY in .env -- its last KEY= line, quotes stripped -- read, not
# sourced: .env is compose's format, not a shell script.
env_get() {
    local line value=""
    [ -f "$env_file" ] || return 0
    while IFS= read -r line || [ -n "$line" ]; do
        line=${line%$'\r'}
        case $line in
            "$1="*) value=${line#"$1="} ;;
        esac
    done < "$env_file"
    case $value in
        \"*\") value=${value#\"}; value=${value%\"} ;;
        \'*\') value=${value#\'}; value=${value%\'} ;;
    esac
    printf '%s' "$value"
}

# setting NAME DEFAULT: the environment's value, else .env's, else DEFAULT.
setting() {
    local value=${!1:-}
    [ -n "$value" ] || value=$(env_get "$1")
    [ -n "$value" ] || value=$2
    printf '%s' "$value"
}

upload=0
up=0
build=1
dest=""
platform=""
while [ $# -gt 0 ]; do
    case $1 in
        --upload)
            upload=1
            if [ $# -gt 1 ] && [ "${2#-}" = "$2" ]; then
                dest=$2
                shift
            fi
            ;;
        --upload=*) upload=1; dest=${1#--upload=} ;;
        --up) up=1 ;;
        --no-build) build=0 ;;
        --platform)
            [ $# -gt 1 ] || die "--platform needs os/arch"
            platform=$2
            shift
            ;;
        --platform=*) platform=${1#--platform=} ;;
        -h | --help) usage; exit 0 ;;
        *) die "unknown argument: $1 (see --help)" ;;
    esac
    shift
done
[ $upload = 0 ] || [ $up = 0 ] || die "--up starts it here and --upload there: give one"

[ -f "$env_file" ] || die "no .env in $web: cp .env.example .env and fill it in"

name=$(setting IMAGE_NAME nos-web)
version=$(setting IMAGE_VERSION "")
if [ -z "$version" ]; then
    [ -f VERSION ] || die "no IMAGE_VERSION and no VERSION file"
    version=$(tr -d ' \t\r\n' < VERSION)
fi
[[ $name =~ ^[a-z0-9]+([._-][a-z0-9]+)*(/[a-z0-9]+([._-][a-z0-9]+)*)*$ ]] ||
    die "IMAGE_NAME is not an image name: '$name'"
[[ $version =~ ^[A-Za-z0-9_][A-Za-z0-9_.-]{0,127}$ ]] ||
    die "the version is not an image tag: '$version'"
[ "$version" != latest ] || die "latest is what an unversioned build is called: give the release a version"
ref=$name:$version

revision=""
if git -C "$web" rev-parse --git-dir > /dev/null 2>&1; then
    revision=$(git -C "$web" rev-parse --short=12 HEAD)
    if [ -n "$(git -C "$web" status --porcelain -- . ../docs)" ]; then
        revision=$revision-dirty
    fi
fi

dir=.
tmp=$(mktemp -d "${TMPDIR:-/tmp}/nos-release.XXXXXX")
ssh_args=()
cleanup() {
    # (An empty array expanded under set -u is an error in the bash macOS ships.)
    if [ -n "$dest" ] && [ ${#ssh_args[@]} -gt 0 ]; then
        ssh "${ssh_args[@]}" -O exit "$dest" > /dev/null 2>&1 || true
    fi
    rm -rf "$tmp"
}
trap cleanup EXIT

# A command line where the site runs: the destination over ssh, or here.
# Every value put into one is checked against a charset that needs no quoting.
on_target() {
    if [ -n "$dest" ]; then
        ssh "${ssh_args[@]}" "$dest" "$1"
    else
        sh -c "$1"
    fi
}

if [ $upload = 1 ]; then
    [ -n "$dest" ] || dest=$(setting DEPLOY_HOST "")
    [ -n "$dest" ] || die "--upload needs a destination: --upload user@host, or DEPLOY_HOST in .env"
    [[ $dest =~ ^[A-Za-z0-9_.@:%/-]+$ && $dest != -* ]] || die "not an ssh destination: '$dest'"
    dir=$(setting DEPLOY_DIR nos-web)
    [[ $dir =~ ^[A-Za-z0-9_./-]+$ && $dir != -* ]] || die "DEPLOY_DIR has characters it should not: '$dir'"
    extra=$(setting DEPLOY_SSH_OPTS "")
    if [ -n "$extra" ]; then
        read -r -a ssh_args <<< "$extra"
    fi
    # One connection for the whole release: one login, one host key prompt.
    ssh_args+=(-o ControlMaster=auto -o "ControlPath=$tmp/ssh" -o ControlPersist=600)

    note "checking $dest"
    info=$(on_target 'docker version -f "{{.Server.Os}}/{{.Server.Arch}} {{.Server.Version}}" && docker compose version --short') ||
        die "$dest: docker or its compose plugin is missing there, or this user may not use them"
    read -r remote_platform remote_docker <<< "$(printf '%s\n' "$info" | sed -n 1p)"
    remote_compose=$(printf '%s\n' "$info" | sed -n 2p)
    note "$dest runs docker $remote_docker ($remote_platform), compose $remote_compose"
    [ -n "$platform" ] || platform=$remote_platform
    [ "$platform" = "$remote_platform" ] || die "asked to build for $platform, but $dest runs $remote_platform"
else
    [ -n "$platform" ] || platform=$(docker version -f '{{.Server.Os}}/{{.Server.Arch}}')
fi
[[ $platform =~ ^[a-z0-9]+/[a-z0-9]+(/[a-z0-9]+)?$ ]] || die "not a platform: '$platform'"

if [ $build = 1 ]; then
    note "building $ref for $platform${revision:+ from $revision}"
    # Compose builds it as `docker compose up --build` would, from the same
    # file, args and .env. No attestation manifests: they are of no use on the
    # server, and an older docker load cannot read them.
    IMAGE_NAME=$name IMAGE_VERSION=$version REVISION=$revision \
        DOCKER_DEFAULT_PLATFORM=$platform BUILDX_NO_DEFAULT_ATTESTATIONS=1 \
        docker compose build web
fi
docker image inspect "$ref" > /dev/null 2>&1 || die "there is no image $ref here: build it, or leave out --no-build"
built_for=$(docker image inspect -f '{{.Os}}/{{.Architecture}}' "$ref")
[ "$built_for" = "$platform" ] || die "$ref is built for $built_for, not $platform"
note "$ref is ready ($built_for, $(docker image inspect -f '{{index .Config.Labels "org.opencontainers.image.revision"}}' "$ref"))"

if [ $upload = 0 ] && [ $up = 0 ]; then
    exit 0
fi

# Before anything changes where the site runs: Caddy, the very image the
# compose file names, must accept the Caddyfile.
caddy_image=$(docker compose config --images caddy)
note "checking the Caddyfile with $caddy_image"
if ! out=$(docker run --rm -e "DOMAIN=$(setting DOMAIN '')" -e "ACME_EMAIL=$(setting ACME_EMAIL '')" \
        -v "$web/Caddyfile:/etc/caddy/Caddyfile:ro" "$caddy_image" \
        caddy validate --config /etc/caddy/Caddyfile --adapter caddyfile 2>&1); then
    printf '%s\n' "$out" >&2
    die "Caddy does not accept the Caddyfile; nothing was changed"
fi

if [ $upload = 1 ]; then
    # The image's creation time is in its config, which a save and a load keep:
    # the same time there means the same build, already delivered.
    created=$(docker image inspect -f '{{.Created}}' "$ref")
    there=$(on_target "docker image inspect -f '{{.Created}}' $ref 2> /dev/null" || true)
    if [ "$created" = "$there" ]; then
        note "$dest already has this build of $ref"
    else
        note "sending $ref to $dest"
        docker save "$ref" | gzip -c | on_target 'gunzip -c | docker load'
    fi

    # The server's .env is this one, with the image it was sent pinned in it.
    grep -v -E '^(IMAGE_NAME|IMAGE_VERSION)=' "$env_file" > "$tmp/env" || true
    {
        echo
        echo "# Pinned by scripts/release.sh: the image this server runs."
        echo "IMAGE_NAME=$name"
        echo "IMAGE_VERSION=$version"
    } >> "$tmp/env"
    on_target "mkdir -p $dir"
    on_target "cat > $dir/.docker-compose.yml.new && mv $dir/.docker-compose.yml.new $dir/docker-compose.yml" < "$web/docker-compose.yml"
    on_target "umask 077 && cat > $dir/.env.new && mv $dir/.env.new $dir/.env" < "$tmp/env"
    # Not renamed into place like those two: a running Caddy has the Caddyfile
    # bind-mounted, which pins the file's inode, so a new file under the old
    # name would never reach it. It is rewritten in place, from a copy that
    # has arrived whole.
    on_target "cat > $dir/.Caddyfile.new && cat $dir/.Caddyfile.new > $dir/Caddyfile && rm $dir/.Caddyfile.new" < "$web/Caddyfile"
    note "sent docker-compose.yml, Caddyfile and .env to $dest:$dir"
fi

where=${dest:-this machine}
note "starting $ref on $where"
on_target "cd $dir && IMAGE_NAME=$name IMAGE_VERSION=$version docker compose up -d --no-build --remove-orphans"

# Up is not enough: the web container must pass its healthcheck, and run the
# image this release is about rather than one left from before.
result=$(on_target 'cd '"$dir"' && cid=$(docker compose ps -q web) && [ -n "$cid" ] || { echo "none -"; exit 0; }
i=0
while :; do
    state=$(docker inspect -f "{{if .State.Health}}{{.State.Health.Status}}{{else}}{{.State.Status}}{{end}}" "$cid")
    case $state in healthy | unhealthy | exited | dead) break ;; esac
    i=$((i + 1))
    [ $i -lt 120 ] || break
    sleep 1
done
echo "$state $(docker inspect -f "{{.Config.Image}}" "$cid")"')
read -r state running <<< "$result"
if [ "$state" != healthy ] || [ "$running" != "$ref" ]; then
    on_target "cd $dir && docker compose ps && docker compose logs --tail 40 web" >&2 || true
    die "on $where the web container is $state, running $running -- expected a healthy $ref"
fi

# The Caddyfile is a bind mount: a running Caddy takes the new one by a
# reload, which keeps the running configuration should it fail; a Caddy
# that is not running is started afresh on it. Then it must stay up.
result=$(on_target 'cd '"$dir"' && caddy=$(docker compose ps -aq caddy)
state=$(docker inspect -f "{{.State.Status}}" "$caddy" 2> /dev/null || echo none)
if [ "$state" = running ]; then
    out=$(docker compose exec -T caddy caddy reload --config /etc/caddy/Caddyfile --adapter caddyfile 2>&1) || { echo "refused-reload"; echo "$out" >&2; exit 0; }
else
    docker compose up -d --no-build --force-recreate caddy > /dev/null 2>&1 || { echo "not-started"; exit 0; }
fi
sleep 2
caddy=$(docker compose ps -aq caddy)
echo "$(docker inspect -f "{{.State.Status}}/{{.RestartCount}}" "$caddy" 2> /dev/null || echo none)"')
case $result in
    running/*) ;;
    refused-reload) die "Caddy on $where refused the Caddyfile, and goes on with the one it had" ;;
    *)
        on_target "cd $dir && docker compose ps && docker compose logs --tail 40 caddy" >&2 || true
        die "Caddy on $where is not running (${result%%/*})"
        ;;
esac
on_target "cd $dir && docker compose ps"
note "$ref is up on $where"
if [ $upload = 1 ]; then
    versions=$(on_target "docker image ls $name --format '{{.Tag}}'" | sort | tr '\n' ' ')
    note "versions on $dest: $versions(roll back: set IMAGE_VERSION in $dir/.env, then docker compose up -d --no-build there)"
fi
