import 'dart:io';
import 'dart:typed_data';

import 'package:flutter/services.dart' show PlatformException;
import 'package:flutter_secure_storage/flutter_secure_storage.dart';
import 'package:http/http.dart' as http;
import 'package:path_provider/path_provider.dart';
import 'package:url_launcher/url_launcher.dart';

/// Native builds keep the device's refresh token in the platform's secure
/// storage (Android Keystore, Windows Credential Manager) and stay signed in.
http.Client sessionClient() => RefreshCookieClient(http.Client());

/// What the platform's secure storage holds for [RefreshCookieClient].
abstract class SecretStore {
  Future<String?> read(String key);
  Future<void> write(String key, String value);
  Future<void> delete(String key);
}

class _PlatformSecrets implements SecretStore {
  static const _storage = FlutterSecureStorage();

  @override
  Future<String?> read(String key) async {
    try {
      return await _storage.read(key: key);
    } on PlatformException {
      // Storage the platform cannot open (a reset keystore) holds nothing.
      return null;
    }
  }

  @override
  Future<void> write(String key, String value) async {
    try {
      await _storage.write(key: key, value: value);
    } on PlatformException {
      // A device whose storage refuses only signs in again next start.
    }
  }

  @override
  Future<void> delete(String key) async {
    try {
      await _storage.delete(key: key);
    } on PlatformException {
      // What it holds is refused with the next refresh.
    }
  }
}

/// Plays the browser's part for the refresh cookie: keeps what the server
/// sets, per server, and sends it back to the server's `/auth/` endpoints
/// only.
class RefreshCookieClient extends http.BaseClient {
  RefreshCookieClient(this._inner, [SecretStore? secrets])
    : _secrets = secrets ?? _PlatformSecrets();

  final http.Client _inner;
  final SecretStore _secrets;
  final _known = <String, String?>{};
  static const _cookie = 'ryzhub_refresh';

  String _key(Uri url) => 'refresh:${url.origin}';

  @override
  Future<http.StreamedResponse> send(http.BaseRequest request) async {
    final url = request.url;
    final auth = url.path.contains('/auth/');
    if (auth) {
      final key = _key(url);
      final value = _known.containsKey(key)
          ? _known[key]
          : _known[key] = await _secrets.read(key);
      if (value != null && value.isNotEmpty) {
        request.headers['cookie'] = '$_cookie=$value';
      }
    }
    final response = await _inner.send(request);
    final set = response.headers['set-cookie'];
    if (auth && set != null) await _remember(url, set);
    return response;
  }

  Future<void> _remember(Uri url, String header) async {
    for (final cookie in header.split(RegExp(r',(?=\s*\w+=)'))) {
      final parts = cookie.split(';').map((p) => p.trim()).toList();
      final pair = parts.first.split('=');
      if (pair.first != _cookie) continue;
      final value = pair.skip(1).join('=');
      final cleared =
          value.isEmpty ||
          parts.any((p) => p.toLowerCase().replaceAll(' ', '') == 'max-age=0');
      final key = _key(url);
      _known[key] = cleared ? null : value;
      if (cleared) {
        await _secrets.delete(key);
      } else {
        await _secrets.write(key, value);
      }
    }
  }

  @override
  void close() => _inner.close();
}

String sameSiteServer(String server) => server;

/// Saves [bytes] as [name] among the user's downloads and opens it.
Future<void> saveFile(String name, Uint8List bytes) async {
  final folder = await getDownloadsDirectory() ?? await getTemporaryDirectory();
  final file = File(
    '${folder.path}${Platform.pathSeparator}${name.replaceAll(RegExp(r'[/\\]'), '_')}',
  );
  await file.writeAsBytes(bytes);
  await launchUrl(Uri.file(file.path));
}

/// Files pasted into the window; native builds take files from the picker and
/// drops only. Returns the function that stops listening.
void Function() listenForPastedFiles(
  void Function(String name, Uint8List bytes) onFile,
) => () {};

/// Native builds hash with Dart; [open] is not read.
Future<String?> platformSha256(Stream<List<int>> Function() open) async => null;

/// Signs in with another account in the system browser, which returns to a loopback address
/// this app listens on. [start] builds the provider's start address for that return address;
/// the result is what came back: `login_code`, `login_linked` or `login_error`.
Future<Map<String, String>> signInElsewhere(
  String Function(String returnTo) start,
) async {
  final listener = await HttpServer.bind(InternetAddress.loopbackIPv4, 0);
  try {
    final returnTo = 'http://127.0.0.1:${listener.port}/signed-in';
    await launchUrl(
      Uri.parse(start(returnTo)),
      mode: LaunchMode.externalApplication,
    );
    final request = await listener.first.timeout(const Duration(minutes: 10));
    final result = request.uri.queryParameters;
    request.response
      ..headers.contentType = ContentType.html
      ..write(
        '<!doctype html><meta charset="utf-8"><title>RyzHub</title>'
        '<p style="font-family:sans-serif;margin:3em">${result.containsKey('login_error') ? '登录没有完成，请回到 RyzHub 重试。' : '已完成，可以回到 RyzHub 了。'}</p>',
      );
    await request.response.close();
    return result;
  } finally {
    await listener.close(force: true);
  }
}

/// What a sign-in elsewhere left in this page's address; native builds have none.
Map<String, String> takeSignInResult() => const {};
