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

/// An encoded frame ready to be sent to connections.
///
/// Produced by the encoder thread after YUV conversion and encoding.
/// Sent back to the main (capture) thread via a bounded channel so it can
/// be dispatched to connected peers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedFrame {
    /// Encoded bitstream data (VP8/VP9/H264/H265/AV1).
    pub data: Vec<u8>,
    /// Whether this is a keyframe.
    pub key: bool,
    /// Presentation timestamp in milliseconds (relative to session start).
    pub pts: i64,
}

// P2-1 status: FrameBuffer is integrated into run() with temporal decoupling.
// The encoder thread split is implemented: the capturer stays on the main
// thread (it holds !Send platform handles), while the encoder runs on a
// separate thread that reads from the FrameBuffer via Arc.

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

    // =======================================================================
    // EncodedFrame tests
    // =======================================================================

    #[test]
    fn encoded_frame_struct_fields() {
        let ef = EncodedFrame {
            data: vec![0xDE, 0xAD, 0xBE, 0xEF],
            key: true,
            pts: 12345,
        };
        assert_eq!(ef.data, vec![0xDE, 0xAD, 0xBE, 0xEF]);
        assert!(ef.key);
        assert_eq!(ef.pts, 12345);
    }

    #[test]
    fn encoded_frame_clone_and_eq() {
        let ef1 = EncodedFrame {
            data: vec![1, 2, 3],
            key: false,
            pts: 42,
        };
        let ef2 = ef1.clone();
        assert_eq!(ef1, ef2);
    }

    #[test]
    fn encoded_frame_empty_data() {
        let ef = EncodedFrame {
            data: vec![],
            key: false,
            pts: 0,
        };
        assert!(ef.data.is_empty());
        assert_eq!(ef.pts, 0);
    }

    #[test]
    fn encoded_frame_channel_send_recv() {
        // Verify EncodedFrame can be sent through a bounded sync_channel,
        // mirroring the encoder thread -> main thread communication.
        let (tx, rx) = std::sync::mpsc::sync_channel::<EncodedFrame>(2);

        let ef = EncodedFrame {
            data: vec![0x00, 0x00, 0x01],
            key: true,
            pts: 1000,
        };
        tx.send(ef.clone()).expect("send should succeed");

        let received = rx.recv().expect("recv should succeed");
        assert_eq!(received, ef);
    }

    #[test]
    fn encoded_frame_channel_try_recv_empty() {
        let (_tx, rx) = std::sync::mpsc::sync_channel::<EncodedFrame>(2);
        assert!(
            rx.try_recv().is_err(),
            "try_recv on empty channel should return Err"
        );
    }

    #[test]
    fn encoded_frame_channel_multiple_frames() {
        let (tx, rx) = std::sync::mpsc::sync_channel::<EncodedFrame>(4);

        for i in 0..4 {
            tx.send(EncodedFrame {
                data: vec![i as u8],
                key: i == 0,
                pts: i * 33,
            })
            .expect("send should succeed");
        }

        for i in 0..4 {
            let ef = rx.recv().expect("recv should succeed");
            assert_eq!(ef.data, vec![i as u8]);
            assert_eq!(ef.key, i == 0);
            assert_eq!(ef.pts, i * 33);
        }
    }

    #[test]
    fn encoded_frame_channel_cross_thread() {
        let (tx, rx) = std::sync::mpsc::sync_channel::<EncodedFrame>(2);

        let producer = thread::spawn(move || {
            for i in 0..10 {
                tx.send(EncodedFrame {
                    data: vec![i as u8; 100],
                    key: i % 5 == 0,
                    pts: i * 16,
                })
                .expect("send should succeed");
            }
        });

        let consumer = thread::spawn(move || {
            let mut count = 0;
            for _ in 0..10 {
                let ef = rx.recv().expect("recv should succeed");
                assert_eq!(ef.data.len(), 100);
                count += 1;
            }
            count
        });

        producer.join().expect("producer panicked");
        let count = consumer.join().expect("consumer panicked");
        assert_eq!(count, 10);
    }

    // =======================================================================
    // FrameBuffer Arc sharing between threads (encoder thread pattern)
    // =======================================================================

    #[test]
    fn frame_buffer_arc_capture_encode_pattern() {
        // Simulates the full capture/encode thread split pattern:
        // - Capture thread: stores frames into Arc<FrameBuffer>
        // - Encoder thread: takes frames, "encodes" them, sends EncodedFrame via channel
        // - Main thread: receives EncodedFrame from channel
        use std::sync::atomic::{AtomicBool, Ordering};

        let fb = Arc::new(FrameBuffer::new());
        let running = Arc::new(AtomicBool::new(true));
        let (encoded_tx, encoded_rx) = std::sync::mpsc::sync_channel::<EncodedFrame>(2);

        // Capture thread
        let fb_capture = Arc::clone(&fb);
        let running_capture = Arc::clone(&running);
        let capture_thread = thread::spawn(move || {
            let mut frame_count = 0u64;
            while running_capture.load(Ordering::Relaxed) && frame_count < 50 {
                fb_capture.store(make_frame(1920, 1080, 0));
                frame_count += 1;
                thread::yield_now();
            }
            frame_count
        });

        // Encoder thread
        let fb_encoder = Arc::clone(&fb);
        let running_encoder = Arc::clone(&running);
        let encoder_thread = thread::spawn(move || {
            let mut encoded_count = 0i64;
            while running_encoder.load(Ordering::Relaxed) {
                if let Some(cf) = fb_encoder.take() {
                    // Simulate encoding: just produce an EncodedFrame
                    let ef = EncodedFrame {
                        data: vec![0u8; cf.width * cf.height / 100], // "compressed"
                        key: encoded_count == 0,
                        pts: encoded_count * 16,
                    };
                    // Use try_send to avoid blocking when the channel is full
                    // (mirrors the real encoder thread's non-blocking behavior).
                    match encoded_tx.try_send(ef) {
                        Ok(()) => encoded_count += 1,
                        Err(std::sync::mpsc::TrySendError::Full(_)) => {
                            // Channel full — drop this frame, keep going.
                        }
                        Err(std::sync::mpsc::TrySendError::Disconnected(_)) => break,
                    }
                } else {
                    thread::yield_now();
                }
            }
            encoded_count
        });

        // Main thread: receive encoded frames while waiting for threads
        let captured = capture_thread.join().expect("capture thread panicked");
        running.store(false, Ordering::Relaxed);
        let encoded = encoder_thread.join().expect("encoder thread panicked");

        // Drain any remaining encoded frames from the channel
        let mut received = 0i64;
        while let Ok(_ef) = encoded_rx.try_recv() {
            received += 1;
        }

        assert!(captured > 0, "should have captured frames");
        assert!(encoded > 0, "should have encoded at least 1 frame");
        assert!(received > 0, "should have received at least 1 encoded frame");
        assert!(
            received <= encoded,
            "cannot receive more than encoded: received={}, encoded={}",
            received,
            encoded
        );
    }
}
