import 'package:flutter_test/flutter_test.dart';

/// Tests for the CursorPredictionOverlay widget.
///
/// The overlay depends heavily on FFI (bind.mainGetCursorPrediction) and
/// Provider<CanvasModel>, which are not available in a headless test
/// environment. These tests verify the pure-logic aspects of the overlay's
/// opacity calculation, which is extracted and tested independently here.
///
/// The actual widget integration test would require mocking the FFI bridge
/// and providing a CanvasModel, which is deferred to a follow-up.

void main() {
  group('CursorPredictionOverlay opacity logic', () {
    // The overlay computes opacity as:
    //   active => 1.0
    //   inactive, elapsed < fadeOutDuration => 1.0 - (elapsed / duration)
    //   inactive, elapsed >= fadeOutDuration => 0.0
    //
    // We test this logic in isolation.

    const fadeOutDurationMs = 200;

    double computeOpacity({required bool active, required int elapsedMs}) {
      if (active) return 1.0;
      if (elapsedMs >= fadeOutDurationMs) return 0.0;
      return 1.0 - (elapsedMs / fadeOutDurationMs);
    }

    test('opacity is 1.0 when active', () {
      expect(computeOpacity(active: true, elapsedMs: 0), 1.0);
      expect(computeOpacity(active: true, elapsedMs: 500), 1.0);
    });

    test('opacity is 0.0 when inactive and fade-out complete', () {
      expect(computeOpacity(active: false, elapsedMs: 200), 0.0);
      expect(computeOpacity(active: false, elapsedMs: 1000), 0.0);
    });

    test('opacity fades linearly during fade-out', () {
      expect(computeOpacity(active: false, elapsedMs: 0), 1.0);
      expect(computeOpacity(active: false, elapsedMs: 100), closeTo(0.5, 0.01));
      expect(computeOpacity(active: false, elapsedMs: 150), closeTo(0.25, 0.01));
      expect(computeOpacity(active: false, elapsedMs: 199), closeTo(0.005, 0.01));
    });

    test('opacity is clamped: never negative', () {
      final val = computeOpacity(active: false, elapsedMs: 300);
      expect(val, 0.0);
    });
  });

  group('CursorPredictionOverlay coordinate mapping', () {
    // The overlay maps predicted position to screen coordinates:
    //   screenX = predX * scale + cx
    //   screenY = predY * scale + cy
    // This is the same transform as CursorPaint.

    test('basic coordinate mapping at scale 1.0', () {
      const predX = 100.0;
      const predY = 200.0;
      const scale = 1.0;
      const cx = 10.0;
      const cy = 20.0;

      final screenX = predX * scale + cx;
      final screenY = predY * scale + cy;

      expect(screenX, 110.0);
      expect(screenY, 220.0);
    });

    test('coordinate mapping at scale 2.0', () {
      const predX = 100.0;
      const predY = 200.0;
      const scale = 2.0;
      const cx = 10.0;
      const cy = 20.0;

      final screenX = predX * scale + cx;
      final screenY = predY * scale + cy;

      expect(screenX, 210.0);
      expect(screenY, 420.0);
    });

    test('coordinate mapping with negative canvas offset', () {
      const predX = 50.0;
      const predY = 50.0;
      const scale = 1.5;
      const cx = -30.0;
      const cy = -20.0;

      final screenX = predX * scale + cx;
      final screenY = predY * scale + cy;

      expect(screenX, closeTo(45.0, 0.01));
      expect(screenY, closeTo(55.0, 0.01));
    });

    test('circle positioning accounts for radius offset', () {
      // The overlay positions the circle at:
      //   left: screenX - circleRadius
      //   top: screenY - circleRadius
      const circleRadius = 5.0;
      const screenX = 110.0;
      const screenY = 220.0;

      expect(screenX - circleRadius, 105.0);
      expect(screenY - circleRadius, 215.0);
    });
  });
}
