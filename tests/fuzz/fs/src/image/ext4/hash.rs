//! The hash an htree directory places its names by -- half-MD4, the
//! default, over unsigned characters, from the superblock's seed -- as
//! fs/ext4/hash.c computes it: the judge's own, for its mkfs to build
//! indexes e2fsck holds to their ranges.

/// The default seed, where the superblock's is zeros: MD4's own start.
const MD4_INIT: [u32; 4] = [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476];

/// The marker of the end of the hash space, which no name may hash to.
const EOF_32BIT: u32 = 0x7fff_ffff;

fn f(x: u32, y: u32, z: u32) -> u32 {
    z ^ (x & (y ^ z))
}

fn g(x: u32, y: u32, z: u32) -> u32 {
    (x & y).wrapping_add((x ^ y) & z)
}

fn h(x: u32, y: u32, z: u32) -> u32 {
    x ^ y ^ z
}

const K2: u32 = 0o13240474631;
const K3: u32 = 0o15666365641;

fn half_md4(buf: &mut [u32; 4], inp: &[u32; 8]) {
    let (mut a, mut b, mut c, mut d) = (buf[0], buf[1], buf[2], buf[3]);
    macro_rules! round {
        ($f:ident, $a:ident, $b:ident, $c:ident, $d:ident, $x:expr, $s:expr) => {
            $a = $a.wrapping_add($f($b, $c, $d)).wrapping_add($x).rotate_left($s);
        };
    }
    round!(f, a, b, c, d, inp[0], 3);
    round!(f, d, a, b, c, inp[1], 7);
    round!(f, c, d, a, b, inp[2], 11);
    round!(f, b, c, d, a, inp[3], 19);
    round!(f, a, b, c, d, inp[4], 3);
    round!(f, d, a, b, c, inp[5], 7);
    round!(f, c, d, a, b, inp[6], 11);
    round!(f, b, c, d, a, inp[7], 19);

    round!(g, a, b, c, d, inp[1].wrapping_add(K2), 3);
    round!(g, d, a, b, c, inp[3].wrapping_add(K2), 5);
    round!(g, c, d, a, b, inp[5].wrapping_add(K2), 9);
    round!(g, b, c, d, a, inp[7].wrapping_add(K2), 13);
    round!(g, a, b, c, d, inp[0].wrapping_add(K2), 3);
    round!(g, d, a, b, c, inp[2].wrapping_add(K2), 5);
    round!(g, c, d, a, b, inp[4].wrapping_add(K2), 9);
    round!(g, b, c, d, a, inp[6].wrapping_add(K2), 13);

    round!(h, a, b, c, d, inp[3].wrapping_add(K3), 3);
    round!(h, d, a, b, c, inp[7].wrapping_add(K3), 9);
    round!(h, c, d, a, b, inp[2].wrapping_add(K3), 11);
    round!(h, b, c, d, a, inp[6].wrapping_add(K3), 15);
    round!(h, a, b, c, d, inp[1].wrapping_add(K3), 3);
    round!(h, d, a, b, c, inp[5].wrapping_add(K3), 9);
    round!(h, c, d, a, b, inp[0].wrapping_add(K3), 11);
    round!(h, b, c, d, a, inp[4].wrapping_add(K3), 15);

    buf[0] = buf[0].wrapping_add(a);
    buf[1] = buf[1].wrapping_add(b);
    buf[2] = buf[2].wrapping_add(c);
    buf[3] = buf[3].wrapping_add(d);
}

/// `msg` as the words a round takes (`str2hashbuf_unsigned`): bytes
/// shifted in four to a word, the rest padded by its length.
fn words(msg: &[u8], out: &mut [u32; 8]) {
    let len = msg.len() as u32;
    let mut pad = len | (len << 8);
    pad |= pad << 16;
    let mut val = pad;
    let take = msg.len().min(32);
    let mut k = 0;
    for (i, &c) in msg[..take].iter().enumerate() {
        val = u32::from(c).wrapping_add(val << 8);
        if i % 4 == 3 {
            out[k] = val;
            k += 1;
            val = pad;
        }
    }
    if k < 8 {
        out[k] = val;
        k += 1;
    }
    while k < 8 {
        out[k] = pad;
        k += 1;
    }
}

/// Name `name`'s hash in a directory of a filesystem whose seed is `seed`:
/// even, as an index entry's low bit says whether a run of one hash goes on
/// into the next block.
pub fn name_hash(name: &[u8], seed: &[u32; 4]) -> u32 {
    let mut buf = if seed.iter().all(|&w| w == 0) { MD4_INIT } else { *seed };
    let mut inp = [0u32; 8];
    let mut p = name;
    loop {
        words(p, &mut inp);
        half_md4(&mut buf, &inp);
        if p.len() <= 32 {
            break;
        }
        p = &p[32..];
    }
    let mut hash = buf[1] & !1;
    if hash == EOF_32BIT << 1 {
        hash = (EOF_32BIT - 1) << 1;
    }
    hash
}
