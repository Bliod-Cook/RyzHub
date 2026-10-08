import 'dart:async';
import 'dart:collection';

import 'package:flutter/foundation.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:ryzhub/controller.dart';
import 'package:ryzhub/rust/api.dart';
import 'package:ryzhub/rust/frb_generated.dart';
import 'package:ryzhub/storage/cache.dart';
import 'package:web_socket_channel/web_socket_channel.dart';

class _Api implements RustLibApi {
  @override
  int crateApiReconnectDelay({required int attempt, required int random}) =>
      1000;
  @override
  dynamic noSuchMethod(Invocation invocation) => super.noSuchMethod(invocation);
}

Update _update({
  List<Uint8List> frames = const [],
  bool save = false,
  bool auth = false,
}) => Update(
  changesJson: '[]',
  resultsJson: '[]',
  authExpiring: auth,
  cacheWrites: save ? [const CacheWrite(key: 'pending', value: 'command')] : [],
  outboundFrames: frames,
);

class _Core implements ClientCore {
  bool stored = false;
  bool disposed = false;
  @override
  Update cancel({required String channel}) => _update(
    frames: [
      Uint8List.fromList([42]),
    ],
    save: true,
  );
  @override
  void committed() {
    expect(disposed, isFalse);
    stored = true;
  }

  @override
  List<Uint8List> retryPending() => stored
      ? [
          Uint8List.fromList([42]),
        ]
      : [];
  @override
  Uint8List hello({required String deviceId}) => Uint8List.fromList([0]);
  @override
  Uint8List authRefresh({required String token}) => Uint8List.fromList([3]);
  @override
  Update ingest({required List<Uint8List> frames}) {
    expect(disposed, isFalse);
    // Incoming ping, welcome and auth-expiring events, respectively.
    return _update(
      frames: [
        for (final frame in frames)
          ...switch (frame.first) {
            1 => [
              Uint8List.fromList([2]),
            ],
            7 => retryPending(),
            _ => <Uint8List>[],
          },
      ],
      auth: frames.any((f) => f.first == 9),
    );
  }

  @override
  void dispose() => disposed = true;
  @override
  dynamic noSuchMethod(Invocation invocation) => super.noSuchMethod(invocation);
}

class _Cache implements LocalCache {
  Completer<void>? writeGate;
  final writing = Completer<void>();
  final records = <String, String?>{};
  @override
  Future<void> write(Map<String, String?> values) async {
    if (!writing.isCompleted) writing.complete();
    await writeGate?.future;
    records.addAll(values);
  }

  @override
  Future<void> close() async {}
  @override
  dynamic noSuchMethod(Invocation invocation) => super.noSuchMethod(invocation);
}

class _Sink implements WebSocketSink {
  final outgoing = StreamController<dynamic>(sync: true);
  final closing = Completer<void>();
  final sent = <List<int>>[];
  Completer<void>? closeGate;
  _Sink() {
    outgoing.stream.listen((data) => sent.add(List<int>.from(data as List)));
  }
  @override
  void add(dynamic data) => outgoing.add(data);
  @override
  Future<void> close([int? closeCode, String? closeReason]) async {
    final closed = outgoing.close();
    if (!closing.isCompleted) closing.complete();
    await closeGate?.future;
    await closed;
  }

  @override
  Future<dynamic> get done => outgoing.done;
  @override
  dynamic noSuchMethod(Invocation invocation) => super.noSuchMethod(invocation);
}

class _Socket implements WebSocketChannel {
  final incoming = StreamController<dynamic>(sync: true);
  final opened = Completer<void>();
  Completer<void>? readyGate;
  @override
  final _Sink sink = _Sink();
  @override
  Stream<dynamic> get stream => incoming.stream;
  @override
  Future<void> get ready => readyGate?.future ?? Future.value();
  @override
  int? get closeCode => null;
  @override
  String? get closeReason => null;
  @override
  dynamic noSuchMethod(Invocation invocation) => super.noSuchMethod(invocation);
  void receive(int byte) => incoming.add(Uint8List.fromList([byte]));
}

class _Hub extends HubController {
  final tickets = Queue<Completer<dynamic>>();
  final refresh = Completer<dynamic>();
  int ticketRequests = 0, refreshRequests = 0;
  _Hub(_Core core, _Cache cache, Queue<_Socket> ready)
    : super.testing(
        core: core,
        cache: cache,
        connect: (_) {
          final socket = ready.removeFirst();
          socket.opened.complete();
          return socket;
        },
      ) {
    token = 'token';
    user = 'user';
    org = 'org';
    selected = 'chat';
  }
  @override
  Future<dynamic> request(
    String path, {
    Map<String, dynamic>? body,
    String? method,
  }) async {
    if (path == '/auth/ticket') {
      ticketRequests++;
      return tickets.isEmpty
          ? {'ticket': 'ticket'}
          : tickets.removeFirst().future;
    }
    if (path == '/auth/refresh') {
      refreshRequests++;
      return refresh.future;
    }
    if (path == '/auth/logout') return {};
    throw StateError('Unexpected request $path');
  }
}

void main() {
  setUpAll(() => RustLib.initMock(api: _Api()));
  tearDownAll(RustLib.dispose);

  late _Core core;
  late _Cache cache;
  late _Socket first, second;
  late Queue<_Socket> sockets;
  late _Hub hub;
  setUp(() {
    core = _Core();
    cache = _Cache();
    first = _Socket();
    second = _Socket();
    sockets = Queue.of([first, second]);
    hub = _Hub(core, cache, sockets);
  });
  tearDown(() => hub.dispose());

  test(
    'reconnect drains pending replies without writing to the closed socket',
    () async {
      await hub.reconnect();
      first.receive(1);
      await hub.reconnect();
      expect(hub.error, isEmpty);
      expect(first.sink.sent, [
        [0],
      ]);
      expect(second.sink.sent, [
        [0],
      ]);
      expect(hub.connection, 'online');
    },
  );

  test('a command being persisted survives reconnect and replays on the new socket', () async {
    await hub.reconnect();
    cache.writeGate = Completer<void>();
    final command = hub.cancel();
    final commandCheck = expectLater(command, completes);
    await cache.writing.future;
    final reconnecting = hub.reconnect();
    await first.sink.closing.future;
    cache.writeGate!.complete();
    await commandCheck;
    await reconnecting;
    expect(hub.error, isEmpty);
    expect(cache.records['pending'], 'command');
    expect(core.stored, isTrue);
    expect(first.sink.sent, [
      [0],
    ]);
    second.receive(7);
    // Let the normal frame batch process the new connection's welcome.
    await Future<void>.delayed(const Duration(milliseconds: 50));
    expect(second.sink.sent, [
      [0],
      [42],
    ]);
  });

  test('returning focus keeps a healthy socket', () async {
    await hub.reconnect();
    await hub.resume();
    await hub.resume();
    expect(hub.ticketRequests, 1);
    expect(first.sink.closing.isCompleted, isFalse);
    expect(hub.connection, 'online');
  });

  test(
    'suspend can finish after resume without closing the new socket',
    () async {
      await hub.reconnect();
      first.sink.closeGate = Completer<void>();
      first.receive(1);
      final suspending = hub.suspend();
      await first.sink.closing.future;
      expect(hub.connection, 'background');
      await hub.resume();
      first.sink.closeGate!.complete();
      await suspending;
      expect(hub.connection, 'online');
      expect(second.sink.closing.isCompleted, isFalse);
      expect(hub.error, isEmpty);
      second.receive(1);
      await Future<void>.delayed(const Duration(milliseconds: 50));
      expect(second.sink.sent, [
        [0],
        [2],
      ]);
    },
  );

  test('a superseded handshake cannot replace or report errors on the new connection', () async {
    await hub.reconnect();
    second.readyGate = Completer<void>();
    final reconnecting = hub.reconnect();
    await second.opened.future;
    await hub.suspend();
    final third = _Socket();
    sockets.add(third);
    await hub.resume();
    second.readyGate!.completeError(
      WebSocketChannelException('old handshake failed'),
    );
    await reconnecting;
    expect(hub.connection, 'online');
    expect(hub.error, isEmpty);
    expect(third.sink.sent, [
      [0],
    ]);
    await hub.resume();
    expect(hub.ticketRequests, 3);
  });

  test(
    'a superseded ticket failure cannot sign out the resumed connection',
    () async {
      await hub.reconnect();
      final ticket = Completer<dynamic>();
      hub.tickets.add(ticket);
      final reconnecting = hub.reconnect();
      await Future<void>.delayed(Duration.zero);
      expect(hub.ticketRequests, 2);
      await hub.suspend();
      await hub.resume();
      ticket.completeError(Exception('UNAUTHORIZED'));
      await reconnecting;
      expect(hub.sessionEnded, isFalse);
      expect(hub.connection, 'online');
      expect(hub.error, isEmpty);
    },
  );

  test(
    'a ticket arriving after logout does not create another socket',
    () async {
      await hub.reconnect();
      final ticket = Completer<dynamic>();
      hub.tickets.add(ticket);
      final reconnecting = hub.reconnect();
      await Future<void>.delayed(Duration.zero);
      await hub.logout();
      ticket.complete({'ticket': 'obsolete'});
      await reconnecting;
      expect(second.opened.isCompleted, isFalse);
      expect(hub.error, isEmpty);
    },
  );

  test(
    'logout while persisting a command never sends it on the closing socket',
    () async {
      await hub.reconnect();
      cache.writeGate = Completer<void>();
      final command = hub.cancel();
      await cache.writing.future;
      final loggingOut = hub.logout();
      await first.sink.closing.future;
      cache.writeGate!.complete();
      await command;
      await loggingOut;
      expect(cache.records['pending'], 'command');
      expect(first.sink.sent, [
        [0],
      ]);
      expect(hub.error, isEmpty);
    },
  );

  test('remote close can reconnect on focus', () async {
    await hub.reconnect();
    await first.incoming.close();
    expect(hub.connection, 'retrying');
    await hub.resume();
    expect(hub.connection, 'online');
    expect(second.sink.sent, [
      [0],
    ]);
  });

  test(
    'a refresh finishing after reconnect cannot write to either generation',
    () async {
      await hub.reconnect();
      first.receive(9);
      await Future<void>.delayed(const Duration(milliseconds: 50));
      expect(hub.refreshRequests, 1);
      await hub.reconnect();
      hub.refresh.complete({
        'user_id': 'user',
        'token': 'renewed',
        'expires_at_ms': 123,
      });
      await Future<void>.delayed(Duration.zero);
      expect(first.sink.sent, [
        [0],
      ]);
      expect(second.sink.sent, [
        [0],
      ]);
      expect(hub.error, isEmpty);
    },
    skip: !kIsWeb,
  );

  test('a refresh on the current socket is still sent', () async {
    await hub.reconnect();
    first.receive(9);
    await Future<void>.delayed(const Duration(milliseconds: 50));
    hub.refresh.complete({
      'user_id': 'user',
      'token': 'renewed',
      'expires_at_ms': 123,
    });
    await Future<void>.delayed(Duration.zero);
    expect(first.sink.sent, [
      [0],
      [3],
    ]);
    expect(hub.error, isEmpty);
  }, skip: !kIsWeb);
}
