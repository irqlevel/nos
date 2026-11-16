---
title: "nos at work: a demo"
description: "A short video of a three-host nos cluster driven from its panel: a VM image from Debian's public cloud image, a network shared across the hosts, two VMs on two hosts, and a ping from one to the other."
updated: 2026-11-16
---

The video is one session at the management panel of a small nos cluster. The cluster has three hosts. One is a bare-metal Intel server at Hetzner, an EX44, with nos running directly on the hardware. The other two are nos instances in KVM virtual machines on a second server, an AMD AX41. The only edits are cuts: the wait for a download and the wait for a VM to boot are taken out, and everything else plays as it happened.

## What it shows

1. **The hosts.** The panel talks to each host's API over mutual TLS and sees all three.
2. **An image from a public link.** The host downloads Debian 13's cloud image, converts it from qcow2 and writes it into NBS, the cluster's block storage. There it is frozen read-only, and VMs are made from its clones.
3. **A network across the hosts.** vnet gives VMs on different hosts one subnet, a gateway and a way out to the internet. The state of the cluster's networks is kept on every host and agreed through Raft.
4. **Two VMs on two hosts.** Each disk is a clone of the image. Both VMs are on the new network, behind a cluster firewall that lets in only ping from that network. cloud-init gives the image's user the password typed into the form.
5. **Storage that is green.** Every chunk of every volume is kept on three hosts, and NBS's metadata is agreed through Raft, so the storage keeps working when a host goes down.
6. **The consoles and a ping.** The VMs' serial consoles stream into the panel. We log in on one VM and ping the other. The packets cross an encrypted tunnel between the hosts, ChaCha20-Poly1305 running on CRYPTOGAMS' assembly.

## Read more

- [NBS: replicated block storage](/blog/nbs-block-storage) covers how volumes, replicas and the metadata service work.
- [vnet: VM networks across hosts](/blog/vnet-networks-across-hosts) covers the cluster's networks, the firewalls and the tunnel.
- [The hypervisor](/hypervisor) covers what runs the VMs.

## The narration

nos is a small operating system for servers. Its kernel is a small amount of C++ that you can read and review, and everything else, the device drivers, the hypervisor, the filesystems, networking and storage, is written in safe Rust. Here is a cluster of three nos hosts at work.

This is the management panel. The cluster has three hosts, and the panel sees each of them. First, an image for virtual machines, made from a public link: the Debian 13 cloud image. The host has downloaded it and written it into NBS, the cluster's block storage. The image is ready.

Next, a virtual network that machines on different hosts share: a subnet, a gateway and a way out to the internet. The networks' state is kept on every host and agreed through Raft, so the network survives a host failure.

The first machine runs on this host: made from the image, on the new network, with a password to log in and a firewall that lets in only ping. It is made. We start it right from the list. The second machine goes on another host of the cluster, with the same image, network and firewall. Both machines show up in the panel, already running.

Storage is green. Every chunk lives on three hosts, and the metadata is agreed through Raft, so losing a host does not hurt the storage.

The machine's console: Debian has booted. Now the first machine's console. We log in as debian. And we ping the second machine on the other host. The packets cross an encrypted tunnel between the hosts.

nos is open source. The code is on GitHub, and there is more on the website.
