//! Triple-buffer–style frame buffer for decoupling capture and encode threads.
//!
//! # Motivation (Phase 2-1 of performance optimization plan)
//!
//! Currently the video service (`video_service.rs`) captures and encodes in a
//! single loop:
//!
//! ```text
//! loop {
//!     capture frame      // ~5ms
//!     encode frame       // ~10-15ms
//!     wait for ACK       // reduced but still present
//!     sleep to hit FPS   // remainder of 1/fps budget
//! }
//! ```
//!
//! At 60fps the per-frame budget is 16.6ms.  If capture takes 5ms and encode
//! takes 15ms, the loop already exceeds the budget and frames are dropped.
//!
//! # Design
//!
//! `FrameBuffer` holds a single `Option<CapturedFrame>` behind a `Mutex`.
//! The capture side *overwrites* the slot on every capture (latest-wins),
//! and the encode side *takes* the slot, leaving `None` until the next
//! capture completes.  This means:
//!
//! - The capture thread is never blocked by a slow encoder — it just
//!   overwrites the previous unconsumed frame.
//! - The encode thread always gets the freshest available frame, or `None`
//!   if capture hasn't produced anything new yet.
//! - No unbounded queue growth; memory is bounded to one frame at a time.
//!
//! # Integration guide
//!
//! To wire this into the existing `run()` loop in `video_service.rs`:
//!
//! 1. Spawn a dedicated **capture thread** that calls `c.frame()` in a tight
//!    loop at the target FPS cadence and stores results via
//!    `FrameBuffer::store()`.
//!
//! 2. The existing loop becomes the **encode thread**: instead of calling
//!    `c.frame()` directly it calls `FrameBuffer::take()` to get the latest
//!    captured frame.
//!
//! 3. The capture thread sleeps based on `spf` (seconds-per-frame) minus
//!    actual capture time; the encode thread runs as fast as it can consume
//!    frames, naturally pacing itself by the availability of new frames.
//!
//! This eliminates the coupling where a slow encode stalls the next capture.

use scrap::Pixfmt;
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// A single captured frame, ready to be encoded.
#[derive(Debug, Clone)]
pub struct CapturedFrame {
    /// Raw pixel data (typically BGRA or similar, depending on the capturer).
    pub data: Vec<u8>,
    /// Width of the captured image in pixels.
    pub width: usize,
    /// Height of the captured image in pixels.
    pub height: usize,
    /// Row stride in bytes (may be larger than `width * bytes_per_pixel` due to
    /// alignment padding).
    pub stride: usize,
    /// Pixel format of the captured data, needed for YUV conversion during
    /// the encode phase.
    pub pixfmt: Pixfmt,
    /// The instant at which this frame was captured — used for latency
    /// measurement and frame-age decisions.
    pub capture_time: Instant,
    /// Index of the display this frame was captured from, matching the
    /// `display_idx` used throughout `video_service.rs`.
    pub display_idx: usize,
}

/// Latest-wins frame buffer for decoupling a capture producer from an encode
/// consumer.
///
/// See the module-level docs for the full design rationale.
pub struct FrameBuffer {
    /// The latest captured frame.  `None` means either no frame has been
    /// stored yet, or the last stored frame has already been taken by the
    /// encoder.
    latest: Arc<Mutex<Option<CapturedFrame>>>,
}

impl FrameBuffer {
    /// Create an empty frame buffer.
    pub fn new() -> Self {
        FrameBuffer {
            latest: Arc::new(Mutex::new(None)),
        }
    }

    /// Store a new frame, overwriting any previously unconsumed frame.
    ///
    /// Called by the **capture thread**.  This never blocks on the encode side
    /// — the lock is held only for the duration of a pointer swap.
    pub fn store(&self, frame: CapturedFrame) {
        let mut guard = self.latest.lock().expect("FrameBuffer lock poisoned");
        *guard = Some(frame);
    }

    /// Take the latest frame, leaving the buffer empty.
    ///
    /// Called by the **encode thread**.  Returns `None` if no new frame is
    /// available (i.e. the capture thread has not produced one since the last
    /// `take()`).
    pub fn take(&self) -> Option<CapturedFrame> {
        let mut guard = self.latest.lock().expect("FrameBuffer lock poisoned");
        guard.take()
    }

    /// Check whether a new frame is available without consuming it.
    pub fn has_new_frame(&self) -> bool {
        let guard = self.latest.lock().expect("FrameBuffer lock poisoned");
        guard.is_some()
    }
}

// P2-1 status: FrameBuffer is integrated into run() with temporal decoupling.
// Capture stores into the buffer; encode takes from the buffer at the top of
// the next iteration.  Full thread split is documented as a TODO in
// video_service.rs (requires capturer to be Send+Sync).

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::{Duration, Instant};

    /// Helper: build a `CapturedFrame` with the given distinguishing values.
    fn make_frame(width: usize, height: usize, display_idx: usize) -> CapturedFrame {
        let stride = width * 4; // assume BGRA
        let data = vec![0u8; stride * height];
        CapturedFrame {
            data,
            width,
            height,
            stride,
            pixfmt: Pixfmt::BGRA,
            capture_time: Instant::now(),
            display_idx,
        }
    }

    #[test]
    fn store_and_take_round_trip() {
        let buf = FrameBuffer::new();
        let frame = make_frame(1920, 1080, 0);
        buf.store(frame);
        let got = buf.take().expect("take should return the stored frame");
        assert_eq!(got.width, 1920);
        assert_eq!(got.height, 1080);
        assert_eq!(got.display_idx, 0);
        assert_eq!(got.stride, 1920 * 4);
        assert_eq!(got.data.len(), 1920 * 4 * 1080);
    }

    #[test]
    fn take_returns_none_when_empty() {
        let buf = FrameBuffer::new();
        assert!(buf.take().is_none(), "empty buffer should return None");
    }

    #[test]
    fn store_overwrites_old_frame_latest_wins() {
        let buf = FrameBuffer::new();
        buf.store(make_frame(800, 600, 0));
        buf.store(make_frame(1920, 1080, 1));

        let got = buf.take().expect("should get the latest frame");
        assert_eq!(got.width, 1920);
        assert_eq!(got.height, 1080);
        assert_eq!(got.display_idx, 1);
    }

    #[test]
    fn concurrent_store_and_take() {
        let buf = Arc::new(FrameBuffer::new());
        let buf_producer = Arc::clone(&buf);
        let buf_consumer = Arc::clone(&buf);

        let num_frames = 1000;

        let producer = thread::spawn(move || {
            for i in 0..num_frames {
                buf_producer.store(make_frame(i + 1, i + 1, i));
            }
        });

        let consumer = thread::spawn(move || {
            let mut taken = 0u64;
            // Keep trying to take until the producer is done and the buffer is
            // drained.  We bound iterations to avoid an infinite loop in case
            // of a bug.
            for _ in 0..(num_frames * 100) {
                if let Some(_frame) = buf_consumer.take() {
                    taken += 1;
                }
                // tiny yield so the producer has a chance to run
                thread::yield_now();
            }
            taken
        });

        producer.join().expect("producer panicked");
        let taken = consumer.join().expect("consumer panicked");

        // The consumer should have taken at least 1 frame and at most
        // `num_frames` frames.  Because of latest-wins semantics, many
        // stores may be overwritten before the consumer gets a chance to
        // take, so `taken` is typically much less than `num_frames`.
        assert!(taken >= 1, "consumer should have taken at least 1 frame");
        assert!(
            taken <= num_frames as u64,
            "consumer should not take more frames than produced"
        );
    }

    #[test]
    fn has_new_frame_reflects_state() {
        let buf = FrameBuffer::new();
        assert!(!buf.has_new_frame(), "empty buffer has no frame");

        buf.store(make_frame(640, 480, 0));
        assert!(buf.has_new_frame(), "should have a frame after store");

        let _ = buf.take();
        assert!(!buf.has_new_frame(), "should have no frame after take");
    }

    #[test]
    fn captured_frame_preserves_all_fields() {
        let before = Instant::now();
        let frame = CapturedFrame {
            data: vec![1, 2, 3, 4, 5],
            width: 3840,
            height: 2160,
            stride: 3840 * 4 + 64, // some alignment padding
            pixfmt: Pixfmt::RGBA,
            capture_time: Instant::now(),
            display_idx: 2,
        };
        let after = Instant::now();

        let buf = FrameBuffer::new();
        buf.store(frame);
        let got = buf.take().unwrap();

        assert_eq!(got.data, vec![1, 2, 3, 4, 5]);
        assert_eq!(got.width, 3840);
        assert_eq!(got.height, 2160);
        assert_eq!(got.stride, 3840 * 4 + 64);
        assert_eq!(got.pixfmt, Pixfmt::RGBA);
        assert_eq!(got.display_idx, 2);
        assert!(got.capture_time >= before);
        assert!(got.capture_time <= after);
    }

    #[test]
    fn multiple_rapid_stores_only_keeps_latest() {
        let buf = FrameBuffer::new();

        for i in 0..100 {
            buf.store(make_frame(i + 1, i + 1, i));
        }

        let got = buf.take().expect("should have the latest frame");
        assert_eq!(got.width, 100);
        assert_eq!(got.height, 100);
        assert_eq!(got.display_idx, 99);

        // Only one frame should have been buffered
        assert!(buf.take().is_none(), "buffer should be empty after one take");
    }

    #[test]
    fn take_clears_buffer_second_take_returns_none() {
        let buf = FrameBuffer::new();
        buf.store(make_frame(1280, 720, 0));

        let first = buf.take();
        assert!(first.is_some(), "first take should succeed");

        let second = buf.take();
        assert!(second.is_none(), "second take should return None");
    }

    #[test]
    fn store_after_take_works() {
        let buf = FrameBuffer::new();

        buf.store(make_frame(800, 600, 0));
        let _ = buf.take();

        buf.store(make_frame(1024, 768, 1));
        let got = buf.take().expect("should get frame stored after take");
        assert_eq!(got.width, 1024);
        assert_eq!(got.height, 768);
        assert_eq!(got.display_idx, 1);
    }

    #[test]
    fn concurrent_multiple_producers_single_consumer() {
        let buf = Arc::new(FrameBuffer::new());
        let num_producers = 4;
        let frames_per_producer = 250;

        let mut producers = Vec::new();
        for p in 0..num_producers {
            let buf_clone = Arc::clone(&buf);
            producers.push(thread::spawn(move || {
                for i in 0..frames_per_producer {
                    buf_clone.store(make_frame(p * 1000 + i, 1, p));
                    thread::yield_now();
                }
            }));
        }

        let buf_consumer = Arc::clone(&buf);
        let consumer = thread::spawn(move || {
            let mut taken = 0u64;
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                if let Some(_) = buf_consumer.take() {
                    taken += 1;
                }
                thread::yield_now();
            }
            taken
        });

        for p in producers {
            p.join().expect("producer panicked");
        }
        let taken = consumer.join().expect("consumer panicked");

        assert!(taken >= 1, "consumer should have taken at least 1 frame");
        assert!(
            taken <= (num_producers * frames_per_producer) as u64,
            "cannot take more than total produced"
        );
    }
}
