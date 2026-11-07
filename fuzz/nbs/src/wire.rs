//! The `wire` target: NBS's messages (src/crates/common/storage/nbswire)
//! held to two things.
//!
//! - **Every message read back as written**: random requests, answers,
//!   questions and their answers, of every kind and at their limits, encoded
//!   and decoded the same.
//! - **Bytes read as they are, or as none**: random bytes, and messages
//!   spoiled -- a byte changed, cut short, grown -- read without a panic; any
//!   that read as a message encode back to exactly those bytes. One encoding
//!   a message, so that two hosts never read one message two ways.

use nbscs::{ChunkRef, Epoch};
use nbswire::{Answer, Answered, Device, Identity, Op, Query, Question, Refusal, Reply, Request, Told, DATA_MAX,
              LIST_MAX};

use crate::rng::{mix, Rng};

fn chunk(r: &mut Rng) -> ChunkRef {
    ChunkRef { chunk: r.below(1 << 32) as u32, incarnation: r.below(1 << 32) as u32,
               epoch: Epoch { set: r.below(1 << 32) as u32, version: r.below(1 << 32) as u32 } }
}

fn len(r: &mut Rng, max: u64) -> usize {
    match r.below(10) {
        0 => 0,
        1 => max as usize,
        _ => r.below(max.min(600) + 1) as usize,
    }
}

fn refusal(r: &mut Rng) -> Refusal {
    match r.below(16) {
        0 => Refusal::Fenced { generation: r.below(u64::MAX) },
        1 => Refusal::NotFenced { generation: r.below(u64::MAX) },
        2 => Refusal::StaleChunk { incarnation: r.below(1 << 32) as u32 },
        3 => Refusal::Epoch { known: Epoch { set: r.below(1 << 32) as u32, version: r.below(1 << 32) as u32 } },
        4 => Refusal::NotTheVolumes,
        5 => Refusal::NoRoom,
        6 => Refusal::Bad,
        7 => Refusal::Failed,
        8 => Refusal::Io,
        9 => Refusal::TooManyFences,
        10 => Refusal::NotTheMap,
        11 => Refusal::NotHere,
        12 => Refusal::NotAttached,
        13 => Refusal::Behind { applied: r.below(u64::MAX) },
        14 => Refusal::NoMds,
        _ => Refusal::NotReady,
    }
}

/// Every kind of message, at random: its bytes.
fn message(r: &mut Rng, data: &[u8]) -> (u8, Vec<u8>) {
    let id = r.below(u64::MAX);
    match r.below(4) {
        0 => {
            let n = len(r, u64::from(DATA_MAX));
            let op = match r.below(6) {
                0 => Op::Fence,
                1 => Op::Write { chunk: chunk(r), position: r.below(1 << 32) as u32, index: r.below(u64::MAX),
                                 offset: r.below(1 << 32) as u32, data: &data[..n] },
                2 => Op::Read { chunk: chunk(r), position: r.below(1 << 32) as u32, index: r.below(u64::MAX),
                                offset: r.below(1 << 32) as u32, len: r.below(u64::from(DATA_MAX) + 1) as u32 },
                3 => Op::Flush,
                4 => Op::Close { chunk: chunk(r), position: r.below(1 << 32) as u32, index: r.below(u64::MAX) },
                _ => Op::Held,
            };
            let m = Request { id, volume: r.below(1 << 32) as u32, generation: r.below(u64::MAX), op };
            (0, m.encode())
        }
        1 => {
            let n = len(r, u64::from(DATA_MAX));
            let reply = match r.below(6) {
                0 => Reply::Fenced { generation: r.below(u64::MAX) },
                1 => Reply::Done,
                2 => Reply::Data(&data[..n]),
                3 => Reply::Closed(r.chance(50)),
                4 => {
                    let k = len(r, LIST_MAX as u64);
                    Reply::Held((0..k).map(|i| (i as u32, r.below(1 << 32) as u32, r.below(u64::MAX))).collect())
                }
                _ => Reply::Refused(refusal(r)),
            };
            (1, Answer { id, reply }.encode())
        }
        2 => {
            let question = match r.below(4) {
                0 => Question::Identity,
                1 => Question::Verify { volume: r.below(1 << 32) as u32, generation: r.below(u64::MAX), chunk: chunk(r),
                                        position: r.below(1 << 32) as u32, index: r.below(u64::MAX),
                                        writing: r.chance(50) },
                2 => {
                    let k = len(r, LIST_MAX as u64);
                    Question::Live { chunks: (0..k).map(|i| (i as u32, r.below(1 << 32) as u32)).collect() }
                }
                _ => {
                    let k = len(r, LIST_MAX as u64);
                    Question::Volumes { volumes: (0..k).map(|i| i as u32).collect() }
                }
            };
            (2, Query { id, question }.encode())
        }
        _ => {
            let told = match r.below(5) {
                0 => Told::Identity(None),
                1 => {
                    let n = r.below(nbsmeta::DEVICES_MAX as u64 + 1) as usize;
                    let devices = (0..n).map(|k| Device {
                        ident: format!("part:{:016x}-{}", r.below(u64::MAX), k),
                        device: r.below(1 << 32) as u32,
                        slots: r.below(1 << 32) as u32,
                        draining: r.chance(30),
                    }).collect();
                    let mut cluster = [0u8; 16];
                    for b in cluster.iter_mut() {
                        *b = r.below(256) as u8;
                    }
                    Told::Identity(Some(Identity { cluster, cs: r.below(1 << 32) as u32, devices }))
                }
                2 => Told::Verified,
                3 => Told::Refused(refusal(r)),
                _ => {
                    let k = len(r, LIST_MAX as u64);
                    Told::Gone((0..k).map(|i| i as u32).collect())
                }
            };
            (3, Answered { id, told }.encode())
        }
    }
}

/// Bytes read as message kind `kind`: whether they were one, and if so that
/// they encode back to themselves.
fn read_back(kind: u8, b: &[u8]) -> Result<bool, String> {
    let again = match kind {
        0 => Request::decode(b).map(|m| m.encode()),
        1 => Answer::decode(b).map(|m| m.encode()),
        2 => Query::decode(b).map(|m| m.encode()),
        _ => Answered::decode(b).map(|m| m.encode()),
    };
    match again {
        Some(e) if e != b => Err(format!("kind {}: {} bytes read as a message that encodes as {} other bytes", kind,
                                         b.len(), e.len())),
        Some(_) => Ok(true),
        None => Ok(false),
    }
}

pub fn run(seed: u64, steps: u64, _trace: bool) -> Result<(), String> {
    let mut r = Rng::new(mix(seed, 0x77697265));
    let data: Vec<u8> = (0..DATA_MAX as usize).map(|i| mix(seed, i as u64) as u8).collect();
    for step in 0..steps {
        let (kind, bytes) = message(&mut r, &data);
        if !read_back(kind, &bytes).map_err(|e| format!("step {}: {}", step, e))? {
            return Err(format!("step {}: a message of kind {}, {} bytes, not read back", step, kind, bytes.len()));
        }
        /* Spoiled: a byte changed, cut short, grown -- and as another kind */
        let mut spoiled = bytes.clone();
        match r.below(4) {
            0 if !spoiled.is_empty() => {
                let at = r.below(spoiled.len() as u64) as usize;
                spoiled[at] = r.below(256) as u8;
            }
            1 => spoiled.truncate(r.below(spoiled.len() as u64 + 1) as usize),
            2 => {
                /* Grown exactly: a doubled vector of a big message would be
                   the fuzzer's own block past the kernel heap's 512 KiB */
                let n = 1 + r.below(8) as usize;
                spoiled.reserve_exact(n);
                spoiled.extend((0..n).map(|_| r.below(256) as u8));
            }
            _ => {}
        }
        for k in 0..4 {
            read_back(k, &spoiled).map_err(|e| format!("step {}, spoiled: {}", step, e))?;
        }
        /* And noise */
        let n = r.below(64) as usize;
        let noise: Vec<u8> = (0..n).map(|_| r.below(256) as u8).collect();
        for k in 0..4 {
            read_back(k, &noise).map_err(|e| format!("step {}, noise: {}", step, e))?;
        }
    }
    Ok(())
}
