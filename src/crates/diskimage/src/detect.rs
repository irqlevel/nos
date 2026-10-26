//! What the first bytes of a file say it is: a qcow2 image, an image of
//! another hypervisor's, a compressed stream, an ISO, a disk that is raw
//! already -- so that a file this does not convert is refused by what it
//! is, with what to do with it, and not as a damaged qcow2.

/// How much of a file's start `detect` looks at: past an ISO's first
/// volume descriptor. A shorter file is looked at whole.
pub const HEAD: usize = 0x8006;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    /// QEMU's qcow2, which [`crate::qcow2`] converts.
    Qcow2,
    /// QEMU's old qcow, version 1.
    Qcow,
    /// VMware's VMDK: a sparse extent, or the text that describes one.
    Vmdk,
    /// Hyper-V's VHDX, and Virtual PC's VHD (a dynamic one: a fixed one is
    /// a raw disk with a footer, and reads as one).
    Vhdx,
    Vhd,
    /// VirtualBox's VDI.
    Vdi,
    Gzip,
    Xz,
    Zstd,
    Bzip2,
    /// An ISO 9660 filesystem: a CD's image, which a guest reads as it is.
    Iso9660,
    /// A partition table at the start: a disk that is raw already.
    Raw,
    Unknown,
}

/// What `head`, a file's first bytes, say the file is.
pub fn detect(head: &[u8]) -> Format {
    let at = |offset: usize, magic: &[u8]| head.get(offset..offset + magic.len()) == Some(magic);
    if at(0, b"QFI\xfb") {
        return if at(4, &[0, 0, 0, 1]) { Format::Qcow } else { Format::Qcow2 };
    }
    if at(0, b"KDMV") || at(0, b"COWD") || at(0, b"# Disk DescriptorFile") {
        return Format::Vmdk;
    }
    if at(0, b"vhdxfile") {
        return Format::Vhdx;
    }
    if at(0, b"conectix") {
        return Format::Vhd;
    }
    /* The signature 0xBEDA107F, little-endian, after VDI's banner. */
    if at(0x40, &[0x7F, 0x10, 0xDA, 0xBE]) {
        return Format::Vdi;
    }
    if at(0, &[0x1F, 0x8B]) {
        return Format::Gzip;
    }
    if at(0, &[0xFD, b'7', b'z', b'X', b'Z', 0]) {
        return Format::Xz;
    }
    if at(0, &[0x28, 0xB5, 0x2F, 0xFD]) {
        return Format::Zstd;
    }
    if at(0, b"BZh") {
        return Format::Bzip2;
    }
    /* Before the partition table: a hybrid ISO has one too. */
    if at(0x8001, b"CD001") {
        return Format::Iso9660;
    }
    if at(512, b"EFI PART") || at(510, &[0x55, 0xAA]) {
        return Format::Raw;
    }
    Format::Unknown
}

impl Format {
    /// What it is, in a few words.
    pub fn name(self) -> &'static str {
        match self {
            Format::Qcow2 => "a qcow2 image",
            Format::Qcow => "a qcow image, QEMU's old version 1",
            Format::Vmdk => "a VMDK image, VMware's",
            Format::Vhdx => "a VHDX image, Hyper-V's",
            Format::Vhd => "a VHD image, Virtual PC's",
            Format::Vdi => "a VDI image, VirtualBox's",
            Format::Gzip => "a gzip stream",
            Format::Xz => "an xz stream",
            Format::Zstd => "a zstd stream",
            Format::Bzip2 => "a bzip2 stream",
            Format::Iso9660 => "an ISO 9660 image",
            Format::Raw => "a raw disk, with a partition table",
            Format::Unknown => "nothing this knows",
        }
    }
}
