use std::vec::Drain;


/// Reusable scratch storage that preserves entries belonging to outer parser frames.
#[derive(Debug)]
pub(super) struct ScratchBuffer<T> {
    buffer: Vec<T>,
}

impl<T> ScratchBuffer<T> {
    pub(super) fn new() -> Self {
        Self { buffer: Vec::new() }
    }

    pub(super) fn with_capacity(capacity: usize) -> Self {
        Self {
            buffer: Vec::with_capacity(capacity),
        }
    }

    #[inline]
    pub(super) fn push(&mut self, value: T) {
        self.buffer.push(value);
    }

    #[inline]
    pub(super) fn is_empty(&self, snapshot: &ScratchSnapshot) -> bool {
        debug_assert!(
            self.buffer.len() >= snapshot.len,
            "Scratch buffer snapshots must be restored in reverse order of creation."
        );
        self.buffer.len() == snapshot.len
    }

    #[inline]
    pub(super) fn snapshot(&self) -> ScratchSnapshot {
        ScratchSnapshot {
            len: self.buffer.len(),
            restored: false,
        }
    }

    #[inline]
    pub(super) fn take<C: FromIterator<T>>(&mut self, snapshot: ScratchSnapshot) -> C {
        self.drain_snapshot(snapshot).collect()
    }

    #[inline]
    pub(super) fn take_thin_vec(&mut self, mut snapshot: ScratchSnapshot) -> Vec<T> {
        if self.is_empty(&snapshot) {
            snapshot.restored = true;
            return Vec::new();
        }

        let drain = self.drain_snapshot(snapshot);
        let mut result = Vec::with_capacity(drain.len());
        result.extend(drain);
        result
    }

    #[inline]
    fn drain_snapshot(&mut self, mut snapshot: ScratchSnapshot) -> Drain<'_, T> {
        debug_assert!(
            self.buffer.len() >= snapshot.len,
            "Scratch buffer snapshots must be restored in reverse order of creation."
        );
        snapshot.restored = true;
        self.buffer.drain(snapshot.len..)
    }
}

impl<T> Drop for ScratchBuffer<T> {
    fn drop(&mut self) {
        debug_assert!(
            self.buffer.is_empty() || std::thread::panicking(),
            "Scratch buffers must be empty when dropped."
        );
    }
}

pub(super) struct ScratchSnapshot {
    len: usize,
    restored: bool,
}

impl Drop for ScratchSnapshot {
    fn drop(&mut self) {
        debug_assert!(
            self.restored || std::thread::panicking(),
            "Scratch buffer snapshots must be restored."
        );
    }
}
