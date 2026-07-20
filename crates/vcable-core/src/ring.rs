use std::cell::UnsafeCell;
use std::error::Error;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RingBufferError {
    CapacityTooSmall,
    CapacityNotPowerOfTwo,
}

impl fmt::Display for RingBufferError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CapacityTooSmall => write!(f, "ring-buffer capacity must be at least two"),
            Self::CapacityNotPowerOfTwo => {
                write!(f, "ring-buffer capacity must be a power of two")
            }
        }
    }
}

impl Error for RingBufferError {}

struct Inner {
    samples: Box<[UnsafeCell<f32>]>,
    mask: usize,
    read: AtomicUsize,
    write: AtomicUsize,
}

// Safety: the public API creates exactly one Producer and one Consumer. Only
// the producer writes sample slots, only the consumer reads them, and the
// acquire/release index operations publish those accesses.
unsafe impl Sync for Inner {}

pub struct Producer {
    inner: Arc<Inner>,
}

pub struct Consumer {
    inner: Arc<Inner>,
}

pub fn spsc_ring_buffer(capacity: usize) -> Result<(Producer, Consumer), RingBufferError> {
    if capacity < 2 {
        return Err(RingBufferError::CapacityTooSmall);
    }
    if !capacity.is_power_of_two() {
        return Err(RingBufferError::CapacityNotPowerOfTwo);
    }
    let samples = (0..capacity)
        .map(|_| UnsafeCell::new(0.0))
        .collect::<Vec<_>>()
        .into_boxed_slice();
    let inner = Arc::new(Inner {
        samples,
        mask: capacity - 1,
        read: AtomicUsize::new(0),
        write: AtomicUsize::new(0),
    });
    Ok((
        Producer {
            inner: Arc::clone(&inner),
        },
        Consumer { inner },
    ))
}

impl Producer {
    pub fn capacity(&self) -> usize {
        self.inner.samples.len()
    }

    pub fn available(&self) -> usize {
        let read = self.inner.read.load(Ordering::Acquire);
        let write = self.inner.write.load(Ordering::Relaxed);
        self.capacity() - write.wrapping_sub(read)
    }

    /// Writes as many samples as fit and returns the written sample count.
    pub fn push_slice(&mut self, input: &[f32]) -> usize {
        let write = self.inner.write.load(Ordering::Relaxed);
        let read = self.inner.read.load(Ordering::Acquire);
        let count = input.len().min(self.capacity() - write.wrapping_sub(read));
        for (offset, sample) in input[..count].iter().copied().enumerate() {
            let index = write.wrapping_add(offset) & self.inner.mask;
            // Safety: Producer is unique, and the consumer cannot access a
            // sample until the release-store below publishes the new index.
            unsafe { *self.inner.samples[index].get() = sample };
        }
        self.inner
            .write
            .store(write.wrapping_add(count), Ordering::Release);
        count
    }

    /// Writes one sample, returning `false` when the buffer is full.
    pub fn push(&mut self, sample: f32) -> bool {
        let write = self.inner.write.load(Ordering::Relaxed);
        let read = self.inner.read.load(Ordering::Acquire);
        if write.wrapping_sub(read) == self.capacity() {
            return false;
        }
        let index = write & self.inner.mask;
        // Safety: Producer is unique and publishes the slot below.
        unsafe { *self.inner.samples[index].get() = sample };
        self.inner
            .write
            .store(write.wrapping_add(1), Ordering::Release);
        true
    }
}

impl Consumer {
    pub fn capacity(&self) -> usize {
        self.inner.samples.len()
    }

    pub fn available(&self) -> usize {
        let read = self.inner.read.load(Ordering::Relaxed);
        let write = self.inner.write.load(Ordering::Acquire);
        write.wrapping_sub(read)
    }

    /// Reads as many samples as available and returns the read sample count.
    pub fn pop_slice(&mut self, output: &mut [f32]) -> usize {
        let read = self.inner.read.load(Ordering::Relaxed);
        let write = self.inner.write.load(Ordering::Acquire);
        let count = output.len().min(write.wrapping_sub(read));
        for (offset, sample) in output[..count].iter_mut().enumerate() {
            let index = read.wrapping_add(offset) & self.inner.mask;
            // Safety: Consumer is unique, and acquire-loading the write index
            // ensures the producer completed the corresponding sample write.
            *sample = unsafe { *self.inner.samples[index].get() };
        }
        self.inner
            .read
            .store(read.wrapping_add(count), Ordering::Release);
        count
    }

    /// Reads one sample, returning `None` when the buffer is empty.
    pub fn pop(&mut self) -> Option<f32> {
        let read = self.inner.read.load(Ordering::Relaxed);
        let write = self.inner.write.load(Ordering::Acquire);
        if write == read {
            return None;
        }
        let index = read & self.inner.mask;
        // Safety: Consumer is unique and the acquire-load observes publication.
        let sample = unsafe { *self.inner.samples[index].get() };
        self.inner
            .read
            .store(read.wrapping_add(1), Ordering::Release);
        Some(sample)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_without_overwriting_unread_samples() {
        let (mut producer, mut consumer) = spsc_ring_buffer(4).unwrap();
        assert_eq!(producer.push_slice(&[1.0, 2.0, 3.0]), 3);
        let mut first = [0.0; 2];
        assert_eq!(consumer.pop_slice(&mut first), 2);
        assert_eq!(first, [1.0, 2.0]);
        assert_eq!(producer.push_slice(&[4.0, 5.0, 6.0, 7.0]), 3);
        let mut second = [0.0; 4];
        assert_eq!(consumer.pop_slice(&mut second), 4);
        assert_eq!(second, [3.0, 4.0, 5.0, 6.0]);
    }
}
