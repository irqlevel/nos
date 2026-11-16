/* The kernel's SIMD sections (kernel/simd.h): the vector registers lent to
   code that runs vector instructions -- the crypto crate's CRYPTOGAMS
   assembly -- with preemption off and every register zeroed at the end. */
unsafe extern "C" {
    /// What a section may use on every CPU: hal/simd.h's bits, 0 for
    /// nothing.
    pub safe fn kernel_simd_features() -> u32;
    /// A section opened on this CPU: 1, the vector registers the caller's
    /// until `kernel_simd_end`; 0, nothing changed, where none can be had
    /// -- one open on this CPU already, or no vector unit. From any
    /// context.
    pub safe fn kernel_simd_begin() -> i32;
    /// The section `kernel_simd_begin` opened on this CPU, closed.
    pub fn kernel_simd_end();
}

/* Whose x87, SSE and AVX registers a CPU holds -- hv's guests', by the
   number hvarch gives each (fp.rs) -- kept with the sections, which mark a
   CPU's nobody's. */
unsafe extern "C" {
    /// Whether CPU `cpu` holds guest `id`'s registers as its last exit
    /// there left them; a cpu out of range holds nobody's.
    pub safe fn kernel_simd_guest_is(cpu: u32, id: u64) -> i32;
    /// Guest `id`'s registers have just been put into CPU `cpu`, by its
    /// entry there.
    pub fn kernel_simd_guest_loaded(cpu: u32, id: u64);
    /// CPU `cpu`'s registers are nobody's.
    pub safe fn kernel_simd_guest_lost(cpu: u32);
}
