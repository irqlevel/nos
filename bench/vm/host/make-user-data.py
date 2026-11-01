#!/usr/bin/env python3
"""cloud-init user-data for the golden image's one provisioning boot:
the guest files of the KVM-vs-nos suite into /opt/bench, the packages the
suite runs, provision.sh, then power off.

    make-user-data.py <guest dir> <root's public key file>  > user-data
"""
import base64
import json
import os
import sys

FILES = ["run.sh", "provision.sh", "bench.service", "autologin.conf",
         "60-bench.yaml", "pg-bench.conf"]
PACKAGES = ["postgresql", "sysbench", "fio", "7zip", "rt-tests",
            "linux-tools-generic", "build-essential", "flex", "bison", "bc",
            "libelf-dev", "libssl-dev", "jq"]


def main():
    guest, key_file = sys.argv[1], sys.argv[2]
    with open(key_file) as f:
        key = f.read().strip()
    out = ["#cloud-config",
           "hostname: bench",
           "disable_root: false",
           "ssh_pwauth: false",
           "ssh_authorized_keys:",
           "  - " + json.dumps(key),
           "package_update: true",
           "package_upgrade: true",
           "packages:"]
    out += ["  - " + p for p in PACKAGES]
    out.append("write_files:")
    for name in FILES:
        with open(os.path.join(guest, name), "rb") as f:
            data = base64.b64encode(f.read()).decode()
        mode = "0755" if name.endswith(".sh") else "0644"
        out += ["  - path: /opt/bench/" + name,
                "    encoding: b64",
                "    permissions: '" + mode + "'",
                "    content: " + data]
    out += ["runcmd:",
            "  - [bash, -c, 'if bash /opt/bench/provision.sh; then echo PROVISION-OK > /dev/ttyS0;"
            " else echo PROVISION-FAILED > /dev/ttyS0; tail -40 /var/log/bench-provision.log > /dev/ttyS0; fi']",
            "power_state:",
            "  mode: poweroff",
            "  timeout: 120",
            "  condition: true"]
    print("\n".join(out))


main()
