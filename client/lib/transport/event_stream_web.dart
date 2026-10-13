import 'dart:async';
import 'dart:js_interop';

import 'package:http/http.dart' as http;
import 'package:web/web.dart' as web;

/// The events `GET uri` streams, as (event, data). The browser's EventSource
/// would reconnect by itself; a reconnect is the client's to make, with a new
/// ticket and its cursors.
Stream<(String, String)> serverEvents(Uri uri) {
  late final web.EventSource source;
  final events = StreamController<(String, String)>(
    onCancel: () => source.close(),
  );
  void listen(String kind) => source.addEventListener(
    kind,
    (web.MessageEvent event) {
      events.add((kind, (event.data as JSString).toDart));
    }.toJS,
  );
  source = web.EventSource(uri.toString());
  listen('message');
  listen('connection');
  source.onerror = (web.Event _) {
    source.close();
    if (events.isClosed) return;
    events.addError(http.ClientException('event stream closed', uri));
    events.close();
  }.toJS;
  return events.stream;
}
