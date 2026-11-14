# The demo stand

Three nos hosts for trying the management panel on: the EX44, nos on its
own hardware, running the panel's VM; and two neighbours, `ax41a` and
`ax41b`, nos VMs under KVM on the AX41 -- a cluster's three hosts, and
NBS's, made from the panel. Two machines of this project's own: their
addresses, the AX41's uplink and the EX44's root are the scripts' defaults,
each a variable at its top.

| Script | Where it runs | What it does |
|---|---|---|
| `build.sh` | here | the panel's bundle (`scripts/panel-bundle.py`, into `out/demo/`); in the nos-builder image the kernel and modules, `nos-test.iso` with the UDP shell, and the system image, its `/lib/panel/bundle` naming the bundle at the AX41 |
| `ax41-up.sh <bundle>` | here | the neighbours' files onto the AX41 (`/root/nos-demo`), then there: the network, DHCP, the bundle's HTTP server, the two VMs made and started |
| `ax41-down.sh` | here | the neighbours powered off, DHCP and HTTP stopped, the network taken away; their roots and pools kept |
| `ex44-deploy.sh <image>` | here | the EX44 to the image for one boot: the panel's VM deleted for `/etc/rc` to make anew, the others stopped and started again; nos's root made ready in Ubuntu -- ipcnet, the registry and NBS in `/etc/rc`, a fresh `hosts.toml`, a cluster before kept aside |
| `neighbours.sh` | here | the EX44's panel's key let in on each neighbour, and each one's address and key said, for the Hosts page |
| `ax41/demo-net.sh up\|down\|show` | the AX41 | the bridge `nosbr0` (10.77.0.0/24), NAT out; from the EX44 alone each neighbour's ipcnet (39241, 39242) and API (39251, 39252 to their 39234), and the bundle's port (39299); every rule tagged `nos-demo` |
| `ax41/demo-make.sh <name>...` | the AX41 | a neighbour's root -- ipcnet, cluster, nbs-mds, nbs-cs and api, its `hosts.toml`, an `api.toml` taking the EX44 alone, `/etc/rc` -- and a 40 GiB pool, its partition's GUID fixed |
| `ax41/demo-vm.sh start\|stop\|status <name>` | the AX41 | a neighbour under KVM, its tap on the bridge; `stop` powers it off through its UDP shell |
| `ax41/dhcp.py`, `ax41/nsh.py` | the AX41 | DHCP on the bridge, each neighbour's MAC its address; one command to a neighbour's UDP shell |

In order:

```
scripts/demo-stand/build.sh
scripts/demo-stand/ax41-up.sh out/demo/nos-panel-<version>.tar.gz
DEMO_SSH_CONFIG=<ssh config> scripts/demo-stand/ex44-deploy.sh nos-x86_64.squashfs
DEMO_SSH_CONFIG=<ssh config> scripts/demo-stand/neighbours.sh
```

`ssh_config.example` is the ssh config the EX44's two scripts take: nos's
sshd and Ubuntu's at one address, Ubuntu's host key in a known_hosts of its
own. The EX44's nos boots once: a reboot there is Ubuntu again, and
`ex44-deploy.sh` brings it back.

Then, in the panel at `https://<the EX44>:39233`: the Hosts page adds each
neighbour by the address and key `neighbours.sh` said; the cluster's page
on the EX44 makes the cluster (`init`), its status says each neighbour's
name, key and address for an invitation -- the address the AX41's and the
port its ipcnet is forwarded at, `65.109.93.213:39241` for `ax41a` -- and
each neighbour joins with the token the invitation answered; NBS's page
makes NBS on one host, adds the others, and each host's own page adds its
device: the EX44's spare partition, a neighbour's `part:<GUID>` that
`demo-make.sh` printed.
