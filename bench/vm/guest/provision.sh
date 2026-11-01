#!/bin/bash
# Run once by cloud-init (runcmd) at the image's first boot, under KVM on the
# server's Linux: what the suite needs, and nothing left that wakes by itself.
set -ex
exec > /var/log/bench-provision.log 2>&1
B=/opt/bench
cd $B

# The tree the build benchmark compiles, held to kernel.org's sums
V=6.6.60
curl -fsSLo linux-$V.tar.xz https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-$V.tar.xz
curl -fsSL https://cdn.kernel.org/pub/linux/kernel/v6.x/sha256sums.asc | grep " linux-$V.tar.xz\$" > linux.sha256
sha256sum -c linux.sha256
mv linux-$V.tar.xz linux.tar.xz

# PostgreSQL: the suite's settings, root a superuser, the database it fills
PGV=$(ls /etc/postgresql | sort -V | tail -1)
install -m 644 $B/pg-bench.conf /etc/postgresql/$PGV/main/conf.d/bench.conf
systemctl restart postgresql
sudo -u postgres createuser -s root
sudo -u postgres createdb -O root bench

# Nothing that wakes by itself while the suite runs
for t in apt-daily.timer apt-daily-upgrade.timer man-db.timer fstrim.timer e2scrub_all.timer \
         motd-news.timer dpkg-db-backup.timer ua-timer.timer update-notifier-download.timer \
         update-notifier-motd.timer sysstat-collect.timer sysstat-summary.timer; do
    systemctl disable --now $t || true
done
apt-get purge -y snapd unattended-upgrades || true
apt-get autoremove -y --purge

# Any en* by DHCP: cloud-init's netplan names this machine's MAC
rm -f /etc/netplan/50-cloud-init.yaml
install -m 600 $B/60-bench.yaml /etc/netplan/60-bench.yaml
# root at the serial console
install -d /etc/systemd/system/serial-getty@ttyS0.service.d
install -m 644 $B/autologin.conf /etc/systemd/system/serial-getty@ttyS0.service.d/autologin.conf
# the suite, at boot, when AUTORUN is there
install -m 644 $B/bench.service /etc/systemd/system/bench.service
chmod 755 $B/run.sh
systemctl daemon-reload
systemctl enable bench.service

# cloud-init has done its work for good: the image boots alike under both hosts
touch /etc/cloud/cloud-init.disabled
apt-get clean
journalctl --vacuum-size=16M || true
uname -r > $B/provisioned-under-kernel
ls /usr/lib/linux-tools/ > $B/linux-tools
ls /boot > $B/boot-files
fstrim -av
echo done > $B/provisioned
