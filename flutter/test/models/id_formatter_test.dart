import 'package:flutter_test/flutter_test.dart';
import 'package:flutter_hbb/common/formatter/id_formatter.dart';

void main() {
  group('formatID', () {
    test('returns empty string unchanged', () {
      expect(formatID(''), '');
    });

    test('returns 1-3 digit IDs without spaces', () {
      expect(formatID('1'), '1');
      expect(formatID('12'), '12');
      expect(formatID('123'), '123');
    });

    test('groups digits in threes with leading group', () {
      // 4 digits: 1 + 3
      expect(formatID('1234'), '1 234');
      // 5 digits: 2 + 3
      expect(formatID('12345'), '12 345');
      // 6 digits: 3 + 3
      expect(formatID('123456'), '123 456');
      // 7 digits: 1 + 3 + 3
      expect(formatID('1234567'), '1 234 567');
      // 9 digits: 3 + 3 + 3
      expect(formatID('123456789'), '123 456 789');
    });

    test('strips existing spaces before reformatting', () {
      expect(formatID('123 456'), '123 456');
      expect(formatID('1 2 3 4 5 6'), '123 456');
    });

    test('returns non-numeric IDs unchanged', () {
      expect(formatID('abc'), 'abc');
      expect(formatID('12a34'), '12a34');
      expect(formatID('test-peer'), 'test-peer');
    });

    test(r'preserves \r suffix', () {
      expect(formatID(r'123456\r'), r'123 456\r');
    });

    test(r'preserves /r suffix', () {
      expect(formatID('123456/r'), '123 456/r');
    });

    test(r'non-numeric with \r suffix is returned as-is', () {
      // After stripping \r, if the remainder is not numeric, return original
      expect(formatID(r'abc\r'), r'abc\r');
    });
  });

  group('trimID', () {
    test('removes all spaces', () {
      expect(trimID('123 456 789'), '123456789');
    });

    test('returns already trimmed ID unchanged', () {
      expect(trimID('123456'), '123456');
    });

    test('handles empty string', () {
      expect(trimID(''), '');
    });

    test('handles multiple consecutive spaces', () {
      expect(trimID('1  2  3'), '123');
    });
  });
}
