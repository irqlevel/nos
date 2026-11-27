use alloc::rc::Rc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum NbsCsStepStage {
    ZeroStart,
    ZeroDone,
    PutStart,
    PutDone,
    WriteStart,
    WriteDone,
    FlushStart,
    FlushDone,
    CommitStart,
    CommitDone,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum NbsTurnStage {
    Wait,
    Acquire,
    Release,
}

/// What the caller records of the engine's operations, with no kernel here.
pub trait Trace {
    fn now(&self) -> u64;
    fn step_enabled(&self) -> bool;
    fn step(&self, volume: u32, chunk: u32, offset: u32, len: u32, stage: NbsCsStepStage, since: u64);
    fn turn_enabled(&self) -> bool;
    fn turn(&self, id: u64, key: u32, alone: bool, stage: NbsTurnStage, since: u64);
}

#[inline]
pub(crate) fn step_since(tracer: Option<&Rc<dyn Trace>>) -> u64 {
    match tracer {
        Some(t) if t.step_enabled() => t.now(),
        _ => 0,
    }
}

#[inline]
pub(crate) fn step(tracer: Option<&Rc<dyn Trace>>, volume: u32, chunk: u32, offset: u32, len: u32,
                   stage: NbsCsStepStage, since: u64) {
    if let Some(t) = tracer.filter(|t| t.step_enabled()) {
        t.step(volume, chunk, offset, len, stage, since);
    }
}

#[inline]
pub(crate) fn turn_since(tracer: Option<&Rc<dyn Trace>>) -> u64 {
    match tracer {
        Some(t) if t.turn_enabled() => t.now(),
        _ => 0,
    }
}

#[inline]
pub(crate) fn turn(tracer: Option<&Rc<dyn Trace>>, id: u64, key: u32, alone: bool, stage: NbsTurnStage, since: u64) {
    if let Some(t) = tracer.filter(|t| t.turn_enabled()) {
        t.turn(id, key, alone, stage, since);
    }
}
