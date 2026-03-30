import 'dart:convert';

import 'package:flutter/material.dart';
import 'package:flutter/scheduler.dart';
import 'package:provider/provider.dart';

import '../../models/model.dart';
import '../../models/platform_model.dart';

/// Overlay that renders a semi-transparent circle at the predicted cursor
/// position when cursor prediction is active (low-latency mode).
///
/// The predicted position comes from the Rust cursor predictor via FFI
/// (`main_get_cursor_prediction`). This position leads the server-confirmed
/// cursor by one round-trip, giving the user immediate visual feedback.
///
/// ## Usage
///
/// Wrap the remote canvas `Stack` in `getBodyForDesktop()`:
/// ```dart
/// CursorPredictionOverlay(
///   child: existingRemoteCanvasStack,
/// )
/// ```
///
/// ## Manual testing
///
/// 1. Connect to a remote desktop with `low-latency-mode=Y` in peer options.
/// 2. Move the mouse briskly. A small blue-tinted circle should lead the
///    actual remote cursor by a few pixels.
/// 3. Stop moving. The circle should fade out within ~200 ms.
/// 4. With low-latency mode off, no circle should appear.
class CursorPredictionOverlay extends StatefulWidget {
  final Widget child;

  const CursorPredictionOverlay({Key? key, required this.child})
      : super(key: key);

  @override
  State<CursorPredictionOverlay> createState() =>
      _CursorPredictionOverlayState();
}

class _CursorPredictionOverlayState extends State<CursorPredictionOverlay>
    with SingleTickerProviderStateMixin {
  late final Ticker _ticker;

  // Predicted position in remote-image coordinates.
  double _predX = 0;
  double _predY = 0;
  bool _active = false;

  // Fade-out tracking: when the prediction goes inactive we fade over 200 ms.
  DateTime _lastActiveTime = DateTime.now();
  static const _fadeOutDuration = Duration(milliseconds: 200);

  // Visual constants.
  static const double _circleRadius = 5.0;
  static const Color _circleColor = Color(0x664A90D9); // semi-transparent blue

  @override
  void initState() {
    super.initState();
    _ticker = createTicker(_onTick);
    _ticker.start();
  }

  @override
  void dispose() {
    _ticker.dispose();
    super.dispose();
  }

  void _onTick(Duration _) {
    _pollPrediction();
  }

  void _pollPrediction() {
    try {
      final json = bind.mainGetCursorPrediction();
      final data = jsonDecode(json) as Map<String, dynamic>;
      final nowActive = data['active'] == true;

      if (nowActive) {
        final newX = (data['x'] as num).toDouble();
        final newY = (data['y'] as num).toDouble();
        if (newX != _predX || newY != _predY || !_active) {
          setState(() {
            _predX = newX;
            _predY = newY;
            _active = true;
            _lastActiveTime = DateTime.now();
          });
        }
      } else if (_active) {
        // Prediction just became inactive — start fade-out.
        setState(() {
          _active = false;
        });
      } else {
        // Still inactive — check if fade-out is complete and skip rebuild.
        final elapsed = DateTime.now().difference(_lastActiveTime);
        if (elapsed < _fadeOutDuration) {
          // Force rebuild to continue the fade animation.
          setState(() {});
        }
      }
    } catch (_) {
      // FFI not available or JSON parse error — silently ignore.
    }
  }

  @override
  Widget build(BuildContext context) {
    // Compute opacity: full when active, fading when recently deactivated.
    double opacity;
    if (_active) {
      opacity = 1.0;
    } else {
      final elapsed = DateTime.now().difference(_lastActiveTime);
      if (elapsed >= _fadeOutDuration) {
        opacity = 0.0;
      } else {
        opacity = 1.0 - (elapsed.inMilliseconds / _fadeOutDuration.inMilliseconds);
      }
    }

    // If fully invisible, just return the child without any overlay.
    if (opacity <= 0.0) {
      return widget.child;
    }

    // Map predicted position (remote image coords) to local canvas coords,
    // using the same transform as CursorPaint.
    final c = Provider.of<CanvasModel>(context);

    double cx = c.x;
    double cy = c.y;
    if (c.viewStyle.style == kRemoteViewStyleOriginal &&
        c.scrollStyle == ScrollStyle.scrollbar) {
      final rect = c.parent.target?.ffiModel.rect;
      if (rect != null) {
        if (cx < 0) {
          cx = -rect.width * c.scale * c.scrollX;
        }
        if (cy < 0) {
          cy = -rect.height * c.scale * c.scrollY;
        }
      }
    }

    final screenX = _predX * c.scale + cx;
    final screenY = _predY * c.scale + cy;

    return Stack(
      children: [
        widget.child,
        Positioned(
          left: screenX - _circleRadius,
          top: screenY - _circleRadius,
          child: IgnorePointer(
            child: Opacity(
              opacity: opacity.clamp(0.0, 1.0),
              child: Container(
                width: _circleRadius * 2,
                height: _circleRadius * 2,
                decoration: BoxDecoration(
                  shape: BoxShape.circle,
                  color: _circleColor,
                ),
              ),
            ),
          ),
        ),
      ],
    );
  }
}
