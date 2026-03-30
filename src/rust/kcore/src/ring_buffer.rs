use core::mem::MaybeUninit;

/// Fixed-capacity ring buffer (FIFO queue).
///
/// `T` is the element type, `N` is the maximum number of elements held inline.
/// Does not allocate from the heap.  Thread safety is the caller's
/// responsibility — wrap in a `SpinLock` when accessed from multiple contexts.
///
/// `push` returns `Err(val)` when full so the caller retains ownership of the
/// rejected value.  `pop` returns `None` when empty.
///
/// # Example
/// ```ignore
/// let mut rb = RingBuffer::<u32, 8>::new();
/// rb.push(1).ok();
/// rb.push(2).ok();
/// assert_eq!(rb.pop(), Some(1));
/// ```
pub struct RingBuffer<T, const N: usize> {
    buf:   [MaybeUninit<T>; N],
    head:  usize,
    count: usize,
}

impl<T, const N: usize> RingBuffer<T, N> {
    pub const fn new() -> Self {
        Self {
            buf:   unsafe { MaybeUninit::uninit().assume_init() },
            head:  0,
            count: 0,
        }
    }

    /// Push a value onto the tail.  Returns `Err(val)` if the buffer is full.
    pub fn push(&mut self, val: T) -> Result<(), T> {
        if self.count == N {
            return Err(val);
        }
        let tail = (self.head + self.count) % N;
        self.buf[tail].write(val);
        self.count = self.count + 1;
        Ok(())
    }

    /// Pop a value from the head.  Returns `None` if the buffer is empty.
    pub fn pop(&mut self) -> Option<T> {
        if self.count == 0 {
            return None;
        }
        let val = unsafe { self.buf[self.head].assume_init_read() };
        self.head = (self.head + 1) % N;
        self.count = self.count - 1;
        Some(val)
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    pub fn is_full(&self) -> bool {
        self.count == N
    }

    pub fn len(&self) -> usize {
        self.count
    }

    pub fn capacity(&self) -> usize {
        N
    }

    /// Discard all elements.
    pub fn clear(&mut self) {
        while self.pop().is_some() {}
    }
}

impl<T, const N: usize> Drop for RingBuffer<T, N> {
    fn drop(&mut self) {
        self.clear();
    }
}
