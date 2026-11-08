# memsites: who holds Rust's heap

`memsites` answers "what is all this memory, and which line of code
allocated it" for the Rust half of a running kernel -- every `Box`, `Vec`,
`String`, `BTreeMap` and `Arc` the image and its modules hold -- down to
the file and the line, from the shell of the machine, the
[UDP shell](udp-shell.md) included.

```
> memsites on                  # from now on, every Rust allocation gets its site
> memsites                     # the 20 sites holding the most, and what holds the rest
> memsites 50                  # the top 50
> memsites tags                # the heap's pool blocks by owner, C++'s and Rust's
> memsites dump                # every site, for scripts/memsites.py
> memsites off
```

or from the first allocation of the boot: `memsites=on`
([kernel parameters](kernel-parameters.md)). `memusage` says how much
Rust's heap holds; `memsites` says who.

## What it says

```
memsites: on; 1027 stacks of 3640, 0 allocations given none, 0 big blocks noted (0 not)
     taken      asked  blocks paths  where (and the two frames above it)
     24.2K      12.0K      12     1  <fs::ext4::cache::Cache>::insert_clean+0x114
                                     <- fs::ext4::source::read_block+0x160
                                     <- <fs::ext4::Ext4>::load_dir+0x464
     16.1K       8192       8     7  <fs::ext4::txn::Txn>::read+0x120
                                     <- <fs::ext4::txn::Txn>::modify+0xCC
                                     <- <fs::ext4::txn::Txn>::put_inode+0xF4
memsites: the sites hold 45.6K in 27 blocks; the rest of Rust's heap, about 16.2M in 4299 blocks, has none: allocated while off, or no room for its stack
```

A line is a **site**: the first frame above the allocation that is the
code's own -- past Rust's allocator shims (`__rustc::__rust_alloc`), the
kernel's `GlobalAlloc`, and `alloc`'s and `core`'s code: a `Vec`'s growth,
`to_vec`, `format!`, `to_string` for a type of the code's. Its two callers
follow. *Taken* is what the blocks take of RAM once the heap rounded them
up (`memusage`'s sense: a pool's block and its header, or a run of a power
of two of pages), *asked* what the code asked for -- the line above holds
twelve 1 KiB blocks in 2 KiB each, which is the pools' rounding of 1024
bytes and the allocator's 8-byte header. *Paths* is how many distinct
stacks above the site were merged into its line. The last line is what the
sites do not cover: what was allocated before `memsites on`, or what the
table of stacks had no room for (*given none*, the header's count).

`memsites tags` reads the pools by the tag every block carries, the C++
code's owners (`'Cmd '`, `'Cpu '` ...) beside Rust's -- `rust, by site`,
and `rust` for those with none: Rust's blocks allocated while off, and the
C++ objects made for Rust's use under the same tag (its mutexes, spin locks
and wait groups, `kernel/rust_ffi.cpp`). A block of half a page and more
has no tag and is not in it; Rust's are in `memsites` by their site.

With `memsites=on` the first report of a boot already says something: on
arm64 under QEMU, the network's frame pool (`net::frame::FramePool::setup`)
holds 16 MiB for the 8.2 MiB it asked for -- 4096 frames of 2104 bytes,
each past the largest pool block (2040 bytes and the header) and so a page
of its own.

## To the file and the line

The kernel names frames by its own symbol table, which has functions and
no lines. `memsites dump` writes every site with its frames as addresses,
and `scripts/memsites.py` names them over the kernel's ELF -- whose Rust is
built with its line tables (`CARGO_PROFILE_RELEASE_DEBUG=line-tables-only`;
the image itself loads none of it):

```
$ ssh nos memsites dump | scripts/memsites.py -
$ scripts/memsites.py --udp 127.0.0.1:9000 --top 30
memsites: 8 sites hold 42.8K in 24 blocks; the rest of Rust's heap, about 16.2M in 4296 blocks, has none
     taken      asked  blocks paths  where
     24.2K      12.0K      12     1  <fs::ext4::cache::Cache>::insert_clean+0x114  src/crates/layers/fs/src/ext4/cache.rs:43
                                     <- fs::ext4::source::read_block+0x160  src/crates/layers/fs/src/ext4/source.rs:58
                                     <- <fs::ext4::Ext4>::load_dir+0x464  src/crates/layers/fs/src/ext4/source.rs:73 (inlined at mod.rs:657)
```

The line of a site is the line of the code that asked -- `cache.rs:43` is
`self.clean.push_back((b, data.to_vec()))` -- not the line inside `alloc`
that was inlined into it: of a frame's chain of inlined calls the script
names the innermost that is not Rust's library, and, where that is a helper
inlined into the frame's function, where the function has it. The ELF is
`bin/kernel64.elf` or `kernel-arm64.elf`, told apart by the addresses
(`--elf` to name one): the one the kernel was built as. A module's frames
are named by the module and not to a line. `llvm-symbolizer` is the host's,
or the nos-builder image's.

## How it works

The code is `mm/alloc_sites.{h,cpp}` (the tables, under the allocator) and
`kernel/memsites.{h,cpp}` (the reading, the command, the parameter).

Every block Rust's allocator takes goes through one function,
`kernel_alloc`. With the tracker on, it walks the frame pointers above
itself -- Rust is built with `-Cforce-frame-pointers=yes` on both
architectures, as the C++ is with `-fno-omit-frame-pointer` -- eight return
addresses, the **stack**, and looks it up in a table of the stacks seen so
far. A stack not there is added: no lock, a slot claimed by a
compare-and-swap and its frames published by a release store of their
hash. The table only grows -- 3640 stacks in 256 KiB, made the first time
the tracker is turned on and kept for good, since a block names a stack in
it for as long as it lives -- and stops taking new stacks at seven-eighths
full, so that a look always ends at an empty slot soon; an allocation it
has no room for is counted and carries no site.

The site's number then goes where the heap keeps a block's owner already:
the tag of a pool's block (`Pool::Block`), compared and swapped on every
claim and free to catch a block handed out twice -- which every Rust block
carried as one `'rust'` before. A block of half a page and more is the
page allocator's and has no tag; its site goes into a second table, the
big blocks', from `kernel_alloc` to `kernel_free`, which takes it out
before the block can be anybody else's. That table is in 32 shards by
address, each under a spin lock of its own and on a cache line of its own,
and a free looks only while its shard has a line in it; a line is removed
by shifting the ones after it back, so that no tombstone is left to slow a
look down.

Nothing is counted on the way. What a site holds is read off the heap when
asked: every pool's pages walked under the pool's lock
(`AllocatorImpl::walk`), each block's tag and the size its header says it
was asked for, and the big blocks' table read under its lock. It is what is
live at that moment, whatever came and went before, and a free costs a
pool's block nothing more. A block that a CPU's cache hands out or takes
back while its pool is walked -- the caches take no pool lock -- may be
counted under its old owner or its new one: a report is a picture of a
heap that goes on moving.

Turned off, the blocks given a site keep it until they are freed, and the
report goes on reading them; turned on again, nothing is lost.

## What it costs

With the tracker off, an allocation pays a load and a branch, and the free
of a big block another load or two. On, an allocation pays the walk of its
frames and a look in the table of stacks, and a big block a line in its
shard of the big blocks' table and its free that line taken out, each under
the shard's lock. `heapbench`, arm64 under HVF, four CPUs; nanoseconds for
an allocation and its free, the median of four boots of each, interleaved:

| workload | master | off | on | master, 4 CPUs | off, 4 CPUs | on, 4 CPUs |
|---|---|---|---|---|---|---|
| pair 64 | 119.6 | 120.3 | 145.9 | 122.9 | 123.8 | 146.6 |
| pair 2032 | 120.8 | 123.9 | 146.3 | 125.1 | 126.8 | 148.8 |
| pair 4096 | 93.2 | 93.5 | 189.9 | 94.3 | 95.0 | 194.7 |
| pair 65536 | 113.2 | 114.3 | 212.8 | 963.4 | 991.8 | 1603.5 |
| pair 262144 | 114.3 | 116.1 | 217.3 | 915.4 | 887.2 | 818.4 |
| grow 64k | 5376.6 | 5381.0 | 6005.5 | 6126.6 | 6157.6 | 7053.7 |

Off is within a few nanoseconds of the tree without it, inside the runs'
own spread. On, a pool's block costs some 25 ns more, a page run some 100.
The big blocks' table was one lock at first, and four CPUs taking pages at
once queued on it: a pair of a page cost 1.4 us on them where it cost
0.1 us; in 32 shards by address it costs 0.19. Where the page allocator's
own contention sets the pace -- 64 KiB runs on four CPUs at once, ten times
a page's cost on master -- on adds 0.6 us to the pair, steadily over the
boots; to 256 KiB runs, nothing.

The tables take 516 KiB once turned on -- the stacks' 256, the big blocks'
256 and a page of their shards -- and a report its own few hundred KiB
while it runs.

## What it does not do

- The C++ code's allocations are not given sites: they carry the tags their
  callers pass (`memsites tags` reads those), which name an owner, not a
  line.
- A block allocated before `memsites on` has no site. For the whole picture,
  `memsites=on` at boot.
- The stacks are eight frames deep. A site reached by many paths is one line
  with its paths counted; the callers printed are those of the path holding
  the most.
- A frame that is a tail call leaves no return address behind: the site is
  then the frame above it, as the stack has it.
