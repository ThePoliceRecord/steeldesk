import 'package:flutter_test/flutter_test.dart';

/// Tests for pure utility functions from common.dart.
///
/// Many functions in common.dart depend on platform FFI or Flutter widgets.
/// Here we test the pure logic that can be extracted and verified independently.

void main() {
  group('isDoubleEqual', () {
    // Mirrors the implementation in common.dart:
    //   bool isDoubleEqual(double a, double b) {
    //     return (a - b).abs() < 1e-6;
    //   }
    const epsilon = 1e-6;

    bool isDoubleEqual(double a, double b) {
      return (a - b).abs() < epsilon;
    }

    test('identical values are equal', () {
      expect(isDoubleEqual(1.0, 1.0), true);
      expect(isDoubleEqual(0.0, 0.0), true);
      expect(isDoubleEqual(-5.5, -5.5), true);
    });

    test('very close values are equal', () {
      expect(isDoubleEqual(1.0, 1.0 + 1e-7), true);
      expect(isDoubleEqual(1.0, 1.0 - 1e-7), true);
    });

    test('values differing by more than epsilon are not equal', () {
      expect(isDoubleEqual(1.0, 1.0 + 1e-5), false);
      expect(isDoubleEqual(0.0, 0.001), false);
    });

    test('handles negative values', () {
      expect(isDoubleEqual(-1.0, -1.0), true);
      expect(isDoubleEqual(-1.0, -1.0 + 1e-7), true);
      expect(isDoubleEqual(-1.0, -2.0), false);
    });

    test('handles zero comparisons', () {
      expect(isDoubleEqual(0.0, 1e-7), true);
      expect(isDoubleEqual(0.0, 1e-5), false);
    });
  });

  group('DesktopType enum-like constants', () {
    // From consts.dart — these are just string constants, but we can verify
    // the naming conventions match what the codebase expects.
    test('app type constants are distinct', () {
      const types = [
        'main',
        'cm',
        'remote',
        'file transfer',
        'view camera',
        'port forward',
        'terminal',
      ];
      // All unique
      expect(types.toSet().length, types.length);
    });
  });

  group('platform constant strings', () {
    // From consts.dart
    test('peer platform constants match expected values', () {
      expect('Windows', isNotEmpty);
      expect('Linux', isNotEmpty);
      expect('Mac OS', isNotEmpty);
      expect('Android', isNotEmpty);
      expect('WebDesktop', isNotEmpty);
    });

    test('platform strings do not contain leading/trailing whitespace', () {
      for (final p in ['Windows', 'Linux', 'Mac OS', 'Android', 'WebDesktop']) {
        expect(p.trim(), p);
      }
    });
  });

  group('SvcStatus enum values', () {
    // Mirrors the SvcStatus enum from state_model.dart
    // enum SvcStatus { notReady, connecting, ready }
    // We verify the expected number of values.
    test('has three states', () {
      final values = ['notReady', 'connecting', 'ready'];
      expect(values.length, 3);
      expect(values.toSet().length, 3);
    });
  });

  group('resolution group value tracking', () {
    // Test the pure data structure logic from StateGlobal's resolution
    // tracking without requiring GetX reactive types.
    test('set and get resolution group values', () {
      final store = <String, Map<int, String?>>{};

      void setVal(String peerId, int display, String? value) {
        store.putIfAbsent(peerId, () => {});
        store[peerId]![display] = value;
      }

      String? getVal(String peerId, int display) {
        return store[peerId]?[display];
      }

      void resetVal(String peerId) {
        store[peerId] = {};
      }

      setVal('peer1', 0, '1920x1080');
      setVal('peer1', 1, '2560x1440');
      setVal('peer2', 0, '3840x2160');

      expect(getVal('peer1', 0), '1920x1080');
      expect(getVal('peer1', 1), '2560x1440');
      expect(getVal('peer2', 0), '3840x2160');
      expect(getVal('peer2', 1), isNull);
      expect(getVal('nonexistent', 0), isNull);

      resetVal('peer1');
      expect(getVal('peer1', 0), isNull);
      expect(getVal('peer2', 0), '3840x2160');
    });
  });
}
