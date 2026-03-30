//! Encoder thread for the capture/encode split in low-latency mode.
//!
//! # Overview
//!
//! The platform capturer holds `!Send` handles (X11 connections, DXGI
//! pointers, etc.) so it cannot be moved to another thread.  Instead, we
//! move the **encoder** to a separate thread: the capturer stays on the
//! main thread and stores raw pixel data into a [`FrameBuffer`], while the
//! encoder thread reads from it, performs YUV conversion + encoding, and
//! sends [`EncodedFrame`]s back via a bounded channel.
//!
//! This fully decouples capture and encode so they can run in parallel
//! on different cores, eliminating the pipeline stall where a slow encode
//! blocks the next capture.

use super::frame_buffer::{CapturedFrame, EncodedFrame, FrameBuffer};
use hbb_common::log;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// Handle returned by [`spawn_encoder_thread`].
///
/// Owns the join handle and the stop flag.  Dropping this handle signals
/// the encoder thread to stop and joins it.
pub struct EncoderThreadHandle {
    /// Set to `false` to request the encoder thread to exit.
    running: Arc<AtomicBool>,
    /// Join handle for the encoder thread.
    handle: Option<JoinHandle<()>>,
}

impl EncoderThreadHandle {
    /// Signal the encoder thread to stop and block until it exits.
    pub fn stop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }

    /// Check whether the encoder thread is still marked as running.
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }
}

impl Drop for EncoderThreadHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The encode function type used by the encoder thread.
///
/// Given a [`CapturedFrame`], perform YUV conversion and encoding, returning
/// an [`EncodedFrame`] on success.  The function is called on the encoder
/// thread and must be `Send + 'static`.
///
/// In production this wraps `scrap::convert_to_yuv` + `Encoder::encode_to_message`.
/// In tests it can be replaced with a simple stub.
pub type EncodeFn = Box<dyn FnMut(CapturedFrame) -> Option<EncodedFrame> + Send + 'static>;

/// Spawn an encoder thread that reads from `frame_buffer` and sends
/// encoded frames through `encoded_tx`.
///
/// # Arguments
///
/// * `frame_buffer` - Shared frame buffer that the capture thread writes to.
/// * `encoded_tx` - Bounded channel sender for encoded frames.
/// * `encode_fn` - The function that converts a `CapturedFrame` into an
///   `EncodedFrame`.  Created on the caller side so it can capture encoder
///   configuration, but it runs on the encoder thread.
///
/// # Returns
///
/// An [`EncoderThreadHandle`] that can be used to stop the thread.
pub fn spawn_encoder_thread(
    frame_buffer: Arc<FrameBuffer>,
    encoded_tx: SyncSender<EncodedFrame>,
    mut encode_fn: EncodeFn,
) -> EncoderThreadHandle {
    let running = Arc::new(AtomicBool::new(true));
    let running_flag = running.clone();

    let handle = thread::Builder::new()
        .name("steeldesk-encoder".into())
        .spawn(move || {
            log::info!("encoder thread started");
            while running_flag.load(Ordering::Relaxed) {
                if let Some(frame) = frame_buffer.take() {
                    if let Some(encoded) = encode_fn(frame) {
                        match encoded_tx.try_send(encoded) {
                            Ok(()) => {}
                            Err(TrySendError::Full(_)) => {
                                // Channel is full — drop this encoded frame.
                                // The capture thread will keep producing fresh
                                // frames; dropping an encoded frame here is
                                // preferable to blocking the encoder.
                                log::trace!("encoder thread: channel full, dropping frame");
                            }
                            Err(TrySendError::Disconnected(_)) => {
                                log::info!("encoder thread: channel disconnected, stopping");
                                break;
                            }
                        }
                    }
                } else {
                    // No frame available yet — brief sleep to avoid busy-spinning.
                    thread::sleep(Duration::from_millis(1));
                }
            }
            log::info!("encoder thread stopped");
        })
        .expect("failed to spawn encoder thread");

    EncoderThreadHandle {
        running,
        handle: Some(handle),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use scrap::Pixfmt;
    use std::sync::mpsc;
    use std::time::Instant;

    /// Helper: build a `CapturedFrame` with the given dimensions.
    fn make_frame(width: usize, height: usize) -> CapturedFrame {
        let stride = width * 4;
        CapturedFrame {
            data: vec![128u8; stride * height],
            width,
            height,
            stride,
            pixfmt: Pixfmt::BGRA,
            capture_time: Instant::now(),
            display_idx: 0,
        }
    }

    /// Stub encode function: just compresses the data trivially.
    fn stub_encode(frame: CapturedFrame) -> Option<EncodedFrame> {
        Some(EncodedFrame {
            data: vec![0u8; frame.width * frame.height / 100],
            key: true,
            pts: 0,
        })
    }

    #[test]
    fn encoder_thread_processes_single_frame() {
        let fb = Arc::new(FrameBuffer::new());
        let (tx, rx) = mpsc::sync_channel::<EncodedFrame>(4);

        let handle = spawn_encoder_thread(
            fb.clone(),
            tx,
            Box::new(|f| stub_encode(f)),
        );

        // Store a frame for the encoder to pick up.
        fb.store(make_frame(1920, 1080));

        // Wait for the encoded frame.
        let ef = rx.recv_timeout(Duration::from_secs(2))
            .expect("should receive encoded frame");
        assert!(!ef.data.is_empty());
        assert!(ef.key);

        drop(handle);
    }

    #[test]
    fn encoder_thread_processes_multiple_frames() {
        let fb = Arc::new(FrameBuffer::new());
        let (tx, rx) = mpsc::sync_channel::<EncodedFrame>(8);

        let mut handle = spawn_encoder_thread(
            fb.clone(),
            tx,
            Box::new(|f| stub_encode(f)),
        );

        // Store and wait for several frames.
        let mut received = 0;
        for _ in 0..5 {
            fb.store(make_frame(640, 480));
            // Give the encoder thread time to process.
            if let Ok(_) = rx.recv_timeout(Duration::from_secs(1)) {
                received += 1;
            }
        }
        assert!(received >= 1, "should have received at least 1 encoded frame");

        handle.stop();
        assert!(!handle.is_running());
    }

    #[test]
    fn encoder_thread_stops_on_signal() {
        let fb = Arc::new(FrameBuffer::new());
        let (tx, _rx) = mpsc::sync_channel::<EncodedFrame>(4);

        let mut handle = spawn_encoder_thread(
            fb.clone(),
            tx,
            Box::new(|f| stub_encode(f)),
        );

        assert!(handle.is_running());
        handle.stop();
        assert!(!handle.is_running());
    }

    #[test]
    fn encoder_thread_stops_on_channel_disconnect() {
        let fb = Arc::new(FrameBuffer::new());
        let (tx, rx) = mpsc::sync_channel::<EncodedFrame>(1);

        let handle = spawn_encoder_thread(
            fb.clone(),
            tx,
            Box::new(|f| stub_encode(f)),
        );

        // Drop the receiver to disconnect the channel.
        drop(rx);

        // Store a frame so the encoder tries to send and discovers disconnect.
        fb.store(make_frame(320, 240));

        // The thread should stop on its own.
        // Drop handle which calls stop() + join().
        drop(handle);
    }

    #[test]
    fn encoder_thread_drops_frame_when_channel_full() {
        // Channel capacity = 1
        let fb = Arc::new(FrameBuffer::new());
        let (tx, rx) = mpsc::sync_channel::<EncodedFrame>(1);

        let handle = spawn_encoder_thread(
            fb.clone(),
            tx,
            Box::new(|f| stub_encode(f)),
        );

        // Store two frames quickly — the encoder should encode both but
        // may drop the second if the channel is full.
        fb.store(make_frame(640, 480));
        thread::sleep(Duration::from_millis(50));
        fb.store(make_frame(640, 480));
        thread::sleep(Duration::from_millis(50));

        // We should get at least 1 frame.
        let first = rx.recv_timeout(Duration::from_secs(1));
        assert!(first.is_ok(), "should receive at least one frame");

        drop(handle);
    }

    #[test]
    fn encoder_thread_encode_fn_returning_none_skips() {
        let fb = Arc::new(FrameBuffer::new());
        let (tx, rx) = mpsc::sync_channel::<EncodedFrame>(4);

        let handle = spawn_encoder_thread(
            fb.clone(),
            tx,
            Box::new(|_f| None), // encode always fails
        );

        fb.store(make_frame(640, 480));
        thread::sleep(Duration::from_millis(100));

        // Should not receive anything since encode returns None.
        assert!(rx.try_recv().is_err());

        drop(handle);
    }

    #[test]
    fn encoder_thread_handle_drop_stops_thread() {
        let fb = Arc::new(FrameBuffer::new());
        let (tx, _rx) = mpsc::sync_channel::<EncodedFrame>(4);

        let handle = spawn_encoder_thread(
            fb.clone(),
            tx,
            Box::new(|f| stub_encode(f)),
        );

        // Just drop it — should not hang.
        drop(handle);
    }

    #[test]
    fn encoder_thread_with_pts_sequence() {
        use std::sync::atomic::AtomicI64;

        let fb = Arc::new(FrameBuffer::new());
        let (tx, rx) = mpsc::sync_channel::<EncodedFrame>(16);
        let pts_counter = Arc::new(AtomicI64::new(0));
        let pts = pts_counter.clone();

        let handle = spawn_encoder_thread(
            fb.clone(),
            tx,
            Box::new(move |frame| {
                let p = pts.fetch_add(16, Ordering::Relaxed);
                Some(EncodedFrame {
                    data: vec![0u8; frame.width],
                    key: p == 0,
                    pts: p,
                })
            }),
        );

        for _ in 0..5 {
            fb.store(make_frame(100, 100));
            thread::sleep(Duration::from_millis(20));
        }

        // Collect all received frames.
        thread::sleep(Duration::from_millis(100));
        let mut frames = Vec::new();
        while let Ok(ef) = rx.try_recv() {
            frames.push(ef);
        }

        assert!(!frames.is_empty(), "should have received some frames");
        // First frame should be a keyframe.
        assert!(frames[0].key, "first frame should be a keyframe");
        assert_eq!(frames[0].pts, 0);
        // PTS values should be monotonically increasing.
        for i in 1..frames.len() {
            assert!(
                frames[i].pts > frames[i - 1].pts,
                "PTS should be monotonically increasing"
            );
        }

        drop(handle);
    }
}
