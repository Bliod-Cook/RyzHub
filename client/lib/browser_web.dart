import 'dart:js_interop';
import 'dart:typed_data';

import 'package:http/browser_client.dart';
import 'package:http/http.dart' as http;
import 'package:web/web.dart' as web;

/// The browser holds the device's refresh token in an HttpOnly cookie.
const keepsSession = true;

/// Requests carry the cookie to the server, which is another origin.
http.Client sessionClient() => BrowserClient()..withCredentials = true;

const _loopback = {'localhost', '127.0.0.1', '[::1]', '::1'};

/// [server] reached through the page's own loopback name, so the refresh
/// cookie stays same-site: a page on `localhost` calling `127.0.0.1` is
/// cross-site, and browsers withhold its cookies.
String sameSiteServer(String server) {
  final uri = Uri.tryParse(server);
  final page = web.window.location.hostname;
  if (uri == null ||
      uri.host == page ||
      !_loopback.contains(uri.host) ||
      !_loopback.contains(page)) {
    return server;
  }
  return uri.replace(host: page).toString().replaceAll(RegExp(r'/+$'), '');
}

/// Hands [bytes] to the browser as a download named [name].
Future<void> saveFile(String name, Uint8List bytes) async {
  final url = web.URL.createObjectURL(web.Blob([bytes.toJS].toJS));
  (web.HTMLAnchorElement()
        ..href = url
        ..download = name)
      .click();
  // The download has its own reference once it starts.
  Future.delayed(
    const Duration(minutes: 1),
    () => web.URL.revokeObjectURL(url),
  );
}

/// Files pasted anywhere on the page, e.g. a screenshot. A paste that also
/// carries text stays an ordinary text paste. Returns the function that stops
/// listening.
void Function() listenForPastedFiles(
  void Function(String name, Uint8List bytes) onFile,
) {
  final listener = ((web.ClipboardEvent event) {
    final data = event.clipboardData;
    final files = data?.files;
    if (data == null ||
        files == null ||
        files.length == 0 ||
        data.getData('text/plain').isNotEmpty) {
      return;
    }
    event.preventDefault();
    for (var i = 0; i < files.length; i++) {
      final file = files.item(i);
      if (file == null) continue;
      file.arrayBuffer().toDart.then(
        (buffer) => onFile(file.name, buffer.toDart.asUint8List()),
      );
    }
  }).toJS;
  web.document.addEventListener('paste', listener);
  return () => web.document.removeEventListener('paste', listener);
}
