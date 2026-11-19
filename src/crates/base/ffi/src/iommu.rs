/* The IOMMU -- VT-d on x86-64 (arch/x86_64/vtd.cpp), none on arm64 -- as a
 * domain a device is moved to: a second-level table of its own, by a handle
 * the kernel looks up (0 for none). `sid` is a requester id, bus << 8 |
 * device << 3 | function. Wrapped by kcore::iommu. */
unsafe extern "C" {
    /// Whether DMA is translated: iommu=on, and every unit came up.
    pub safe fn kernel_iommu_on() -> i32;
    /// Whether interrupts are remapped as well: every unit taken over has
    /// its interrupt table, compatibility-format interrupts blocked.
    pub safe fn kernel_iommu_remapping() -> i32;
    /// A domain for the unit that translates `sid`, as wide as that unit
    /// walks: 0 where DMA is not translated, the unit is one left be, or
    /// there is no room.
    pub safe fn kernel_iommu_domain_create(sid: u16) -> usize;
    /// `pages` pages from `iova` mapped to those from `phys`, readable and
    /// writable: 0 where one is mapped already, or the range is past the
    /// domain's width or not whole pages; the domain is then to be
    /// destroyed. Not 0 on success.
    ///
    /// # Safety
    /// The pages must stay allocated until the domain is destroyed: a
    /// device in it may reach them until then.
    pub fn kernel_iommu_domain_map(domain: usize, iova: u64, phys: u64, pages: usize) -> i32;
    /// `sid`'s DMA translated through the domain -- untranslated requests
    /// alone -- not 0 on success: 0 where `sid` is not translated by the
    /// domain's unit or is in a domain already.
    pub safe fn kernel_iommu_domain_attach(domain: usize, sid: u16) -> i32;
    /// `sid`'s DMA blocked: out of the domain, its context entry not present.
    pub safe fn kernel_iommu_domain_detach(domain: usize, sid: u16) -> i32;
    /// The domain gone: every device still in it blocked, its tables freed.
    /// After this nothing reaches what it mapped.
    pub safe fn kernel_iommu_domain_destroy(domain: usize);
    /// A device blocked, and in no domain, passed through again: not 0 on
    /// success.
    ///
    /// # Safety
    /// The device must hold no address it may no longer reach -- it was
    /// reset, or never told one: passed through, it reaches every byte.
    pub fn kernel_iommu_release(sid: u16) -> i32;
}
