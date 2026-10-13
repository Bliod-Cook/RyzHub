import 'dart:async';
import 'dart:convert';
import 'dart:typed_data';

import 'package:stream_channel/stream_channel.dart';
import 'package:web_socket_channel/web_socket_channel.dart';

import 'event_stream_native.dart'
    if (dart.library.js_interop) 'event_stream_web.dart'
    as platform;

/// The connection where WebSockets do not get through: frames from the server
/// as server-sent events (`GET /stream`), frames to it posted one at a time
/// (`POST /stream/{connection}`), in order. It reads and writes like a
/// WebSocket, so the connection logic is the same over either.
class EventStreamChannel extends StreamChannelMixin<dynamic>
    implements WebSocketChannel {
  EventStreamChannel(Uri uri, {required this.post}) {
    _events = platform
        .serverEvents(uri)
        .listen(
          _event,
          onError: (Object error) {
            if (!_ready.isCompleted) _ready.completeError(error);
            _incoming.addError(error);
            _finish();
          },
          onDone: _finish,
          cancelOnError: true,
        );
  }

  /// Posts a frame to the connection.
  final Future<void> Function(String connection, Uint8List frame) post;
  late final StreamSubscription<(String, String)> _events;
  final _incoming = StreamController<dynamic>();
  final _ready = Completer<void>();
  late final _sink = _EventStreamSink(this);

  /// Names the connection, from its first event.
  String? _connection;

  /// Posts go one after another, so frames arrive in the order sent.
  Future<void> _posting = Future.value();

  void _event((String, String) event) {
    final (kind, data) = event;
    if (kind == 'connection') {
      _connection = data;
      if (!_ready.isCompleted) _ready.complete();
      return;
    }
    _incoming.add(base64Decode(data));
  }

  void _send(Uint8List frame) {
    final connection = _connection;
    if (connection == null || _incoming.isClosed) return;
    _posting = _posting.then((_) => post(connection, frame)).catchError((
      Object error,
    ) {
      // A frame that did not get through ends the connection; the client
      // reconnects and resubscribes from its cursors.
      _incoming.addError(error);
      _finish();
    });
  }

  void _finish() {
    if (_incoming.isClosed) return;
    unawaited(_events.cancel());
    _incoming.close();
  }

  @override
  Stream<dynamic> get stream => _incoming.stream;
  @override
  WebSocketSink get sink => _sink;
  @override
  Future<void> get ready => _ready.future;
  @override
  String? get protocol => 'event-stream';
  @override
  int? get closeCode => null;
  @override
  String? get closeReason => null;
}

class _EventStreamSink implements WebSocketSink {
  _EventStreamSink(this.channel);
  final EventStreamChannel channel;

  @override
  void add(dynamic data) => channel._send(
    data is Uint8List ? data : Uint8List.fromList((data as List).cast<int>()),
  );
  @override
  void addError(Object error, [StackTrace? stackTrace]) {}
  @override
  Future<void> addStream(Stream<dynamic> stream) => stream.forEach(add);
  @override
  Future<void> close([int? closeCode, String? closeReason]) async =>
      channel._finish();
  @override
  Future<void> get done => channel._incoming.done;
}
