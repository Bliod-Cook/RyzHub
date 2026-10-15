import 'dart:async';
import 'dart:collection';
import 'dart:convert';
import 'dart:math';
import 'dart:typed_data';

import 'package:crypto/crypto.dart';
import 'package:http/http.dart' as http;

import 'browser.dart';

const defaultServerUrl = String.fromEnvironment(
  'RYZHUB_SERVER_URL',
  defaultValue: 'http://127.0.0.1:8080',
);

/// What [contentHash] hashes at once before letting frames through.
const _hashSlice = 1024 * 1024;

/// The SHA-256 of the stream [open] gives, as lowercase hex: how the server
/// names content.
Future<String> contentHash(Stream<List<int>> Function() open) async {
  final hex = await platformSha256(open);
  if (hex != null) return hex;
  late Digest digest;
  final sink = sha256.startChunkedConversion(
    ChunkedConversionSink<Digest>.withCallback((d) => digest = d.single),
  );
  await for (final chunk in open()) {
    if (chunk.length <= _hashSlice) {
      sink.add(chunk);
      continue;
    }
    // A browser hands over a whole file as one chunk: hashed at once, it
    // would hold the page for seconds.
    final bytes = chunk is Uint8List ? chunk : Uint8List.fromList(chunk);
    for (var at = 0; at < bytes.length; at += _hashSlice) {
      sink.add(
        Uint8List.sublistView(bytes, at, min(at + _hashSlice, bytes.length)),
      );
      await Future<void>.delayed(Duration.zero);
    }
  }
  sink.close();
  return '$digest';
}

/// [source] in chunks of exactly [size] bytes, the last one shorter.
Stream<Uint8List> byteChunks(Stream<List<int>> source, int size) async* {
  final buffer = BytesBuilder(copy: false);
  await for (final data in source) {
    var offset = 0;
    while (offset < data.length) {
      final room = size - buffer.length, left = data.length - offset;
      final take = room < left ? room : left;
      buffer.add(data.sublist(offset, offset + take));
      offset += take;
      if (buffer.length == size) yield buffer.takeBytes();
    }
  }
  if (buffer.length > 0) yield buffer.takeBytes();
}

/// The server's HTTP API as the signed-in member: requests, uploads and
/// files. A request whose access token expired renews it once.
mixin HubApi {
  String base = defaultServerUrl;
  String token = '', org = '';

  /// What the server offers, from `GET /capabilities`.
  Map<String, dynamic> capabilities = {};

  /// A new access token for requests that found theirs expired; false when
  /// the device is signed out.
  Future<bool> renew();

  final http.Client _http = sessionClient();

  /// Closes the connections requests keep open.
  void closeApi() => _http.close();

  /// Requests that exchange credentials themselves; any other request whose
  /// access token expired renews it once.
  static const _exchanges = {
    '/auth/login',
    '/auth/register',
    '/auth/oauth/exchange',
    '/auth/refresh',
    '/auth/logout',
  };

  Future<dynamic> request(
    String path, {
    Map<String, dynamic>? body,
    String? method,
    Duration timeout = const Duration(seconds: 15),
  }) async {
    final response = _accepted(
      await _authorized(
        () => _send(path, body, method, timeout),
        renews: !_exchanges.contains(path),
      ),
    );
    return response.body.isEmpty ? null : jsonDecode(response.body);
  }

  Future<http.Response> _send(
    String path,
    Map<String, dynamic>? body,
    String? method,
    Duration timeout,
  ) {
    final headers = {
      'Content-Type': 'application/json',
      if (token.isNotEmpty) 'Authorization': 'Bearer $token',
    };
    final url = Uri.parse('$base$path');
    return (method == 'PATCH'
            ? _http.patch(url, headers: headers, body: jsonEncode(body))
            : method == 'PUT'
            ? _http.put(url, headers: headers, body: jsonEncode(body))
            : method == 'DELETE'
            ? _http.delete(url, headers: headers)
            : body == null
            ? _http.get(url, headers: headers)
            : _http.post(url, headers: headers, body: jsonEncode(body)))
        .timeout(timeout);
  }

  /// The response to [send], sent once more under a renewed access token if
  /// the server found the token expired.
  Future<http.Response> _authorized(
    Future<http.Response> Function() send, {
    bool renews = true,
  }) async {
    final response = await send();
    if (response.statusCode == 401 &&
        renews &&
        token.isNotEmpty &&
        await renew()) {
      return send();
    }
    return response;
  }

  /// [response], unless the server refused: then its error code, and the
  /// server's explanation when it gave one.
  http.Response _accepted(http.Response response) {
    if (response.statusCode < 400) return response;
    Object? data;
    try {
      data = jsonDecode(response.body);
    } on FormatException {
      // Not the server's answer, e.g. a proxy's error page.
    }
    final code = data is Map ? data['error'] : null;
    final message = data is Map ? data['message'] : null;
    throw Exception(
      message is String ? '$code\n$message' : code ?? response.statusCode,
    );
  }

  /// Posts [frame] to event-stream connection [connection].
  Future<void> postFrame(String connection, Uint8List frame) async {
    final response = await _authorized(
      () => _http
          .post(
            Uri.parse('$base/stream/$connection'),
            headers: {
              'Authorization': 'Bearer $token',
              'Content-Type': 'application/octet-stream',
            },
            body: frame,
          )
          .timeout(const Duration(seconds: 15)),
    );
    if (response.statusCode >= 400) {
      throw http.ClientException('frame not taken: ${response.statusCode}');
    }
  }

  /// A one-time ticket that opens a socket as the member.
  Future<String> ticket() async =>
      (await request('/auth/ticket', body: {}))['ticket'] as String;

  /// Uploads in parts that a failure interrupted, by name and size: trying the
  /// same file again resumes them.
  final _resumable = <String, String>{};

  /// Upload [size] bytes from [open] as [name] to the member's library, or to
  /// [to] (a path that takes the same request). Library files larger than the
  /// server's part size go up in parts. [open] is called again if the access
  /// token has to be renewed first, or to resume an upload in parts.
  Future<Map<String, dynamic>> uploadFile(
    String name,
    int size,
    Stream<List<int>> Function() open, {
    String? to,
  }) async {
    final part = (capabilities['attachments']['part_bytes'] as num).toInt();
    if (to == null && size > part) return _uploadInParts(name, size, open);
    final query = Uri(queryParameters: {'name': name}).query;
    final response = _accepted(
      await _authorized(() async {
        final request = http.StreamedRequest(
          'POST',
          Uri.parse('$base${to ?? '/orgs/$org/attachments'}?$query'),
        );
        request.headers['Authorization'] = 'Bearer $token';
        request.headers['Content-Type'] = 'application/octet-stream';
        request.contentLength = size;
        open().listen(
          request.sink.add,
          onDone: request.sink.close,
          onError: request.sink.addError,
          cancelOnError: true,
        );
        return http.Response.fromStream(await _http.send(request));
      }),
    );
    return Map<String, dynamic>.from(jsonDecode(response.body));
  }

  /// An upload through `/uploads`: each part is sent, retried and, after a
  /// failure, resumed on its own; the server then puts the file together.
  Future<Map<String, dynamic>> _uploadInParts(
    String name,
    int size,
    Stream<List<int>> Function() open,
  ) async {
    final key = '$size/$name';
    Map<String, dynamic>? upload;
    final known = _resumable[key];
    if (known != null) {
      try {
        upload = Map<String, dynamic>.from(
          await request('/orgs/$org/uploads/$known'),
        );
        if (upload['state'] != 'open') upload = null;
      } catch (_) {
        upload = null;
      }
    }
    upload ??= Map<String, dynamic>.from(
      await request('/orgs/$org/uploads', body: {'name': name, 'size': size}),
    );
    final id = upload['id'] as String;
    _resumable[key] = id;
    final received = {
      for (final n in upload['received'] as List) (n as num).toInt(),
    };
    final urls = {
      for (final u in upload['urls'] as List)
        (u['n'] as num).toInt(): u['url'] as String,
    };
    var n = 0;
    await for (final chunk in byteChunks(
      open(),
      (upload['part_size'] as num).toInt(),
    )) {
      n++;
      if (received.contains(n)) continue;
      final url = urls[n];
      await _retrying(
        () => url != null
            ? _sendDirect(id, n, url, chunk)
            : _sendPart(id, n, chunk),
      );
    }
    final file = await request(
      '/orgs/$org/uploads/$id/complete',
      body: {},
      timeout: const Duration(minutes: 2),
    );
    _resumable.remove(key);
    return Map<String, dynamic>.from(file);
  }

  Future<void> _retrying(Future<void> Function() send) async {
    for (var attempt = 1; ; attempt++) {
      try {
        return await send();
      } catch (_) {
        if (attempt == 3) rethrow;
        await Future.delayed(Duration(seconds: attempt));
      }
    }
  }

  Future<void> _sendPart(String id, int n, Uint8List bytes) async {
    _accepted(
      await _authorized(
        () => _http.put(
          Uri.parse('$base/orgs/$org/uploads/$id/parts/$n'),
          headers: {
            'Authorization': 'Bearer $token',
            'Content-Type': 'application/octet-stream',
          },
          body: bytes,
        ),
      ),
    );
  }

  /// A part straight to the bucket. Browsers read the entity tag only if the
  /// bucket's CORS rules expose `ETag`.
  Future<void> _sendDirect(
    String id,
    int n,
    String url,
    Uint8List bytes,
  ) async {
    final response = await _http.put(Uri.parse(url), body: bytes);
    final etag = response.headers['etag'];
    if (response.statusCode >= 400 || etag == null) {
      throw Exception('UPLOAD_PART_FAILED');
    }
    await request('/orgs/$org/uploads/$id/parts/$n/sent', body: {'etag': etag});
  }

  /// A file's bytes from [path], e.g. its preview or content.
  Future<Uint8List> fileBytes(String path) async => _accepted(
    await _authorized(
      () => _http.get(
        Uri.parse('$base$path'),
        headers: {'Authorization': 'Bearer $token'},
      ),
    ),
  ).bodyBytes;

  final LinkedHashMap<String, Future<Uint8List>> _previews = LinkedHashMap();

  /// An image file as models see it, cached for the timeline.
  Future<Uint8List> preview(String id) {
    final cached = _previews.remove(id);
    final future = cached ?? fileBytes('/orgs/$org/attachments/$id/preview');
    _previews[id] = future;
    while (_previews.length > 64) {
      _previews.remove(_previews.keys.first);
    }
    // A failure is not remembered: the next look tries again.
    future.catchError((_) {
      _previews.remove(id);
      return Uint8List(0);
    });
    return future;
  }

  Future<Uint8List> fileContent(String id) =>
      fileBytes('/orgs/$org/attachments/$id/content');

  /// Files as uploaded that were recently previewed, newest last.
  final LinkedHashMap<String, Uint8List> _originals = LinkedHashMap();

  /// What [_originals] holds at most, in bytes; a larger file is not kept.
  static const _originalsBudget = 32 * 1024 * 1024;

  /// A file as uploaded, for the preview: stepping back to a file shows it
  /// again without fetching it again.
  Future<Uint8List> original(String id) async {
    final cached = _originals.remove(id);
    if (cached != null) return _originals[id] = cached;
    final bytes = await fileContent(id);
    if (bytes.length > _originalsBudget) return bytes;
    _originals[id] = bytes;
    var held = _originals.values.fold(0, (sum, b) => sum + b.length);
    while (held > _originalsBudget) {
      held -= _originals.remove(_originals.keys.first)!.length;
    }
    return bytes;
  }

  /// The member's files, newest first: those whose name contains [query] and,
  /// given [kinds], only of those kinds; given [direction], only uploads
  /// (`upload`) or files models handed over (`output`).
  Future<List<Map<String, dynamic>>> library({
    String? before,
    String? query,
    List<String>? kinds,
    String? direction,
  }) async {
    final path = Uri(
      path: '/orgs/$org/attachments',
      queryParameters: {
        'before': ?before,
        if (query != null && query.trim().isNotEmpty) 'q': query.trim(),
        if (kinds != null) 'kind': kinds.join(','),
        'direction': ?direction,
      },
    ).toString();
    final data = await request(path);
    return [
      for (final file in data['files'] as List)
        Map<String, dynamic>.from(file as Map),
    ];
  }

  /// The member's newest file with the content [sha256] names, if any.
  Future<Map<String, dynamic>?> fileWithHash(String sha256) async {
    final data = await request('/orgs/$org/attachments?sha256=$sha256&limit=1');
    final files = data['files'] as List;
    return files.isEmpty ? null : Map<String, dynamic>.from(files.first as Map);
  }

  /// The member's file with the same content as the one [open] reads, if
  /// any. A check the server or the network fails finds none: the file is
  /// uploaded as before.
  Future<Map<String, dynamic>?> sameFile(
    Stream<List<int>> Function() open,
  ) async {
    try {
      return await fileWithHash(await contentHash(open));
    } on Exception {
      return null;
    }
  }

  /// Delete a file everywhere: every conversation that carried it loses it.
  Future<void> deleteFile(String id) async {
    await request('/orgs/$org/attachments/$id', method: 'DELETE');
    _previews.remove(id);
    _originals.remove(id);
  }

  /// Messages of the member's conversations that match [query].
  Future<dynamic> search(String query) =>
      request('/orgs/$org/search?q=${Uri.encodeQueryComponent(query)}');

  /// The member's memories, of [project] when given, sorted by [sort] and
  /// matching [query], with the member's memory preferences.
  Future<dynamic> memories({
    String? project,
    required String sort,
    String query = '',
  }) => request(
    Uri(
      path: '/orgs/$org/memories',
      queryParameters: {
        'project': ?project,
        'sort': sort,
        if (query.isNotEmpty) 'q': query,
      },
    ).toString(),
  );

  /// Remembers [content], in place of memory [replace] when given.
  Future<void> saveMemory({
    required String content,
    String? project,
    String? replace,
    num priority = 0,
  }) => request(
    '/orgs/$org/memories',
    body: {
      'content': content,
      'project_id': project,
      'replace': ?replace,
      'priority': priority,
    },
  );

  Future<void> setMemoryPreferences(Map<String, dynamic> preferences) =>
      request(
        '/orgs/$org/memories/preferences',
        method: 'PATCH',
        body: preferences,
      );

  /// Forgets memory [id]; the answer names the change, to undo it.
  Future<dynamic> deleteMemory(String id) =>
      request('/orgs/$org/memories/$id', method: 'DELETE');

  /// The knowledge files of project [id].
  Future<dynamic> projectFiles(String id) =>
      request('/orgs/$org/projects/$id/files');

  Future<dynamic> createProject(Map<String, dynamic> project) =>
      request('/orgs/$org/projects', body: project);

  Future<void> updateProject(String id, Map<String, dynamic> project) =>
      request('/orgs/$org/projects/$id', body: project, method: 'PATCH');

  /// Brings back project [id] while its deletion is pending.
  Future<void> restoreProject(String id) =>
      request('/orgs/$org/projects/$id/restore', body: {});

  Future<void> addProjectFile(String project, String name, String content) =>
      request(
        '/orgs/$org/projects/$project/files',
        body: {'name': name, 'content': content},
      );

  Future<void> deleteProjectFile(String project, String file) =>
      request('/orgs/$org/projects/$project/files/$file', method: 'DELETE');

  /// The organization's plugins, and whether plugins are on.
  Future<dynamic> plugins() => request('/orgs/$org/plugins');

  /// Changes plugin [id]: whether it is on, or its settings and secrets.
  Future<void> updatePlugin(String id, Map<String, dynamic> change) =>
      request('/orgs/$org/plugins/$id', method: 'PATCH', body: change);

  /// Accepts what changed in plugin [id]'s tools.
  Future<void> confirmPlugin(String id) =>
      request('/orgs/$org/plugins/$id/confirm', body: {});

  /// Starts signing the member in to [server] of plugin [id]: the answer
  /// names the page that does it.
  Future<dynamic> connectPlugin(String id, String server) => request(
    '/orgs/$org/plugins/$id/connect',
    body: {'server': server},
    timeout: const Duration(seconds: 60),
  );

  /// Signs the member out of [server] of plugin [id].
  Future<void> disconnectPlugin(String id, String server) => request(
    '/orgs/$org/plugins/$id/connections/${Uri.encodeComponent(server)}',
    method: 'DELETE',
  );

  /// Reads plugin [id]'s tools again.
  Future<void> refreshPlugin(String id) => request(
    '/orgs/$org/plugins/$id/refresh',
    body: {},
    timeout: const Duration(seconds: 60),
  );

  /// Installs the newest version of plugin [id] its source offers.
  Future<void> upgradePlugin(String id) => request(
    '/orgs/$org/plugins/$id/update',
    body: {},
    timeout: const Duration(seconds: 120),
  );

  Future<void> uninstallPlugin(String id) =>
      request('/orgs/$org/plugins/$id', method: 'DELETE');

  /// Plugins the instance offers to install.
  Future<dynamic> pluginCatalog() => request(
    '/orgs/$org/plugins/catalog',
    timeout: const Duration(seconds: 30),
  );

  /// What installing the plugin at [source] would add, from its [archive]
  /// when uploaded, before it is trusted.
  Future<dynamic> previewPlugin(
    Map<String, dynamic> source, {
    String? archive,
  }) => request(
    '/orgs/$org/plugins/preview',
    body: {'source': source, 'archive': ?archive},
    timeout: const Duration(minutes: 2),
  );

  /// Installs a previewed plugin version with its settings and secrets.
  Future<void> installPlugin(
    String version,
    Map<String, dynamic> config,
    Map<String, dynamic> secrets,
  ) => request(
    '/orgs/$org/plugins',
    body: {'version_id': version, 'config': config, 'secrets': secrets},
    timeout: const Duration(minutes: 2),
  );

  /// The plugins of conversation [conv], each on or off there.
  Future<dynamic> conversationPlugins(String conv) =>
      request('/orgs/$org/conversations/$conv/plugins');

  /// Turns [plugin] on or off in conversation [conv]; the answer is its
  /// plugins as [conversationPlugins] gives them.
  Future<dynamic> setConversationPlugin(
    String conv,
    String plugin,
    bool enabled,
  ) => request(
    '/orgs/$org/conversations/$conv/plugins/$plugin',
    method: 'PUT',
    body: {'enabled': enabled},
  );

  /// The member's other ways to sign in, and the providers that offer one.
  Future<dynamic> identities() => request('/me/identities');

  Future<void> removeIdentity(String provider, String subject) => request(
    '/me/identities/${Uri.encodeComponent(provider)}/${Uri.encodeComponent(subject)}',
    method: 'DELETE',
  );

  Future<List<dynamic>> usage() async =>
      await request('/orgs/$org/usage') as List;

  /// The member's model credentials and the organization's.
  Future<List<dynamic>> credentials() async =>
      await request('/orgs/$org/credentials') as List;

  /// Adds a credential for [provider]: the organization's when
  /// [organization], else the member's.
  Future<void> addCredential({
    required String name,
    required String apiKey,
    required String provider,
    required bool organization,
  }) => request(
    '/orgs/$org/credentials',
    body: {
      'name': name,
      'api_key': apiKey,
      'provider': provider,
      'organization': organization,
    },
  );

  Future<void> revokeCredential(String id) =>
      request('/orgs/$org/credentials/$id', method: 'DELETE');

  /// The organization's policy.
  Future<dynamic> policy() => request('/orgs/$org/policy');
}
