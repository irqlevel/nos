#!/bin/sh
# nos's management panel, installed in its VM from its bundle -- what
# nos-panel-fetch runs once it has the bundle unpacked in /opt/nos-panel, as
# root (the seed `api panel create` made has cloud-init run that):
#
# - the user it runs as, nos-panel, which alone may read the seed and the
#   key of its certificate;
# - /var/lib/nos-panel, its data -- the panel's SQLite, Caddy's state --
#   which a new bundle installed over this one leaves as it is;
# - two services: the panel (Node, on 127.0.0.1:3000) and Caddy in front of
#   it, HTTPS and HTTP/2 on 443 with the certificate nos made, which `api
#   panel` on the host prints the fingerprint of.
#
# Run again, it installs the bundle beside it anew and restarts both.
set -eu

dir=$(cd "$(dirname "$0")" && pwd)
user=nos-panel
data=/var/lib/nos-panel
etc=/etc/nos-panel

for f in "$etc/seed.json" "$etc/tls/cert.pem" "$etc/tls/key.pem"; do
    if [ ! -f "$f" ]; then
        echo "nos-panel: no $f -- it comes in the seed api panel create makes" >&2
        exit 1
    fi
done

if ! id "$user" >/dev/null 2>&1; then
    useradd --system --home-dir "$data" --no-create-home --shell /usr/sbin/nologin "$user"
fi
install -d -m 0700 -o "$user" -g "$user" "$data" "$data/cache" "$data/caddy"

# The seed and the key are the panel's to read: root writes them, the
# panel's group reads them, nobody else does.
chown root:"$user" "$etc" "$etc/tls" "$etc/seed.json" "$etc/tls/key.pem"
chmod 0750 "$etc" "$etc/tls"
chmod 0640 "$etc/seed.json" "$etc/tls/key.pem"
chmod 0644 "$etc/tls/cert.pem"

# Next.js keeps its cache beside the app, which is read-only to it: the
# cache is the data's.
ln -sfn "$data/cache" "$dir/app/.next/cache"

install -m 0644 "$dir/systemd/nos-panel.service" /etc/systemd/system/nos-panel.service
install -m 0644 "$dir/systemd/nos-panel-caddy.service" /etc/systemd/system/nos-panel-caddy.service
systemctl daemon-reload
systemctl enable nos-panel.service nos-panel-caddy.service
systemctl restart nos-panel.service nos-panel-caddy.service

# Said on the console too: hv keeps it in the VM's console.log on the host.
echo "nos-panel: $(cat "$dir/VERSION") installed -- HTTPS on 443" | tee /dev/console || true
