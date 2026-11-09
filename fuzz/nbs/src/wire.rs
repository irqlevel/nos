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
use nbswire::{Answer, Answered, Ask, Asking, Device, Identity, JoinView, Op, Place, Query, Question, Refusal, Reply,
              ReplicaState, Request, Said, Saying, Told, DATA_MAX, LIST_MAX, MAP_MAX, REASON_MAX};

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
    match r.below(17) {
        0 => Refusal::Fenced { generation: r.below(u64::MAX) },
        1 => Refusal::NotFenced { generation: r.below(u64::MAX) },
        2 => Refusal::StaleChunk { incarnation: r.below(1 << 32) as u32 },
        3 => Refusal::Epoch { known: Epoch { set: r.below(1 << 32) as u32, version: r.below(1 << 32) as u32 } },
        4 => Refusal::NoRoom,
        5 => Refusal::Bad,
        6 => Refusal::Failed,
        7 => Refusal::Io,
        8 => Refusal::TooManyFences,
        9 => Refusal::NotTheMap,
        10 => Refusal::NotHere,
        11 => Refusal::NotAttached,
        12 => Refusal::Behind { applied: r.below(u64::MAX) },
        13 => Refusal::NoMds,
        14 => Refusal::NotHeld,
        15 => Refusal::NotTheJoiner,
        _ => Refusal::NotReady,
    }
}

/// A name, now and then at a limit's length, or past it -- which the writer
/// keeps and the reader refuses.
fn name(r: &mut Rng, max: usize) -> String {
    let n = match r.below(10) {
        0 => 0,
        1 => max,
        2 => max + 1,
        _ => r.below(20) as usize,
    };
    (0..n).map(|i| (b'a' + (mix(r.below(1 << 20), i as u64) % 26) as u8) as char).collect()
}

/// A reason, of any characters -- some wider than a byte -- and of any
/// length: what is told is cut at a character's edge.
fn reason(r: &mut Rng) -> String {
    let n = match r.below(6) {
        0 => REASON_MAX + r.below(40) as usize,
        _ => r.below(80) as usize,
    };
    (0..n).map(|_| match r.below(8) {
        0 => '\u{e9}',
        1 => '\u{2014}',
        2 => '\u{1f600}',
        _ => (b'a' + r.below(26) as u8) as char,
    }).collect()
}

fn state(r: &mut Rng) -> ReplicaState {
    match r.below(3) {
        0 | 1 => ReplicaState::Active,
        _ => ReplicaState::Down,
    }
}

/// A place: its chunk, three cs's and their states, and a joiner as the
/// reader takes one -- none, a cs no member is, or a member down back.
fn place(r: &mut Rng) -> Place {
    let cs = [r.below(1 << 32) as u32, r.below(1 << 32) as u32, r.below(1 << 32) as u32];
    let replicas = [state(r), state(r), state(r)];
    let joiner = match r.below(3) {
        0 => None,
        1 => (0..3).find(|&k| replicas[k] == ReplicaState::Down).map(|k| cs[k]),
        _ => Some(r.below(u64::from(u32::MAX)) as u32).filter(|j| !cs.contains(j)),
    };
    Place { chunk: chunk(r), cs, replicas, joiner, shared: r.chance(30) }
}

/// Every kind of message, at random: its kind, its bytes, and whether the
/// reader is to take it -- not one with a name past its limit, nor a map
/// asked for past MAP_MAX.
fn message(r: &mut Rng, data: &[u8]) -> (u8, Vec<u8>, bool) {
    let id = r.below(u64::MAX);
    match r.below(6) {
        0 => {
            let n = len(r, u64::from(DATA_MAX));
            let op = match r.below(12) {
                0 => Op::Fence,
                1 => Op::Write { chunk: chunk(r), position: r.below(1 << 32) as u32, index: r.below(u64::MAX),
                                 offset: r.below(1 << 32) as u32, data: &data[..n] },
                2 => Op::Read { chunk: chunk(r), position: r.below(1 << 32) as u32, index: r.below(u64::MAX),
                                offset: r.below(1 << 32) as u32, len: r.below(u64::from(DATA_MAX) + 1) as u32 },
                3 => Op::Flush,
                4 => Op::Close { chunk: chunk(r), position: r.below(1 << 32) as u32, index: r.below(u64::MAX) },
                5 if r.chance(50) => Op::Ping,
                5 => Op::Held,
                /* A joiner's asks of a source (N9) */
                6 => Op::Holdings { set: r.below(1 << 32) as u32, from: r.below(1 << 32) as u32,
                                    joiner: r.below(1 << 32) as u32, version: r.below(1 << 32) as u32,
                                    index: r.below(u64::MAX) },
                7 => Op::Pull { chunk: chunk(r), joiner: r.below(1 << 32) as u32, index: r.below(u64::MAX),
                                offset: r.below(1 << 32) as u32, len: r.below(u64::from(DATA_MAX) + 1) as u32 },
                /* The leader's scrub (N9e) */
                8 => Op::Scrub { chunk: chunk(r) },
                /* A copy-up's copy (N10) */
                9 => Op::CopyUp { from: chunk(r), to: chunk(r), position: r.below(1 << 32) as u32,
                                  index: r.below(u64::MAX) },
                _ => Op::Sums { chunk: chunk(r), joiner: r.below(1 << 32) as u32, index: r.below(u64::MAX),
                                offset: r.below(1 << 32) as u32, len: r.below(nbscs::SLOT + 1) as u32 },
            };
            let m = Request { id, volume: r.below(1 << 32) as u32, generation: r.below(u64::MAX), op };
            (0, m.encode(), true)
        }
        1 => {
            let n = len(r, u64::from(DATA_MAX));
            let reply = match r.below(10) {
                0 => Reply::Fenced { generation: r.below(u64::MAX) },
                1 => Reply::Done,
                2 => Reply::Data(&data[..n]),
                3 => Reply::Closed(r.chance(50)),
                4 => {
                    let k = len(r, LIST_MAX as u64);
                    Reply::Held((0..k).map(|i| (i as u32, r.below(1 << 32) as u32, r.below(u64::MAX))).collect())
                }
                5 => {
                    let k = len(r, LIST_MAX as u64);
                    Reply::Chunks((0..k).map(|i| (i as u32, r.below(1 << 32) as u32, r.below(1 << 32) as u32,
                                                  r.below(1 << 32) as u32)).collect())
                }
                6 => {
                    let k = len(r, nbswire::SUMS_MAX as u64);
                    Reply::Sums((0..k).map(|_| r.below(1 << 32) as u32).collect())
                }
                7 => {
                    let k = len(r, nbswire::SUMS_MAX as u64);
                    Reply::Scrub { open: r.chance(50), written: r.below(1 << 32) as u32,
                                   sums: (0..k).map(|_| r.below(1 << 32) as u32).collect() }
                }
                _ => Reply::Refused(refusal(r)),
            };
            (1, Answer { id, reply }.encode(), true)
        }
        2 => {
            let question = match r.below(9) {
                0 => Question::Identity,
                1 => Question::Verify { volume: r.below(1 << 32) as u32, generation: r.below(u64::MAX), chunk: chunk(r),
                                        position: r.below(1 << 32) as u32, index: r.below(u64::MAX),
                                        writing: r.chance(50) },
                2 => {
                    let k = len(r, LIST_MAX as u64);
                    Question::Live { chunks: (0..k).map(|i| (i as u32, r.below(1 << 32) as u32)).collect() }
                }
                3 => {
                    let k = len(r, LIST_MAX as u64);
                    Question::Volumes { volumes: (0..k).map(|i| i as u32).collect() }
                }
                /* A source's of a joiner, and a joiner's of its joins (N9) */
                4 => Question::VerifyJoin { set: r.below(1 << 32) as u32, joiner: r.below(1 << 32) as u32,
                                            version: r.below(1 << 32) as u32, index: r.below(u64::MAX) },
                5 => Question::Joins,
                6 => Question::JoinDone { set: r.below(1 << 32) as u32, since: r.below(1 << 32) as u32 },
                7 => Question::GiveUp { set: r.below(1 << 32) as u32, since: r.below(1 << 32) as u32 },
                /* A copy-up's copy (N10) */
                _ => Question::VerifyCopy { volume: r.below(1 << 32) as u32, generation: r.below(u64::MAX),
                                            position: r.below(1 << 32) as u32, from: chunk(r), to: chunk(r),
                                            index: r.below(u64::MAX) },
            };
            (2, Query { id, question }.encode(), true)
        }
        3 => {
            let told = match r.below(7) {
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
                4 => {
                    let k = len(r, LIST_MAX as u64);
                    Told::Gone((0..k).map(|i| i as u32).collect())
                }
                5 => Told::VerifiedJoining { since: r.below(1 << 32) as u32 },
                _ => {
                    /* A place past the three, and a name past its limit, now
                       and then: the reader's to refuse */
                    let k = len(r, 40);
                    Told::Joins((0..k).map(|_| {
                        let n = r.below(nbsmeta::REPLICAS as u64 + 1) as usize;
                        JoinView {
                            set: r.below(1 << 32) as u32,
                            place: if r.chance(3) { 3 + r.below(253) as u8 } else { r.below(3) as u8 },
                            since: r.below(1 << 32) as u32,
                            version: r.below(1 << 32) as u32,
                            own: r.chance(50),
                            until: r.below(1 << 32) as u32,
                            sources: (0..n).map(|i| (i as u32, name(r, nbsmeta::NAME_MAX))).collect(),
                            index: r.below(u64::MAX),
                        }
                    }).collect())
                }
            };
            let ok = match &told {
                Told::Joins(joins) => joins.iter().all(|j| usize::from(j.place) < nbsmeta::REPLICAS
                    && j.sources.iter().all(|(_, h)| h.len() <= nbsmeta::NAME_MAX)),
                _ => true,
            };
            (3, Answered { id, told }.encode(), ok)
        }
        4 => {
            let ask = match r.below(6) {
                0 => Ask::Attach { name: name(r, nbsmeta::VOLUME_NAME_MAX), writable: r.chance(50),
                                   force: r.chance(50), token: r.below(u64::MAX) },
                1 => Ask::Detach { name: name(r, nbsmeta::VOLUME_NAME_MAX), generation: r.below(u64::MAX) },
                2 => {
                    /* At the limit and past it, often: either side of it is the reader's to hold */
                    let count = match r.below(8) {
                        0 => MAP_MAX,
                        1 => MAP_MAX + 1 + r.below(1 << 20) as u32,
                        _ => r.below(u64::from(MAP_MAX)) as u32,
                    };
                    Ask::Map { name: name(r, nbsmeta::VOLUME_NAME_MAX), from: r.below(1 << 32) as u32, count,
                               index: r.below(u64::MAX) }
                }
                3 => Ask::Down { set: r.below(1 << 32) as u32, cs: r.below(1 << 32) as u32, refused: r.chance(50),
                                 name: name(r, nbsmeta::VOLUME_NAME_MAX), generation: r.below(u64::MAX) },
                /* A copy-up's asks (N10) */
                4 => Ask::CopyUp { name: name(r, nbsmeta::VOLUME_NAME_MAX), place: r.below(1 << 32) as u32,
                                   generation: r.below(u64::MAX) },
                _ => Ask::CopyDone { name: name(r, nbsmeta::VOLUME_NAME_MAX), place: r.below(1 << 32) as u32,
                                     generation: r.below(u64::MAX), chunk: r.below(1 << 32) as u32 },
            };
            let ok = match &ask {
                Ask::Attach { name, .. } | Ask::Detach { name, .. } => name.len() <= nbsmeta::VOLUME_NAME_MAX,
                Ask::Map { name, count, .. } => name.len() <= nbsmeta::VOLUME_NAME_MAX && *count <= MAP_MAX,
                Ask::Down { name, .. } | Ask::CopyUp { name, .. } | Ask::CopyDone { name, .. } => {
                    name.len() <= nbsmeta::VOLUME_NAME_MAX
                }
            };
            (4, Asking { id, ask }.encode(), ok)
        }
        _ => {
            let said = match r.below(10) {
                0 => Said::Attached { generation: r.below(u64::MAX), index: r.below(u64::MAX) },
                1 => Said::Detached { index: r.below(u64::MAX) },
                2 => {
                    let k = len(r, u64::from(MAP_MAX));
                    let map = (0..k).map(|_| place(r)).collect();
                    let h = r.below(nbsmeta::CS_MAX as u64 + 1) as usize;
                    let hosts = (0..h).map(|i| (i as u32, name(r, nbsmeta::NAME_MAX))).collect();
                    Said::Map { volume: r.below(1 << 32) as u32, size_mib: r.below(u64::MAX), places: k as u32,
                                index: r.below(u64::MAX), from: r.below(1 << 32) as u32, map, hosts }
                }
                3 => Said::Down { version: r.below(1 << 32) as u32, index: r.below(u64::MAX) },
                4 => Said::Behind { applied: r.below(u64::MAX) },
                5 => Said::NotNow(reason(r)),
                6 => Said::LastReplica,
                7 => Said::NotTheWriter,
                /* A copy-up's chunk (N10) */
                8 => Said::Chunk { chunk: r.below(1 << 32) as u32, incarnation: r.below(1 << 32) as u32,
                                   index: r.below(u64::MAX) },
                _ => Said::Refused(reason(r)),
            };
            let ok = match &said {
                Said::Map { hosts, .. } => hosts.iter().all(|(_, h)| h.len() <= nbsmeta::NAME_MAX),
                _ => true,
            };
            (5, Saying { id, said }.encode(), ok)
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
        3 => Answered::decode(b).map(|m| m.encode()),
        4 => Asking::decode(b).map(|m| m.encode()),
        _ => Saying::decode(b).map(|m| m.encode()),
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
        let (kind, bytes, ok) = message(&mut r, &data);
        let read = read_back(kind, &bytes).map_err(|e| format!("step {}: {}", step, e))?;
        if read != ok {
            return Err(format!("step {}: a message of kind {}, {} bytes, {} -- {}", step, kind, bytes.len(),
                               if ok { "within its limits" } else { "past a limit" },
                               if read { "read back" } else { "not read back" }));
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
        for k in 0..6 {
            read_back(k, &spoiled).map_err(|e| format!("step {}, spoiled: {}", step, e))?;
        }
        /* And noise */
        let n = r.below(64) as usize;
        let noise: Vec<u8> = (0..n).map(|_| r.below(256) as u8).collect();
        for k in 0..6 {
            read_back(k, &noise).map_err(|e| format!("step {}, noise: {}", step, e))?;
        }
    }
    Ok(())
}
