---
title: "nos at work: a demo"
description: "A short video of a three-host nos cluster driven from its panel: the nos kernel itself, then several Linux VMs on different hosts, their disks on fault-tolerant storage and the VMs on one private network, from Debian's public cloud image to iperf3 between hosts."
updated: 2026-11-16
---

The goal of the video is to run several Linux virtual machines on different nodes of a cluster, all booted into nos, with their disks on fault-tolerant storage and the machines on one private network. It is one session at the management panel of a small nos cluster with three hosts. One is a bare-metal Intel server at Hetzner, an EX44, with nos running directly on the hardware. The other two are nos instances in KVM virtual machines on a second server, an AMD AX41. The only edits are cuts: the wait for a download and the wait for a VM to boot are taken out, and everything else plays as it happened.

## What it shows

1. **The hosts.** All three are booted into nos, and one of them is a real Hetzner EX44. The panel talks to each host's API over mutual TLS and sees all three.
2. **The nos kernel itself.** The EX44's System page lists the kernel's own tasks and modules: the hypervisor, storage, the network. Its overview gives the version, the CPU and the boot parameters. Guest memory comes in 2 MB pages from a pool of huge pages the kernel sets aside at boot (`hugepages=32G`).
3. **An image from a public link.** The host downloads Debian 13's cloud image, converts it from qcow2 and writes it into NBS, the cluster's block storage. There it is frozen read-only, and VMs are made from its clones.
4. **A network across the hosts.** vnet gives VMs on different hosts one subnet, a gateway and a way out to the internet, and a VM on it can get a public address too. The state of the cluster's networks is kept on every host and agreed through Raft.
5. **Two VMs on two hosts.** Each has two CPUs and a disk that is a clone of the image. Both are on the new network, behind a cluster firewall that lets in only traffic from that network. cloud-init gives the image's user the password typed into the form.
6. **Storage that is green.** Every chunk of every volume is kept on three hosts, and NBS's metadata is agreed through Raft, so the storage keeps working when a host goes down.
7. **The consoles and iperf3.** The VMs' serial consoles stream into the panel. We log in on both, install iperf3 from Debian's mirrors (the VMs reach the internet through NAT), and measure TCP from one VM to the other. The traffic crosses an encrypted tunnel between the hosts, ChaCha20-Poly1305 running on CRYPTOGAMS' assembly.

## Read more

- [NBS: replicated block storage](/blog/nbs-block-storage) covers how volumes, replicas and the metadata service work.
- [vnet: VM networks across hosts](/blog/vnet-networks-across-hosts) covers the cluster's networks, the firewalls and the tunnel.
- [The hypervisor](/hypervisor) covers what runs the VMs.

## The narration

nos is a minimal operating system for servers, with only the components a server needs. Its kernel is a small amount of C++ that you can read and review. Everything else, the device drivers, the hypervisor, the filesystems, networking and storage, is written in safe Rust. Safe Rust helps rule out out-of-bounds memory access and data races, bugs that keep turning up in the C code of Linux.

In this demo we run several Linux virtual machines on different nodes of the cluster, all booted into nos, with their disks on fault-tolerant storage and the machines on a shared private network.

This is the management panel. All three hosts of the cluster are booted into nos. One of them is a real server, a Hetzner EX44; the other two run in virtual machines on a second server. And this is the nos kernel itself on that server: its tasks and modules, the hypervisor, storage and the network. Here are the kernel's version, the CPU and the boot parameters too. Guest memory comes in 2 MB pages, from a pool of huge pages the kernel sets aside at boot.

First, an image for virtual machines, made from a public link: the Debian 13 cloud image. The host has downloaded it and written it into NBS, the cluster's block storage. The image is ready.

Next, a virtual network that machines on different hosts share: a subnet, a gateway and a way out to the internet. A machine on this network can get a public address too. The networks' state is kept on every host and agreed through Raft, so the network survives a host failure.

The first machine runs on this host: two CPUs, a disk from the image, the new network, a password to log in, and a firewall that lets in only traffic from this network. It is made. We start it right from the list. The second machine goes on another host of the cluster, with the same image, network and firewall. Both machines show up in the panel, already running.

Storage is green. Every chunk lives on three hosts, and the metadata is agreed through Raft, so losing a host does not hurt the storage.

The second machine's console: Debian has booted. We log in, install iperf3 and start its server. Now the first machine's console: we log in and install iperf3 here too. And we measure TCP throughput to the second machine on the other host. The traffic crosses an encrypted tunnel between the hosts.

nos is open source, and enthusiasts are welcome: try it, or help with development. The code is on GitHub, and there is more on the website.
