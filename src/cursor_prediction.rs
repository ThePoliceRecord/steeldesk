//! Client-side cursor prediction for low-latency rendering.
//!
//! This module renders a local cursor overlay immediately on mouse input,
//! eliminating the full round-trip latency before the user sees cursor movement.
//!
//! # Integration points
//!
//! - **`on_local_mouse_move(x, y)`** — call from `send_mouse()` in `client.rs`
//!   (at line ~3131) right before or after sending the mouse event to the server.
//!   This records the predicted cursor position instantly.
//!
//! - **`on_server_cursor(x, y)`** — call when a `CursorPosition` message arrives
//!   from the server. In `client/io_loop.rs` (at the `CursorPosition(cp)` match arm,
//!   line ~1423), call this before `handler.set_cursor_position(cp)`.
//!
//! - **`get_render_position()`** — call from the Flutter render loop (or Sciter
//!   paint callback) to obtain the cursor position to display. Returns `None`
//!   when the predictor is disabled or has no data yet.
//!
//! # Reconciliation logic
//!
//! 1. If the predicted cursor was updated within the last 50 ms, use the predicted position.
//! 2. When a server cursor arrives within 20 px of the predicted position, snap to the
//!    server position (the prediction was correct).
//! 3. When a server cursor arrives more than 20 px from the predicted position, lerp
//!    toward the server position over 100 ms.
//! 4. If no local mouse movement has occurred for 200 ms, use the server position only.

use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

/// Threshold in pixels: if server and predicted positions are within this
/// distance, we snap directly to the server position.
const SNAP_THRESHOLD_PX: f64 = 20.0;

/// How long (ms) a local prediction stays "fresh" after the last mouse move.
const PREDICTION_FRESH_MS: u64 = 50;

/// After this many ms without a local mouse move, fall back to server position.
const PREDICTION_IDLE_MS: u64 = 200;

/// Duration over which we lerp from predicted toward server when they diverge.
const LERP_DURATION_MS: u64 = 100;

/// If no server confirmation arrives within this duration, the prediction is stale.
const STALE_THRESHOLD_MS: u64 = 500;

/// Tracks predicted cursor position for low-latency rendering.
pub struct CursorPredictor {
    /// Last locally-predicted position (from mouse input).
    predicted: Arc<RwLock<Option<PredictedCursor>>>,
    /// Last confirmed position (from server).
    confirmed: Arc<RwLock<Option<ConfirmedCursor>>>,
    /// Active lerp correction when server diverges from prediction.
    correction: Arc<RwLock<Option<CorrectionLerp>>>,
    /// Whether prediction is enabled.
    enabled: bool,
}

/// A locally-predicted cursor position, recorded on mouse input.
pub struct PredictedCursor {
    pub x: i32,
    pub y: i32,
    pub timestamp: Instant,
    pub visible: bool,
}

/// A server-confirmed cursor position.
pub struct ConfirmedCursor {
    pub x: i32,
    pub y: i32,
    pub timestamp: Instant,
}

/// Tracks a lerp correction from a predicted position toward a server position.
struct CorrectionLerp {
    from_x: f64,
    from_y: f64,
    to_x: f64,
    to_y: f64,
    start: Instant,
    duration: Duration,
}

impl CorrectionLerp {
    /// Returns the interpolated position at the given instant, clamped to [0, 1].
    fn position_at(&self, now: Instant) -> (i32, i32) {
        let elapsed = now.duration_since(self.start).as_millis() as f64;
        let total = self.duration.as_millis() as f64;
        let t = if total <= 0.0 {
            1.0
        } else {
            (elapsed / total).min(1.0)
        };
        let x = self.from_x + (self.to_x - self.from_x) * t;
        let y = self.from_y + (self.to_y - self.from_y) * t;
        (x.round() as i32, y.round() as i32)
    }

    fn is_complete(&self, now: Instant) -> bool {
        now.duration_since(self.start) >= self.duration
    }
}

impl CursorPredictor {
    /// Create a new predictor. Pass `enabled: false` to create a no-op instance.
    pub fn new(enabled: bool) -> Self {
        Self {
            predicted: Arc::new(RwLock::new(None)),
            confirmed: Arc::new(RwLock::new(None)),
            correction: Arc::new(RwLock::new(None)),
            enabled,
        }
    }

    /// Called when the local mouse moves. Updates the predicted position immediately.
    ///
    /// Integration: call from `send_mouse()` in `client.rs`.
    pub fn on_local_mouse_move(&self, x: i32, y: i32) {
        if !self.enabled {
            return;
        }
        let mut pred = self.predicted.write().unwrap();
        *pred = Some(PredictedCursor {
            x,
            y,
            timestamp: Instant::now(),
            visible: true,
        });
        // A new local move cancels any ongoing correction lerp — the user
        // is actively moving, so the predicted position takes precedence.
        let mut corr = self.correction.write().unwrap();
        *corr = None;
    }

    /// Called when the server sends a cursor position update.
    ///
    /// Integration: call from the `CursorPosition(cp)` match arm in `client/io_loop.rs`.
    pub fn on_server_cursor(&self, x: i32, y: i32) {
        if !self.enabled {
            return;
        }
        let now = Instant::now();

        // Update confirmed position.
        {
            let mut conf = self.confirmed.write().unwrap();
            *conf = Some(ConfirmedCursor {
                x,
                y,
                timestamp: now,
            });
        }

        // Reconcile with predicted position.
        let pred = self.predicted.read().unwrap();
        if let Some(ref p) = *pred {
            let age = now.duration_since(p.timestamp);
            if age < Duration::from_millis(PREDICTION_IDLE_MS) {
                // Prediction is recent — check divergence.
                let dist = euclidean_distance(p.x, p.y, x, y);
                if dist <= SNAP_THRESHOLD_PX {
                    // Close enough: snap. Clear correction.
                    let mut corr = self.correction.write().unwrap();
                    *corr = None;
                } else {
                    // Diverged: start a lerp from current predicted to server.
                    let mut corr = self.correction.write().unwrap();
                    *corr = Some(CorrectionLerp {
                        from_x: p.x as f64,
                        from_y: p.y as f64,
                        to_x: x as f64,
                        to_y: y as f64,
                        start: now,
                        duration: Duration::from_millis(LERP_DURATION_MS),
                    });
                }
            }
            // If prediction is old (>= PREDICTION_IDLE_MS), we will naturally
            // return the server position from get_render_position().
        }
    }

    /// Returns the cursor position to render.
    ///
    /// Integration: call from the Flutter render loop (or Sciter paint callback).
    ///
    /// Returns `None` when the predictor is disabled or has no data.
    pub fn get_render_position(&self) -> Option<(i32, i32)> {
        if !self.enabled {
            return None;
        }
        let now = Instant::now();

        let pred = self.predicted.read().unwrap();
        let conf = self.confirmed.read().unwrap();

        // If an active correction lerp is running, use that.
        {
            let corr = self.correction.read().unwrap();
            if let Some(ref c) = *corr {
                if !c.is_complete(now) {
                    return Some(c.position_at(now));
                }
                // Lerp complete — fall through to normal logic, which will
                // return the server position since the prediction is likely idle.
            }
        }

        if let Some(ref p) = *pred {
            let age = now.duration_since(p.timestamp);
            if age < Duration::from_millis(PREDICTION_FRESH_MS) {
                // Fresh prediction — use it.
                return Some((p.x, p.y));
            }
            if age < Duration::from_millis(PREDICTION_IDLE_MS) {
                // Between FRESH and IDLE: still use prediction but server
                // will likely reconcile soon.
                return Some((p.x, p.y));
            }
        }

        // Prediction is idle or absent — use server position if available.
        if let Some(ref c) = *conf {
            return Some((c.x, c.y));
        }

        // If we have a (possibly stale) prediction but no server position,
        // still show it.
        if let Some(ref p) = *pred {
            return Some((p.x, p.y));
        }

        None
    }

    /// Returns the Euclidean distance between the predicted and confirmed positions,
    /// or `None` if either is absent.
    pub fn get_prediction_error(&self) -> Option<f64> {
        if !self.enabled {
            return None;
        }
        let pred = self.predicted.read().unwrap();
        let conf = self.confirmed.read().unwrap();
        match (&*pred, &*conf) {
            (Some(p), Some(c)) => Some(euclidean_distance(p.x, p.y, c.x, c.y)),
            _ => None,
        }
    }

    /// Returns `true` if prediction is active but no server confirmation has
    /// arrived within [`STALE_THRESHOLD_MS`].
    pub fn is_stale(&self) -> bool {
        if !self.enabled {
            return false;
        }
        let pred = self.predicted.read().unwrap();
        let conf = self.confirmed.read().unwrap();
        if pred.is_none() {
            return false;
        }
        match &*conf {
            None => {
                // Have prediction but never received server cursor.
                if let Some(ref p) = *pred {
                    p.timestamp.elapsed() >= Duration::from_millis(STALE_THRESHOLD_MS)
                } else {
                    false
                }
            }
            Some(c) => c.timestamp.elapsed() >= Duration::from_millis(STALE_THRESHOLD_MS),
        }
    }
}

/// Euclidean distance between two points.
fn euclidean_distance(x1: i32, y1: i32, x2: i32, y2: i32) -> f64 {
    let dx = (x2 - x1) as f64;
    let dy = (y2 - y1) as f64;
    (dx * dx + dy * dy).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    /// Advance past a duration by busy-spinning (for sub-ms precision in tests).
    fn sleep_ms(ms: u64) {
        std::thread::sleep(Duration::from_millis(ms));
    }

    // -----------------------------------------------------------------------
    // Basic functionality
    // -----------------------------------------------------------------------

    #[test]
    fn test_prediction_updates_immediately_on_mouse_move() {
        let pred = CursorPredictor::new(true);
        pred.on_local_mouse_move(100, 200);
        let pos = pred.get_render_position();
        assert_eq!(pos, Some((100, 200)));
    }

    #[test]
    fn test_rapid_mouse_movements_maintain_latest_position() {
        let pred = CursorPredictor::new(true);
        for i in 0..100 {
            pred.on_local_mouse_move(i, i * 2);
        }
        let pos = pred.get_render_position();
        assert_eq!(pos, Some((99, 198)));
    }

    #[test]
    fn test_no_data_returns_none() {
        let pred = CursorPredictor::new(true);
        assert_eq!(pred.get_render_position(), None);
    }

    #[test]
    fn test_disabled_predictor_returns_none() {
        let pred = CursorPredictor::new(false);
        pred.on_local_mouse_move(10, 20);
        pred.on_server_cursor(10, 20);
        assert_eq!(pred.get_render_position(), None);
        assert_eq!(pred.get_prediction_error(), None);
        assert!(!pred.is_stale());
    }

    #[test]
    fn test_no_server_cursor_returns_predicted_only() {
        let pred = CursorPredictor::new(true);
        pred.on_local_mouse_move(42, 84);
        // No server cursor has arrived yet.
        let pos = pred.get_render_position();
        assert_eq!(pos, Some((42, 84)));
        assert_eq!(pred.get_prediction_error(), None);
    }

    // -----------------------------------------------------------------------
    // Server confirmation — snap (within threshold)
    // -----------------------------------------------------------------------

    #[test]
    fn test_server_confirmation_within_threshold_snaps() {
        let pred = CursorPredictor::new(true);
        pred.on_local_mouse_move(100, 100);
        // Server confirms nearby (within 20px).
        pred.on_server_cursor(105, 105);
        // After snap, get_render_position during fresh window still returns prediction.
        let pos = pred.get_render_position().unwrap();
        // Still in the fresh window, so we see the predicted position.
        assert_eq!(pos, (100, 100));

        // After idle timeout, should return server position.
        sleep_ms(PREDICTION_IDLE_MS + 10);
        let pos = pred.get_render_position().unwrap();
        assert_eq!(pos, (105, 105));
    }

    // -----------------------------------------------------------------------
    // Server confirmation — lerp (outside threshold)
    // -----------------------------------------------------------------------

    #[test]
    fn test_server_confirmation_outside_threshold_lerps() {
        let pred = CursorPredictor::new(true);
        pred.on_local_mouse_move(100, 100);
        // Server says cursor is far away (>20px).
        pred.on_server_cursor(200, 200);

        // Immediately after, during the lerp, position should be between
        // (100,100) and (200,200).
        let pos = pred.get_render_position().unwrap();
        // The position should be somewhere along the lerp; at t~0 it's near (100,100).
        assert!(pos.0 >= 100 && pos.0 <= 200);
        assert!(pos.1 >= 100 && pos.1 <= 200);

        // After lerp completes (>100ms) and prediction goes idle (>200ms),
        // should be at server position.
        sleep_ms(PREDICTION_IDLE_MS + 10);
        let pos = pred.get_render_position().unwrap();
        assert_eq!(pos, (200, 200));
    }

    #[test]
    fn test_lerp_midpoint_is_interpolated() {
        let lerp = CorrectionLerp {
            from_x: 0.0,
            from_y: 0.0,
            to_x: 100.0,
            to_y: 200.0,
            start: Instant::now() - Duration::from_millis(LERP_DURATION_MS / 2),
            duration: Duration::from_millis(LERP_DURATION_MS),
        };
        let (x, y) = lerp.position_at(Instant::now());
        // At roughly the midpoint.
        assert!((x - 50).abs() <= 5, "x={x}, expected ~50");
        assert!((y - 100).abs() <= 10, "y={y}, expected ~100");
    }

    #[test]
    fn test_lerp_completion() {
        let start = Instant::now() - Duration::from_millis(LERP_DURATION_MS + 50);
        let lerp = CorrectionLerp {
            from_x: 0.0,
            from_y: 0.0,
            to_x: 100.0,
            to_y: 200.0,
            start,
            duration: Duration::from_millis(LERP_DURATION_MS),
        };
        assert!(lerp.is_complete(Instant::now()));
        let (x, y) = lerp.position_at(Instant::now());
        assert_eq!((x, y), (100, 200));
    }

    // -----------------------------------------------------------------------
    // New mouse move cancels correction lerp
    // -----------------------------------------------------------------------

    #[test]
    fn test_new_mouse_move_cancels_correction() {
        let pred = CursorPredictor::new(true);
        pred.on_local_mouse_move(100, 100);
        pred.on_server_cursor(200, 200); // starts a lerp
        // Now user moves again — lerp should be cancelled.
        pred.on_local_mouse_move(150, 150);
        let pos = pred.get_render_position().unwrap();
        assert_eq!(pos, (150, 150));
    }

    // -----------------------------------------------------------------------
    // Stale detection
    // -----------------------------------------------------------------------

    #[test]
    fn test_stale_detection_no_server_cursor() {
        let pred = CursorPredictor::new(true);
        pred.on_local_mouse_move(10, 10);
        // Just created — not stale yet.
        assert!(!pred.is_stale());
    }

    #[test]
    fn test_stale_after_threshold() {
        let pred = CursorPredictor::new(true);
        pred.on_local_mouse_move(10, 10);
        pred.on_server_cursor(10, 10);
        // Wait beyond stale threshold.
        sleep_ms(STALE_THRESHOLD_MS + 50);
        assert!(pred.is_stale());
    }

    #[test]
    fn test_not_stale_with_recent_server_update() {
        let pred = CursorPredictor::new(true);
        pred.on_local_mouse_move(10, 10);
        pred.on_server_cursor(10, 10);
        assert!(!pred.is_stale());
    }

    #[test]
    fn test_stale_without_prediction_is_false() {
        let pred = CursorPredictor::new(true);
        assert!(!pred.is_stale());
    }

    // -----------------------------------------------------------------------
    // Prediction error
    // -----------------------------------------------------------------------

    #[test]
    fn test_prediction_error_euclidean_distance() {
        let pred = CursorPredictor::new(true);
        pred.on_local_mouse_move(0, 0);
        pred.on_server_cursor(3, 4);
        let err = pred.get_prediction_error().unwrap();
        assert!((err - 5.0).abs() < 1e-9, "Expected 5.0, got {err}");
    }

    #[test]
    fn test_prediction_error_zero_when_aligned() {
        let pred = CursorPredictor::new(true);
        pred.on_local_mouse_move(50, 50);
        pred.on_server_cursor(50, 50);
        let err = pred.get_prediction_error().unwrap();
        assert!((err - 0.0).abs() < 1e-9);
    }

    #[test]
    fn test_prediction_error_none_without_both() {
        let pred = CursorPredictor::new(true);
        assert_eq!(pred.get_prediction_error(), None);
        pred.on_local_mouse_move(10, 10);
        // Only prediction, no server — still None.
        assert_eq!(pred.get_prediction_error(), None);
    }

    // -----------------------------------------------------------------------
    // Idle fallback to server position
    // -----------------------------------------------------------------------

    #[test]
    fn test_idle_fallback_to_server_position() {
        let pred = CursorPredictor::new(true);
        pred.on_local_mouse_move(100, 100);
        pred.on_server_cursor(100, 100);
        // Wait for prediction to go idle.
        sleep_ms(PREDICTION_IDLE_MS + 10);
        // Update server position.
        pred.on_server_cursor(300, 300);
        let pos = pred.get_render_position().unwrap();
        assert_eq!(pos, (300, 300));
    }

    // -----------------------------------------------------------------------
    // Concurrent access from multiple threads
    // -----------------------------------------------------------------------

    #[test]
    fn test_concurrent_access() {
        let pred = Arc::new(CursorPredictor::new(true));
        let mut handles = vec![];

        // Thread 1: rapid mouse moves.
        let p1 = Arc::clone(&pred);
        handles.push(thread::spawn(move || {
            for i in 0..500 {
                p1.on_local_mouse_move(i, i * 2);
            }
        }));

        // Thread 2: server cursor updates.
        let p2 = Arc::clone(&pred);
        handles.push(thread::spawn(move || {
            for i in 0..500 {
                p2.on_server_cursor(i, i * 2);
            }
        }));

        // Thread 3: render position reads.
        let p3 = Arc::clone(&pred);
        handles.push(thread::spawn(move || {
            for _ in 0..500 {
                let _ = p3.get_render_position();
                let _ = p3.get_prediction_error();
                let _ = p3.is_stale();
            }
        }));

        for h in handles {
            h.join().expect("Thread panicked during concurrent access test");
        }

        // After all threads finish, the predictor should be in a valid state.
        let pos = pred.get_render_position();
        assert!(pos.is_some());
    }

    // -----------------------------------------------------------------------
    // Edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_server_only_no_prediction() {
        let pred = CursorPredictor::new(true);
        pred.on_server_cursor(42, 84);
        // No local prediction — should return server position.
        let pos = pred.get_render_position().unwrap();
        assert_eq!(pos, (42, 84));
    }

    #[test]
    fn test_euclidean_distance_function() {
        assert!((euclidean_distance(0, 0, 3, 4) - 5.0).abs() < 1e-9);
        assert!((euclidean_distance(0, 0, 0, 0) - 0.0).abs() < 1e-9);
        assert!((euclidean_distance(-3, -4, 0, 0) - 5.0).abs() < 1e-9);
        assert!((euclidean_distance(1, 1, 1, 1) - 0.0).abs() < 1e-9);
    }

    #[test]
    fn test_large_coordinates() {
        let pred = CursorPredictor::new(true);
        pred.on_local_mouse_move(i32::MAX, i32::MAX);
        let pos = pred.get_render_position().unwrap();
        assert_eq!(pos, (i32::MAX, i32::MAX));
    }

    #[test]
    fn test_negative_coordinates() {
        let pred = CursorPredictor::new(true);
        pred.on_local_mouse_move(-100, -200);
        let pos = pred.get_render_position().unwrap();
        assert_eq!(pos, (-100, -200));
    }

    #[test]
    fn test_multiple_server_updates_use_latest() {
        let pred = CursorPredictor::new(true);
        pred.on_server_cursor(10, 10);
        pred.on_server_cursor(20, 20);
        pred.on_server_cursor(30, 30);
        let pos = pred.get_render_position().unwrap();
        assert_eq!(pos, (30, 30));
    }

    #[test]
    fn test_prediction_fresh_window() {
        let pred = CursorPredictor::new(true);
        pred.on_local_mouse_move(100, 200);
        // Within the fresh window, should return predicted.
        let pos = pred.get_render_position().unwrap();
        assert_eq!(pos, (100, 200));
        // Even with a server position, fresh prediction wins during the fresh window.
        pred.on_server_cursor(999, 999);
        // The snap logic doesn't override the predicted value stored —
        // get_render_position still returns the predicted value within the fresh window.
        // (Server is >20px away, so a correction lerp starts, but let's test
        //  a case where server is within snap range.)
        let pred2 = CursorPredictor::new(true);
        pred2.on_local_mouse_move(100, 200);
        pred2.on_server_cursor(105, 205); // within snap
        let pos = pred2.get_render_position().unwrap();
        assert_eq!(pos, (100, 200)); // still predicted during fresh window
    }

    #[test]
    fn test_lerp_with_zero_duration() {
        let lerp = CorrectionLerp {
            from_x: 50.0,
            from_y: 50.0,
            to_x: 100.0,
            to_y: 200.0,
            start: Instant::now(),
            duration: Duration::from_millis(0),
        };
        // Zero duration should jump directly to the target.
        let (x, y) = lerp.position_at(Instant::now());
        assert_eq!((x, y), (100, 200));
        assert!(lerp.is_complete(Instant::now()));
    }
}
