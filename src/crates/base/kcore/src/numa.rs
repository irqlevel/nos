//! NUMA: which CPUs and which memory are near each other, as the firmware
//! said it (`kernel/numa.h`) -- one node on a machine it says nothing of.
//! What a guest's memory is placed by: on the node its CPUs are on
//! ([`crate::frame::Run::new_on`]).

use ffi::numa;

/// How many nodes there are: 1 on a machine of one.
#[inline]
pub fn node_count() -> usize {
    numa::kernel_numa_node_count()
}

/// The node CPU `cpu` is on: 0 for one past the machine's, or the firmware
/// placed nowhere.
#[inline]
pub fn cpu_node(cpu: u32) -> usize {
    numa::kernel_numa_cpu_node(cpu as usize)
}

/// The CPUs of node `node`, a bit each.
#[inline]
pub fn node_cpus(node: usize) -> u64 {
    numa::kernel_numa_node_cpus(node)
}

/// The distance from node `from` to node `to`: 10 a node's to itself, more
/// the further off.
#[inline]
pub fn distance(from: usize, to: usize) -> u32 {
    numa::kernel_numa_distance(from, to)
}

/// The node physical address `phys` is on, as the firmware placed its
/// memory: what a page handed out for a node is checked by.
#[inline]
pub fn phys_node(phys: u64) -> usize {
    numa::kernel_numa_phys_node(phys)
}

/// The free pages of node `node`.
#[inline]
pub fn node_free_pages(node: usize) -> u64 {
    numa::kernel_numa_node_free(node)
}
