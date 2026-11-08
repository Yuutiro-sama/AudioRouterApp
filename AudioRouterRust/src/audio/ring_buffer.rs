//! Lock-Free Single-Producer Single-Consumer (SPSC) Ring Buffer.
//!
//! Cacheline-aligned (64 bytes) to eliminate false sharing between audio capture
//! and render threads. Sized to power of 2 for fast bitwise masking.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Default buffer capacity: 65,536 samples (32,768 stereo frames, ~682ms at 48kHz).
pub const DEFAULT_RING_BUFFER_CAPACITY: usize = 65536;

#[repr(align(64))]
struct CachePaddedAtomicUsize(AtomicUsize);

/// Lock-Free SPSC Ring Buffer for 32-bit floating point audio samples.
pub struct SpscRingBuffer {
    buffer: Box<[UnsafeCell<f32>]>,
    capacity: usize,
    mask: usize,
    write_idx: CachePaddedAtomicUsize,
    read_idx: CachePaddedAtomicUsize,
}

unsafe impl Sync for SpscRingBuffer {}
unsafe impl Send for SpscRingBuffer {}

impl SpscRingBuffer {
    /// Creates a new `SpscRingBuffer` with capacity that must be a power of two.
    pub fn new(power_of_two_capacity: usize) -> Self {
        assert!(
            power_of_two_capacity.is_power_of_two(),
            "Ring buffer capacity must be a power of two"
        );
        let mut vec = Vec::with_capacity(power_of_two_capacity);
        for _ in 0..power_of_two_capacity {
            vec.push(UnsafeCell::new(0.0f32));
        }
        Self {
            buffer: vec.into_boxed_slice(),
            capacity: power_of_two_capacity,
            mask: power_of_two_capacity - 1,
            write_idx: CachePaddedAtomicUsize(AtomicUsize::new(0)),
            read_idx: CachePaddedAtomicUsize(AtomicUsize::new(0)),
        }
    }

    /// Total capacity in f32 samples.
    #[inline(always)]
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Number of f32 samples currently available to read.
    #[inline(always)]
    pub fn available_read(&self) -> usize {
        let w = self.write_idx.0.load(Ordering::Acquire);
        let r = self.read_idx.0.load(Ordering::Relaxed);
        w.wrapping_sub(r)
    }

    /// Number of f32 samples that can be written without overrun.
    #[inline(always)]
    pub fn available_write(&self) -> usize {
        self.capacity.saturating_sub(self.available_read())
    }

    /// Pushes a slice of interleaved f32 samples into the ring buffer.
    /// Returns the number of samples actually written.
    pub fn push_slice(&self, data: &[f32]) -> usize {
        let w = self.write_idx.0.load(Ordering::Relaxed);
        let r = self.read_idx.0.load(Ordering::Acquire);
        let free = self.capacity.saturating_sub(w.wrapping_sub(r));
        let to_write = data.len().min(free);

        for (i, &sample) in data[..to_write].iter().enumerate() {
            unsafe {
                *self.buffer[(w.wrapping_add(i)) & self.mask].get() = sample;
            }
        }
        self.write_idx.0.store(w.wrapping_add(to_write), Ordering::Release);
        to_write
    }

    /// Peeks a stereo frame (Left, Right) at `frame_offset` from current read position
    /// without advancing the read index. Ideal for 4-point cubic Hermite interpolation.
    #[inline(always)]
    pub fn peek_frame_stereo(&self, frame_offset: usize) -> (f32, f32) {
        let r = self.read_idx.0.load(Ordering::Relaxed);
        let idx_l = (r.wrapping_add(frame_offset * 2)) & self.mask;
        let idx_r = (r.wrapping_add(frame_offset * 2 + 1)) & self.mask;
        unsafe { (*self.buffer[idx_l].get(), *self.buffer[idx_r].get()) }
    }

    /// Advances the read pointer by `frames` stereo frames (i.e. `frames * 2` samples).
    #[inline(always)]
    pub fn advance_read_frames(&self, frames: usize) {
        let samples = frames * 2;
        let r = self.read_idx.0.load(Ordering::Relaxed);
        self.read_idx.0.store(r.wrapping_add(samples), Ordering::Release);
    }

    /// Resets the read pointer so exactly `target_frames` stereo frames remain in the buffer.
    /// Used for fast overrun/underrun recovery.
    pub fn reset_to_target_frames(&self, target_frames: usize) {
        let w = self.write_idx.0.load(Ordering::Acquire);
        let desired_r = w.wrapping_sub(target_frames * 2);
        self.read_idx.0.store(desired_r, Ordering::Release);
    }

    /// Clears the ring buffer by setting read_idx = write_idx.
    pub fn clear(&self) {
        let w = self.write_idx.0.load(Ordering::Acquire);
        self.read_idx.0.store(w, Ordering::Release);
    }
}

impl Default for SpscRingBuffer {
    fn default() -> Self {
        Self::new(DEFAULT_RING_BUFFER_CAPACITY)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ring_buffer_basic_push_peek_advance() {
        let rb = SpscRingBuffer::new(64);
        assert_eq!(rb.available_read(), 0);
        assert_eq!(rb.available_write(), 64);

        // Push 2 stereo frames: [L0, R0, L1, R1]
        let input = [1.0f32, 2.0f32, 3.0f32, 4.0f32];
        let written = rb.push_slice(&input);
        assert_eq!(written, 4);
        assert_eq!(rb.available_read(), 4);
        assert_eq!(rb.available_write(), 60);

        // Peek frame 0 and frame 1
        let (l0, r0) = rb.peek_frame_stereo(0);
        assert_eq!((l0, r0), (1.0, 2.0));
        let (l1, r1) = rb.peek_frame_stereo(1);
        assert_eq!((l1, r1), (3.0, 4.0));

        // Advance 1 frame
        rb.advance_read_frames(1);
        assert_eq!(rb.available_read(), 2);

        // Now frame 0 should be previous frame 1
        let (l0_new, r0_new) = rb.peek_frame_stereo(0);
        assert_eq!((l0_new, r0_new), (3.0, 4.0));

        // Advance the last frame
        rb.advance_read_frames(1);
        assert_eq!(rb.available_read(), 0);
    }

    #[test]
    fn test_ring_buffer_wraparound() {
        let capacity = 32;
        let rb = SpscRingBuffer::new(capacity);

        // Push and read multiple times across the 32-sample boundary
        for cycle in 0..10 {
            let samples = [cycle as f32 * 2.0, cycle as f32 * 2.0 + 1.0];
            let written = rb.push_slice(&samples);
            assert_eq!(written, 2);

            let (l, r) = rb.peek_frame_stereo(0);
            assert_eq!(l, cycle as f32 * 2.0);
            assert_eq!(r, cycle as f32 * 2.0 + 1.0);

            rb.advance_read_frames(1);
            assert_eq!(rb.available_read(), 0);
        }
    }

    #[test]
    fn test_ring_buffer_reset_to_target() {
        let rb = SpscRingBuffer::new(1024);
        let data = vec![0.5f32; 800]; // 400 stereo frames
        let written = rb.push_slice(&data);
        assert_eq!(written, 800);
        assert_eq!(rb.available_read(), 800);

        // Reset to 100 target frames = 200 samples
        rb.reset_to_target_frames(100);
        assert_eq!(rb.available_read(), 200);
    }

    #[test]
    fn test_ring_buffer_full_overflow_prevention() {
        let rb = SpscRingBuffer::new(16);
        let data = vec![1.0f32; 32];
        let written = rb.push_slice(&data);
        assert_eq!(written, 16);
        assert_eq!(rb.available_read(), 16);
        assert_eq!(rb.available_write(), 0);

        // Pushing more should write 0 samples
        let written2 = rb.push_slice(&data);
        assert_eq!(written2, 0);
    }
}
