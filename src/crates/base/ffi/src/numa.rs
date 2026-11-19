/* NUMA: which CPUs and which memory are near each other (kernel/numa.h),
   as the firmware's tables said it -- one node on a machine they say nothing
   of. Values only, every one of them: a CPU or a node past the machine's is
   answered for, not trusted. */
unsafe extern "C" {
    /// How many nodes: 1 on a machine of one.
    pub safe fn kernel_numa_node_count() -> usize;
    /// The node of CPU `cpu` (its index; the APIC id on x86-64): 0 for a CPU
    /// past the machine's, or the firmware placed nowhere.
    pub safe fn kernel_numa_cpu_node(cpu: usize) -> usize;
    /// The CPUs of node `node`, a bit each: 0 for a node past the machine's.
    pub safe fn kernel_numa_node_cpus(node: usize) -> u64;
    /// The distance from node `from` to node `to`, 10 a node's to itself as
    /// ACPI has it: 255 for a node past the machine's.
    pub safe fn kernel_numa_distance(from: usize, to: usize) -> u32;
    /// The free pages of node `node`: 0 for a node past the machine's.
    pub safe fn kernel_numa_node_free(node: usize) -> u64;
    /// The node of physical address `phys`, as the firmware placed its
    /// memory: 0 for one no range of it covers.
    pub safe fn kernel_numa_phys_node(phys: u64) -> usize;
}
