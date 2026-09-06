# Run

How to boot what [Build](build.md) produced: in QEMU on x86-64 and arm64, and
on Google Cloud. Boot-time options are listed in
[Kernel parameters](kernel-parameters.md); for the two bare-metal machines
see [Real hardware](real-hardware.md).

## QEMU, x86-64

With KVM (Linux):

```sh
qemu-system-x86_64 -enable-kvm -smp 8 -cdrom nos.iso -serial file:nos.log \
    -device isa-debug-exit,iobase=0xf4,iosize=0x04
```

Without KVM (macOS with TCG):

```sh
qemu-system-x86_64 -smp 2 -cdrom nos.iso -serial file:nos.log -s -vga std
```

The repository scripts wrap the common cases (serial console goes to `nos.log`):

```sh
./scripts/qemu.sh        # ISO boot, 2 CPUs, virtio-net, GDB stub on :1234
./scripts/qemu-disk.sh   # disk boot with virtio-blk/scsi/nvme/net/rng attached
./scripts/run.sh         # Linux/KVM, all host CPUs, isa-debug-exit device
./scripts/smoke-test.sh  # headless boot smoke test, gate on its exit code
```

### UEFI boot (OVMF)

OVMF instead of the legacy BIOS — see
[Firmware: BIOS and UEFI](build.md#firmware-bios-and-uefi). There is no EGA text mode
under UEFI, so GRUB hands the kernel a pixel framebuffer and the screen is
drawn with the 8x16 font console; the PS/2 keyboard is emulated by QEMU, so
the shell is usable on screen as well as on serial (on a real UEFI laptop it
would come from the xHCI USB keyboard instead):

```sh
cp /usr/share/OVMF/OVMF_VARS_4M.fd /tmp/ovmf_vars.fd
qemu-system-x86_64 -smp 2 -m 1G -cdrom nos.iso -serial file:nos.log \
    -drive if=pflash,format=raw,unit=0,readonly=on,file=/usr/share/OVMF/OVMF_CODE_4M.fd \
    -drive if=pflash,format=raw,unit=1,file=/tmp/ovmf_vars.fd
```

### Disk image

Boot from the disk image (with virtio-blk):

```sh
./scripts/qemu-disk.sh
```

## QEMU, arm64

QEMU `virt` board, HVF-accelerated on Apple Silicon (`NOS_TCG=1` forces TCG;
HVF is about 4x faster). virtio-mmio blk/net/rng are attached, serial goes to
`nos-arm64.log`, the UDP shell is forwarded on `:9000`:

```sh
./scripts/qemu-arm64.sh
./scripts/smoke-arm64.sh              # boot smoke test (SMOKE_HVF=1 for HVF)
python3 scripts/udpsh.py 127.0.0.1    # remote shell over UDP
```

## Google Cloud

Deploy `nos.qcow2` (from `./scripts/build-disk.sh`) to Google Cloud Compute
Engine. Select **Skip OS adaptation** when importing the image — the kernel
already has the necessary drivers and no guest agent:

```sh
# Upload disk image to a GCS bucket
gcloud storage cp nos.qcow2 gs://YOUR_BUCKET/nos.qcow2

# Create a Compute Engine image from the disk (skip OS adaptation)
gcloud compute images create nos-image \
    --source-uri=gs://YOUR_BUCKET/nos.qcow2

# Launch a VM (serial console recommended)
gcloud compute instances create nos-vm \
    --image=nos-image \
    --machine-type=e2-small \
    --metadata=serial-port-enable=true

# Connect via serial console
gcloud compute connect-to-serial-port nos-vm
```
