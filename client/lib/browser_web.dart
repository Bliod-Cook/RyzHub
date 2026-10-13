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

/// The SHA-256 of the stream [open] gives, as lowercase hex, worked out by the
/// browser off the page's thread; null where the page is not a secure context
/// and the browser offers none.
Future<String?> platformSha256(Stream<List<int>> Function() open) async {
  if (!web.window.isSecureContext) return null;
  final bytes = BytesBuilder(copy: false);
  await for (final chunk in open()) {
    bytes.add(chunk);
  }
  final digest = await web.window.crypto.subtle
      .digest('SHA-256'.toJS, bytes.takeBytes().toJS)
      .toDart;
  return [
    for (final byte in (digest as JSArrayBuffer).toDart.asUint8List())
      byte.toRadixString(16).padLeft(2, '0'),
  ].join();
}

/// Signs in with another account: the page goes to the provider and comes back to itself,
/// with the result in its address (see [takeSignInResult]). Returns nothing here.
Future<Map<String, String>> signInElsewhere(
  String Function(String returnTo) start,
) async {
  final location = web.window.location;
  web.window.location.assign(start('${location.origin}${location.pathname}'));
  return const {};
}

/// The result of a sign-in elsewhere that this page's address carries, taken out of the
/// address so a reload does not use it again.
Map<String, String> takeSignInResult() {
  final uri = Uri.parse(web.window.location.href);
  const keys = {'login_code', 'login_linked', 'login_error'};
  final result = {
    for (final MapEntry(:key, :value) in uri.queryParameters.entries)
      if (keys.contains(key)) key: value,
  };
  if (result.isNotEmpty) {
    final rest = Map.of(uri.queryParameters)
      ..removeWhere((k, _) => keys.contains(k));
    final clean = uri.replace(queryParameters: rest.isEmpty ? null : rest);
    web.window.history.replaceState(
      null,
      '',
      clean.toString().replaceAll('?#', '#'),
    );
  }
  return result;
}
