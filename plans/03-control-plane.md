# Stage 3 — Control plane: HTTP API, host-side virtio, guest networking

**Goal:** Manage VMs remotely over an HTTP API — create, destroy, list, and
attach to a guest console — and give guests real block and network devices via
host-side virtio. This is the step that makes `nos` a *cloud node*, not just a
hypervisor.

**Depends on:** Stage 2 (a bootable guest).

**Demo:**
```
curl -X POST http://nos-host/vms \
  -d '{"kernel":"bzImage","initrd":"rootfs.cpio","mem":128,"cmdline":"console=ttyS0"}'
curl http://nos-host/vms/0/console      # stream the guest's serial console
```

---

## The HTTP API is the cheap part

The networking substrate already exists: TCP stack, HTTP *client*, and — as a
precedent for "manage from outside" — the UDP remote shell. An HTTP *server* on
that base is quick to write.

Endpoints (v1):
- `POST /vms` — create from `{kernel, initrd, mem, vcpus, cmdline}`; returns id.
- `GET /vms`, `GET /vms/{id}` — list / inspect.
- `DELETE /vms/{id}` — destroy.
- `GET /vms/{id}/console` — stream serial console (chunked / long-poll).
- `POST /system/update` — used in Stage 4 (live host update).

Image storage is nearly free: `bzImage`/initramfs/rootfs can be uploaded through
the same API and kept in **nanofs/ext2**, which already exist.

## The two genuinely new pieces of work

### 3.1 Host-side virtio (the main effort)
Today `nos`'s virtio drivers are **guest-side** (`nos` as a KVM guest). For
`nos`'s *own* guests, the roles invert: `nos` must **emulate** a virtio device
and service the virtqueues from the device side.

- Start with **virtio-mmio**, which is simpler for a guest than virtio-pci and
  needs no PCI config-space emulation.
- Implement the split-virtqueue device side: read the guest's available ring,
  process descriptors, write the used ring, inject the guest interrupt.
- **virtio-net** first (needed for guest connectivity), then **virtio-blk**.
- Reuse existing knowledge: the team already understands virtio from the guest
  side; here it is the mirror image. All descriptor/ring access goes through the
  `GuestMemory` volatile accessors from Stage 1 — never raw references.

### 3.2 Guest networking
- A simple L2 bridge between guests' virtio-net and the physical NIC, **or** NAT
  using the existing IP stack.
- Give each guest a MAC; optionally run the existing DHCP logic host-side to hand
  out guest addresses, or bridge to the upstream DHCP.

## Backing storage for guest disks

virtio-blk needs a backend. Options, simplest first:
- A file on nanofs/ext2 exposed as a virtual disk.
- A raw partition (the block layer + MBR partition support already exist).

## Live-update discipline continues here

Every device model added in this stage (virtio-net, virtio-blk, the bridge)
must keep its state as **serializable POD** and must be **drainable** (finish or
cancel in-flight virtqueue work on request). Stage 4 depends on being able to
quiesce and snapshot every emulated device. Designing it in now is free;
retrofitting it is not.

## What v1 deliberately omits

Live migration between hosts, resource isolation/quotas (cgroups-equivalent),
IOMMU/device passthrough, multi-tenant hardening. These are what separate this
from Firecracker as a *product*; they are out of scope for the engineering
milestone.

## Effort

**Several weeks of hobby time**, dominated by host-side virtio. The HTTP API and
storage are comparatively quick because the substrate exists.

## Exit criteria

- `curl` creates a Linux VM and streams its console.
- Guest gets a working virtio-net link and can reach the network.
- Guest gets a virtio-blk disk backed by host storage.
- VM lifecycle (create/list/destroy) is clean, and every device model is
  POD-serializable and drainable (Stage 4 readiness).
