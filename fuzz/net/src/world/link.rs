//! The link between the world's hosts and the machine: how late a frame
//! arrives, whether it does, whether twice, and so out of order -- what the
//! input says the network is like.

use crate::Input;

#[derive(Clone)]
pub struct Link {
    /// Of 256 frames, how many are lost; how many arrive twice.
    pub loss: u8,
    pub dup: u8,
    /// How late a frame arrives: this, and up to `jitter` more -- which
    /// reorders frames sent close together.
    pub delay: u64,
    pub jitter: u64,
    rng: u64,
}

const MS: u64 = 1_000_000;

impl Link {
    /// Every frame, at once, once.
    pub fn perfect() -> Link {
        Link { loss: 0, dup: 0, delay: 0, jitter: 0, rng: 1 }
    }

    /// A link as the input has it: a good one mostly -- a run that loses
    /// much gets little done -- and now and then a bad one.
    pub fn from_input(r: &mut Input) -> Link {
        let seed = u64::from(r.u32()) | 1;
        match r.u8() % 8 {
            0..=2 => Link { loss: 0, dup: 0, delay: 0, jitter: 0, rng: seed },
            3 | 4 => Link { loss: 0, dup: 0, delay: r.below(50) * MS, jitter: r.below(20) * MS, rng: seed },
            5 => Link { loss: r.pick(&[2u8, 8, 26]), dup: 0, delay: r.below(30) * MS, jitter: 0, rng: seed },
            6 => Link { loss: r.pick(&[2u8, 8]), dup: r.pick(&[4u8, 26]), delay: r.below(10) * MS,
                        jitter: r.below(100) * MS, rng: seed },
            _ => Link { loss: r.pick(&[26u8, 64, 128]), dup: r.pick(&[0u8, 26]), delay: r.below(300) * MS,
                        jitter: r.below(300) * MS, rng: seed },
        }
    }

    fn next(&mut self) -> u64 {
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng = x;
        x
    }

    /// When a frame sent at `now` arrives: never, once, or twice.
    pub fn fate(&mut self, now: u64, _len: usize) -> Vec<u64> {
        if (self.next() % 256) < u64::from(self.loss) {
            return Vec::new();
        }
        let mut at = vec![self.arrival(now)];
        if (self.next() % 256) < u64::from(self.dup) {
            at.push(self.arrival(now));
        }
        at
    }

    fn arrival(&mut self, now: u64) -> u64 {
        let jitter = if self.jitter == 0 { 0 } else { self.next() % self.jitter };
        now.saturating_add(self.delay).saturating_add(jitter)
    }

    /// Whether the link loses or reorders anything: what a check that
    /// counts on every frame arriving asks.
    pub fn lossless(&self) -> bool {
        self.loss == 0 && self.dup == 0 && self.jitter == 0
    }
}
