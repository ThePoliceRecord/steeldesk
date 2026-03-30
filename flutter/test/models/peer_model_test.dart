import 'dart:convert';

import 'package:flutter_test/flutter_test.dart';
import 'package:flutter_hbb/models/peer_model.dart';

void main() {
  group('Peer', () {
    Peer _makePeer({
      String id = 'abc123',
      String hash = 'h1',
      String password = 'pw',
      String username = 'user1',
      String hostname = 'host1',
      String platform = 'Linux',
      String alias = '',
      List<dynamic> tags = const ['tag1'],
      bool forceAlwaysRelay = false,
      String rdpPort = '',
      String rdpUsername = '',
      String loginName = '',
      String deviceGroupName = '',
      String note = '',
      bool? sameServer,
    }) {
      return Peer(
        id: id,
        hash: hash,
        password: password,
        username: username,
        hostname: hostname,
        platform: platform,
        alias: alias,
        tags: List.from(tags),
        forceAlwaysRelay: forceAlwaysRelay,
        rdpPort: rdpPort,
        rdpUsername: rdpUsername,
        loginName: loginName,
        device_group_name: deviceGroupName,
        note: note,
        sameServer: sameServer,
      );
    }

    group('constructor and default values', () {
      test('stores all fields correctly', () {
        final p = _makePeer();
        expect(p.id, 'abc123');
        expect(p.hash, 'h1');
        expect(p.password, 'pw');
        expect(p.username, 'user1');
        expect(p.hostname, 'host1');
        expect(p.platform, 'Linux');
        expect(p.alias, '');
        expect(p.tags, ['tag1']);
        expect(p.forceAlwaysRelay, false);
        expect(p.rdpPort, '');
        expect(p.rdpUsername, '');
        expect(p.loginName, '');
        expect(p.device_group_name, '');
        expect(p.note, '');
        expect(p.sameServer, isNull);
        expect(p.online, false);
      });
    });

    group('Peer.loading()', () {
      test('has placeholder values', () {
        final p = Peer.loading();
        expect(p.id, '...');
        expect(p.username, '...');
        expect(p.hostname, '...');
        expect(p.platform, '...');
        expect(p.hash, '');
        expect(p.password, '');
        expect(p.alias, '');
        expect(p.tags, isEmpty);
        expect(p.forceAlwaysRelay, false);
      });
    });

    group('getId()', () {
      test('returns alias when alias is set', () {
        final p = _makePeer(alias: 'MyAlias');
        expect(p.getId(), 'MyAlias');
      });

      test('returns id when alias is empty', () {
        final p = _makePeer(alias: '');
        expect(p.getId(), 'abc123');
      });
    });

    group('fromJson', () {
      test('parses a full JSON map', () {
        final json = {
          'id': 'peer1',
          'hash': 'hashval',
          'password': 'secret',
          'username': 'admin',
          'hostname': 'server1',
          'platform': 'Windows',
          'alias': 'mypc',
          'tags': ['office', 'dev'],
          'forceAlwaysRelay': 'true',
          'rdpPort': '3389',
          'rdpUsername': 'rdpuser',
          'loginName': 'login1',
          'device_group_name': 'group1',
          'note': 'a note',
          'same_server': true,
        };

        final p = Peer.fromJson(json);
        expect(p.id, 'peer1');
        expect(p.hash, 'hashval');
        expect(p.password, 'secret');
        expect(p.username, 'admin');
        expect(p.hostname, 'server1');
        expect(p.platform, 'Windows');
        expect(p.alias, 'mypc');
        expect(p.tags, ['office', 'dev']);
        expect(p.forceAlwaysRelay, true);
        expect(p.rdpPort, '3389');
        expect(p.rdpUsername, 'rdpuser');
        expect(p.loginName, 'login1');
        expect(p.device_group_name, 'group1');
        expect(p.note, 'a note');
        expect(p.sameServer, true);
      });

      test('uses defaults for missing keys', () {
        final p = Peer.fromJson({});
        expect(p.id, '');
        expect(p.hash, '');
        expect(p.password, '');
        expect(p.username, '');
        expect(p.hostname, '');
        expect(p.platform, '');
        expect(p.alias, '');
        expect(p.tags, []);
        expect(p.forceAlwaysRelay, false);
        expect(p.rdpPort, '');
        expect(p.rdpUsername, '');
        expect(p.loginName, '');
        expect(p.device_group_name, '');
        expect(p.note, '');
        expect(p.sameServer, isNull);
      });

      test('forceAlwaysRelay is false when value is not "true"', () {
        final p1 = Peer.fromJson({'forceAlwaysRelay': 'false'});
        expect(p1.forceAlwaysRelay, false);

        final p2 = Peer.fromJson({'forceAlwaysRelay': 'True'});
        expect(p2.forceAlwaysRelay, false);

        final p3 = Peer.fromJson({'forceAlwaysRelay': '1'});
        expect(p3.forceAlwaysRelay, false);
      });

      test('note defaults to empty string when not a String', () {
        final p = Peer.fromJson({'note': 42});
        expect(p.note, '');
      });
    });

    group('toJson', () {
      test('round-trips through fromJson', () {
        final original = _makePeer(
          id: 'rt1',
          hash: 'h',
          password: 'p',
          username: 'u',
          hostname: 'hn',
          platform: 'Mac OS',
          alias: 'al',
          tags: ['t1', 't2'],
          forceAlwaysRelay: true,
          rdpPort: '1234',
          rdpUsername: 'ru',
          loginName: 'ln',
          deviceGroupName: 'dg',
          note: 'n',
          sameServer: false,
        );

        final json = original.toJson();
        final restored = Peer.fromJson(json);

        expect(restored.id, original.id);
        expect(restored.hash, original.hash);
        expect(restored.password, original.password);
        expect(restored.username, original.username);
        expect(restored.hostname, original.hostname);
        expect(restored.platform, original.platform);
        expect(restored.alias, original.alias);
        expect(restored.tags, original.tags);
        expect(restored.forceAlwaysRelay, original.forceAlwaysRelay);
        expect(restored.rdpPort, original.rdpPort);
        expect(restored.rdpUsername, original.rdpUsername);
        expect(restored.loginName, original.loginName);
        expect(restored.device_group_name, original.device_group_name);
        expect(restored.note, original.note);
        expect(restored.sameServer, original.sameServer);
      });

      test('forceAlwaysRelay is serialized as string', () {
        final p = _makePeer(forceAlwaysRelay: true);
        expect(p.toJson()['forceAlwaysRelay'], 'true');

        final p2 = _makePeer(forceAlwaysRelay: false);
        expect(p2.toJson()['forceAlwaysRelay'], 'false');
      });

      test('serializes to valid JSON string', () {
        final p = _makePeer();
        final jsonStr = jsonEncode(p.toJson());
        expect(() => jsonDecode(jsonStr), returnsNormally);
      });
    });

    group('toCustomJson', () {
      test('includes hash when includingHash is true', () {
        final p = _makePeer(hash: 'secret_hash');
        final json = p.toCustomJson(includingHash: true);
        expect(json['hash'], 'secret_hash');
        expect(json.containsKey('password'), false);
        expect(json.containsKey('forceAlwaysRelay'), false);
      });

      test('excludes hash when includingHash is false', () {
        final p = _makePeer(hash: 'secret_hash');
        final json = p.toCustomJson(includingHash: false);
        expect(json.containsKey('hash'), false);
      });

      test('includes only the expected keys', () {
        final p = _makePeer();
        final json = p.toCustomJson(includingHash: false);
        expect(json.keys.toSet(),
            {'id', 'username', 'hostname', 'platform', 'alias', 'tags'});
      });
    });

    group('toGroupCacheJson', () {
      test('includes correct keys for group cache', () {
        final p = _makePeer(
          id: 'g1',
          username: 'u',
          hostname: 'h',
          platform: 'Linux',
          loginName: 'ln',
          deviceGroupName: 'dg',
        );
        final json = p.toGroupCacheJson();
        expect(json['id'], 'g1');
        expect(json['username'], 'u');
        expect(json['hostname'], 'h');
        expect(json['platform'], 'Linux');
        expect(json['login_name'], 'ln');
        expect(json['device_group_name'], 'dg');
        // Should not contain hash, password, alias, tags, etc.
        expect(json.containsKey('hash'), false);
        expect(json.containsKey('alias'), false);
      });
    });

    group('equal()', () {
      test('returns true for identical peers', () {
        final a = _makePeer();
        final b = _makePeer();
        expect(a.equal(b), true);
      });

      test('returns false when id differs', () {
        final a = _makePeer(id: 'a');
        final b = _makePeer(id: 'b');
        expect(a.equal(b), false);
      });

      test('returns false when tags differ', () {
        final a = _makePeer(tags: ['x']);
        final b = _makePeer(tags: ['y']);
        expect(a.equal(b), false);
      });

      test('returns false when forceAlwaysRelay differs', () {
        final a = _makePeer(forceAlwaysRelay: false);
        final b = _makePeer(forceAlwaysRelay: true);
        expect(a.equal(b), false);
      });

      test('ignores online status', () {
        final a = _makePeer();
        final b = _makePeer();
        a.online = true;
        b.online = false;
        // equal() does not check online
        expect(a.equal(b), true);
      });

      test('ignores sameServer', () {
        final a = _makePeer(sameServer: true);
        final b = _makePeer(sameServer: false);
        // equal() does not compare sameServer
        expect(a.equal(b), true);
      });
    });

    group('Peer.copy()', () {
      test('creates an independent copy', () {
        final original = _makePeer(tags: ['a', 'b']);
        final copy = Peer.copy(original);

        expect(copy.id, original.id);
        expect(copy.tags, original.tags);

        // Mutating the copy's tags should not affect the original
        copy.tags.add('c');
        expect(original.tags, ['a', 'b']);
        expect(copy.tags, ['a', 'b', 'c']);
      });

      test('preserves sameServer', () {
        final original = _makePeer(sameServer: true);
        final copy = Peer.copy(original);
        expect(copy.sameServer, true);
      });
    });
  });
}
