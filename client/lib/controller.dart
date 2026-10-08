import 'dart:async';
import 'dart:collection';
import 'dart:convert';
import 'dart:math';

import 'package:flutter/foundation.dart';
import 'package:http/http.dart' as http;
import 'package:connectivity_plus/connectivity_plus.dart';
import 'package:web_socket_channel/web_socket_channel.dart';

import 'browser.dart';
import 'l10n.dart';
import 'prefs.dart';
import 'rust/api.dart';
import 'storage/cache.dart';

const defaultServerUrl = String.fromEnvironment(
  'RYZHUB_SERVER_URL',
  defaultValue: 'http://127.0.0.1:8080',
);

/// A readable title from the first message; fences and blank lines are skipped.
String conversationTitle(String text) {
  for (final raw in text.split('\n')) {
    final line = raw.trim().replaceAll(RegExp(r'\s+'), ' ');
    if (line.isEmpty || line.startsWith('```')) continue;
    final runes = line.runes.toList();
    return runes.length <= 40
        ? line
        : '${String.fromCharCodes(runes.take(40))}…';
  }
  return tr('新对话', 'New chat');
}

/// A message on its way to the server: what it said and carried, kept until
/// the server accepts it, so a refusal can give it back for editing.
class SentInput {
  final String channel, text;
  final List<Map<String, dynamic>> files;

  /// Why the server refused it; empty while it is on its way.
  final String code;
  const SentInput({
    required this.channel,
    required this.text,
    required this.files,
    this.code = '',
  });

  SentInput refused(String code) =>
      SentInput(channel: channel, text: text, files: files, code: code);
}

/// A file for the next message: on its way into the library, in it, or
/// refused.
class PendingFile {
  final String name;
  final int size;

  /// Local bytes to show an image before the server has it.
  final Uint8List? thumbnail;

  /// Picked from the library: removing it from the message keeps the file.
  final bool fromLibrary;

  /// The library entry, once uploaded.
  String? id;
  String? kind;
  String? error;
  PendingFile({
    required this.name,
    required this.size,
    this.thumbnail,
    this.fromLibrary = false,
    this.id,
    this.kind,
  });
  bool get uploading => id == null && error == null;
}

/// Platform IO only. Protobuf, ordering, deduplication and projection live in Rust.
class HubController extends ChangeNotifier {
  HubController() : _connect = _openSocket;

  @visibleForTesting
  HubController.testing({
    required ClientCore this._core,
    required LocalCache this._cache,
    required this._connect,
  });

  final WebSocketChannel Function(Uri) _connect;
  static WebSocketChannel _openSocket(Uri uri) =>
      WebSocketChannel.connect(uri, protocols: ['ryzhub.v1.protobuf']);

  String base = defaultServerUrl;
  String token = '', user = '', org = '', selected = '', email = '';
  int expiresAtMs = 0;

  /// online | connecting | retrying | background | offline
  String connection = 'offline', error = '';
  String mode = 'chat', harness = 'codex';
  String? project, draftModel, draftReasoning;

  /// `GET /orgs/{org}/models`: Chat models, and Work models grouped by harness.
  /// The wire protocol is the server's business; it follows each model.
  Map<String, dynamic> orgModels = {};
  bool creating = false;

  /// The draft is a temporary chat. Reserved: sent only when the server
  /// advertises `conversation_temporary`.
  bool temporary = false;
  int _view = 0;
  List<Map<String, dynamic>> projects = [];
  Map<String, dynamic> commandCatalog = {};

  /// The server's settings with any change it has not confirmed yet.
  Map<String, dynamic> get settings => {
    ...Map<String, dynamic>.from(commandCatalog['settings'] ?? {}),
    ...?_pendingConfig[channel],
  };

  /// Settings changed per channel but not yet seen in the command catalog.
  /// They show at once and travel with the next message, so the message's run
  /// uses them even if the separate UpdateRunConfig lands after it.
  final Map<String, Map<String, dynamic>> _pendingConfig = {};
  Map<String, dynamic> get selectedConversation =>
      conversations.where((c) => c['id'] == selected).firstOrNull ?? {};
  bool supports(String name) =>
      commandCatalog['capabilities']?[name] != null &&
      commandCatalog['capabilities']?[name] != 'unsupported';
  bool canFork(Map<String, dynamic> item) =>
      supports('fork') &&
      !running &&
      item['kind'] == 'assistant' &&
      item['state'] == 'completed' &&
      (item['run_id'] ?? '').toString().isNotEmpty &&
      (settings['harness'] != 'codex' ||
          (commandCatalog['fork_points'] as List? ?? []).contains(item['id']));
  final List<Map<String, dynamic>> conversations = [];
  final Map<String, Map<String, Map<String, dynamic>>> _items = {};
  final Map<String, List<Map<String, dynamic>>> _actions = {};
  final Map<String, List<Map<String, dynamic>>> _runs = {};
  final Set<String> _historyLoading = {}, _historyExhausted = {};
  final Set<String> _historyFailed = {};
  final Map<String, int> _historyVersion = {};
  Map<String, dynamic> capabilities = {};
  ClientCore? _core;
  LocalCache? _cache;
  WebSocketChannel? _socket;
  StreamSubscription<dynamic>? _subscription;
  StreamSubscription<List<ConnectivityResult>>? _network;
  bool _suspended = false;
  Timer? _retry, _batch, _resend, _heartbeat;
  final List<Uint8List> _frames = [];
  Future<void> _serial = Future.value();
  int _generation = 0, _attempt = 0;
  bool _closed = false;
  int? _connecting;
  DateTime _lastFrame = DateTime.now();
  final http.Client _http = sessionClient();
  Future<bool>? _renewing;

  /// Files picked for the next message.
  final List<PendingFile> pendingFiles = [];
  final LinkedHashMap<String, Future<Uint8List>> _previews = LinkedHashMap();
  bool get canAttach => capabilities['attachments'] is Map;
  int get maxFiles =>
      (capabilities['attachments']?['max_per_message'] as num?)?.toInt() ?? 10;
  bool get filesBusy => pendingFiles.any((f) => f.uploading);

  /// The largest file the server accepts, once it has said.
  int? get maxFileBytes =>
      (capabilities['attachments']?['max_bytes'] as num?)?.toInt();

  /// Ids of the picked files the server has, in order.
  List<String> get readyFiles => [
    for (final f in pendingFiles)
      if (f.id != null) f.id!,
  ];
  final String _device =
      'flutter-${DateTime.now().microsecondsSinceEpoch}-${Random().nextInt(1000000)}';

  /// The file library fills the main area in place of a conversation.
  bool libraryOpen = false;

  /// Messages the server has not accepted yet, by command id.
  final Map<String, SentInput> _outbox = {};

  /// The last message the server refused, to edit and send again.
  SentInput? failedSend;

  /// The session ran out and could not be renewed: sign in again.
  bool sessionEnded = false;

  /// Files the preview steps through, and the one shown; null while there is
  /// no preview.
  List<Map<String, dynamic>>? previewFiles;
  int previewIndex = 0;

  /// The preview was opened from the conversation and floats over it, and
  /// questions about the file go on in it. Otherwise it is the library's.
  bool previewInConversation = false;

  bool get signedIn => token.isNotEmpty;
  bool get isDraft => selected.isEmpty;
  String get channel => 'conv:$selected';

  // The instance capabilities must advertise each action before the UI offers it.
  bool get canPin => capabilities['conversation_pin'] == true;
  bool get canDelete => capabilities['conversation_delete'] == true;
  bool get canTemporary => capabilities['conversation_temporary'] == true;

  /// When the conversation was pinned, in Unix milliseconds; 0 if it is not.
  static int pinnedAt(Map<String, dynamic> c) =>
      (c['pinned_at_ms'] as num?)?.toInt() ?? 0;

  /// Most recently pinned first.
  List<Map<String, dynamic>> get pinned =>
      conversations
          .where(
            (c) =>
                pinnedAt(c) > 0 &&
                c['state'] == 'active' &&
                c['temporary'] != true,
          )
          .toList()
        ..sort((a, b) => pinnedAt(b).compareTo(pinnedAt(a)));
  List<Map<String, dynamic>> get recents => conversations
      .where(
        (c) =>
            c['state'] == 'active' &&
            pinnedAt(c) == 0 &&
            c['temporary'] != true &&
            c['project_id'] == null,
      )
      .toList();
  List<Map<String, dynamic>> get archived => conversations
      .where((c) => c['state'] == 'archived' && c['temporary'] != true)
      .toList();
  List<Map<String, dynamic>> projectConversations(String id) => conversations
      .where(
        (c) =>
            c['project_id'] == id &&
            c['state'] == 'active' &&
            c['temporary'] != true,
      )
      .toList();
  List<Map<String, dynamic>> get activeProjects =>
      projects.where((p) => p['state'] == 'active').toList();
  String? get contextProject =>
      isDraft ? project : selectedConversation['project_id'] as String?;

  List<Map<String, dynamic>> get chatModels => [
    for (final m in orgModels['chat'] as List? ?? [])
      Map<String, dynamic>.from(m),
  ];
  List<Map<String, dynamic>> get workGroups => [
    for (final g in orgModels['work'] as List? ?? [])
      Map<String, dynamic>.from(g),
  ];
  List<String> get availableHarnesses => [
    for (final g in workGroups) g['harness'] as String,
  ];
  List<Map<String, dynamic>> modelsFor(String mode, [String? harness]) =>
      mode == 'work'
      ? [
          for (final m
              in workGroups
                          .where((g) => g['harness'] == harness)
                          .firstOrNull?['models']
                      as List? ??
                  [])
            Map<String, dynamic>.from(m),
        ]
      : chatModels;
  Map<String, dynamic>? modelEntry(String mode, String? harness, String? id) =>
      modelsFor(mode, harness).where((m) => m['id'] == id).firstOrNull;
  String? defaultModel(String mode, [String? harness]) {
    final list = modelsFor(mode, harness);
    return (list.where((m) => m['default'] == true).firstOrNull ??
            list.firstOrNull)?['id']
        as String?;
  }

  /// Levels accepted by the server for the draft's model.
  List<String> get draftReasoningLevels =>
      (modelEntry(mode, harness, draftModel)?['reasoning_levels'] as List? ??
              const [])
          .cast<String>();
  bool get historyLoading => _historyLoading.contains(channel);
  bool get hasOlder => searchFocusSeq != null
      ? _searchHasMore
      : !_historyExhausted.contains(channel);
  bool get historyFailed => _historyFailed.contains(channel);

  Future<void> loadOlder() async {
    final name = channel;
    final current = items;
    if (_closed || current.isEmpty || !hasOlder || historyLoading) return;
    final version = _historyVersion[name] ?? 0;
    final core = _core!;
    final focused = searchFocusSeq;
    final view = _view;
    _historyLoading.add(name);
    notifyListeners();
    try {
      final dynamic body;
      try {
        body = await request(
          '/orgs/$org/conversations/$selected/items?before_seq=${current.first['first_seq']}',
        );
      } catch (_) {
        _historyFailed.add(name);
        rethrow;
      }
      _historyFailed.remove(name);
      // The server says whether older items remain; older servers sent a
      // bare list, where a short page meant the end.
      final page = body is Map ? body['items'] as List : body as List;
      final more = body is Map ? body['has_more'] == true : page.length >= 100;
      if (focused != null) {
        if (!_closed && view == _view && searchFocusSeq == focused) {
          _searchItems = [
            ...?_searchItems,
            ...page.map((v) => Map<String, dynamic>.from(v)),
          ];
          _searchHasMore = more;
        }
        return;
      }
      final task = _serial.then((_) async {
        if (_closed ||
            core != _core ||
            !_items.containsKey(name) ||
            (_historyVersion[name] ?? 0) != version) {
          return;
        }
        await _apply(
          core.mergeHistory(channel: name, itemsJson: jsonEncode(page)),
        );
        if (!more) _historyExhausted.add(name);
      });
      _serial = task.catchError((Object e) {
        if (!_closed) {
          error = '$e';
          notifyListeners();
        }
      });
      await task;
    } finally {
      _historyLoading.remove(name);
      if (!_closed) notifyListeners();
    }
  }

  List<Map<String, dynamic>> get items =>
      (searchFocusSeq != null
            ? [...?_searchItems]
            : (_items[channel]?.values.toList() ?? []))
        ..sort(
          (a, b) => (a['first_seq'] as num).compareTo(b['first_seq'] as num),
        );
  List<Map<String, dynamic>> get actions =>
      (_actions[channel] ?? []).where((a) => a['status'] == 'pending').toList();
  bool get running => (_runs[channel] ?? []).any(
    (r) => r['state'] == 'running' || r['state'] == 'requires_action',
  );

  /// The run is working or waiting for the user.
  bool runActive(String id) => (_runs[channel] ?? []).any(
    (r) =>
        r['run_id'] == id &&
        (r['state'] == 'running' || r['state'] == 'requires_action'),
  );

  /// How long a finished run worked, when its times are known.
  Duration? workedFor(String id) {
    final run = (_runs[channel] ?? []).where((r) => r['run_id'] == id);
    final started = run.firstOrNull?['started_at_ms'];
    final ended = run.firstOrNull?['ended_at_ms'];
    if (started is! int || ended is! int || started <= 0 || ended < started) {
      return null;
    }
    return Duration(milliseconds: ended - started);
  }

  bool get mainlineRunning => (_runs[channel] ?? []).any(
    (r) =>
        r['read_only'] != true &&
        (r['state'] == 'running' || r['state'] == 'requires_action'),
  );
  bool get realChat =>
      capabilities['chat'] != 'mock' &&
      conversations.where((c) => c['id'] == selected).firstOrNull?['mode'] !=
          'work';
  bool get realWork =>
      capabilities['work'] != 'mock' &&
      conversations.where((c) => c['id'] == selected).firstOrNull?['mode'] ==
          'work';
  String get agentLabel => realWork
      ? (settings['harness'] == 'claude' ? 'Claude Code' : 'Codex')
      : realChat
      ? 'Chat'
      : tr('模拟 Agent', 'Mock agent');
  Future<void> refreshModels() async {
    try {
      orgModels = Map<String, dynamic>.from(await request('/orgs/$org/models'));
    } catch (_) {
      // Servers without the catalog keep their defaults; no picker is shown.
      orgModels = {};
    }
    notifyListeners();
  }

  /// Recoverable for 30 days on the server; its chats leave the lists.
  Future<void> deleteProject(String id) async {
    final current = contextProject == id;
    await request('/orgs/$org/projects/$id', method: 'DELETE');
    await refreshProjects();
    await refreshConversations();
    if (current) startDraft();
  }

  Future<void> refreshProjects() async {
    projects = (await request('/orgs/$org/projects') as List)
        .map((p) => Map<String, dynamic>.from(p))
        .toList();
    notifyListeners();
  }

  Future<void> refreshCommands() async {
    if (selected.isEmpty) return;
    final id = selected;
    final value = await request('/orgs/$org/conversations/$id/commands');
    if (selected == id) commandCatalog = Map<String, dynamic>.from(value);
    final confirmed = Map<String, dynamic>.from(value['settings'] ?? {});
    _pendingConfig['conv:$id']?.removeWhere((k, v) => confirmed[k] == v);
    notifyListeners();
  }

  // Commands capture the channel when called: the serial queue may run them
  // after the user has already switched conversations.
  Future<void> configure(Map<String, dynamic> patch) async {
    if (isDraft) return;
    final ch = channel;
    (_pendingConfig[ch] ??= {}).addAll(patch);
    notifyListeners();
    await _schedule(
      () => _core!.configure(channel: ch, configJson: jsonEncode(patch)),
    );
    // No timeline notice confirms a change any more; check the catalog.
    unawaited(
      Future<void>.delayed(
        const Duration(milliseconds: 800),
        refreshCommands,
      ).catchError((_) {}),
    );
  }

  Future<void> compact() async {
    if (isDraft) return;
    final ch = channel;
    await _schedule(() => _core!.compact(channel: ch));
  }

  Future<void> fork(String item) async {
    if (isDraft) return;
    final ch = channel;
    await _schedule(() => _core!.fork(channel: ch, itemId: item));
  }

  Future<void> moveProject(String conv, String? id) async {
    final result = await request(
      '/orgs/$org/conversations/$conv/project',
      body: {'project_id': id},
    );
    if (result['memory_notice'] != null) error = result['memory_notice'];
    await refreshConversations();
    notifyListeners();
  }

  Future<void> rename(String conv, String title) async {
    await request(
      '/orgs/$org/conversations/$conv/settings',
      method: 'PATCH',
      body: {'title': title},
    );
    await refreshConversations();
  }

  /// Work conversations archive through their workspace.
  Future<void> archive(String conv) async {
    final c = conversations.where((c) => c['id'] == conv).firstOrNull;
    if (c?['mode'] == 'work' && capabilities['work'] != 'mock') {
      await workspaceOperation('archive', conv: conv);
      return;
    }
    await request(
      '/orgs/$org/conversations/$conv/settings',
      method: 'PATCH',
      body: {'archive': true},
    );
    await refreshConversations();
    if (conv == selected) startDraft();
  }

  Future<void> unarchive(String conv) async {
    await request(
      '/orgs/$org/conversations/$conv/settings',
      method: 'PATCH',
      body: {'archive': false},
    );
    await refreshConversations();
  }

  Future<dynamic> exportConversation(String conv) =>
      request('/orgs/$org/conversations/$conv/export');

  Future<void> pin(String conv, bool value) async {
    if (!canPin) throw Exception(tr('服务器暂不支持置顶', 'Pinning is not available'));
    await request(
      '/orgs/$org/conversations/$conv/settings',
      method: 'PATCH',
      body: {'pinned': value},
    );
    await refreshConversations();
  }

  /// Reserved contract: `DELETE /orgs/{org}/conversations/{conv}`.
  Future<void> deleteConversation(
    String conv, {
    bool deleteMemories = false,
  }) async {
    if (!canDelete) {
      throw Exception(tr('服务器暂不支持删除', 'Deleting is not available'));
    }
    await request(
      '/orgs/$org/conversations/$conv?delete_memories=$deleteMemories',
      method: 'DELETE',
    );
    await refreshConversations();
    if (conv == selected) startDraft();
  }

  Future<Map<String, dynamic>> me() async =>
      Map<String, dynamic>.from(await request('/me'));

  Map<String, dynamic> workspace = {};

  /// Workspace jobs under way, by conversation: a job keeps running, and
  /// belongs to its conversation, while another is open.
  final Map<String, String> _workspaceBusy = {};
  String get workspaceBusy => _workspaceBusy[selected] ?? '';
  Future<void> refreshWorkspace() async {
    if (!realWork) return;
    final conv = selected, view = _view;
    final status = Map<String, dynamic>.from(
      await request('/orgs/$org/conversations/$conv/workspace'),
    );
    // Answered after another conversation opened: it is not this one's.
    if (selected != conv || view != _view) return;
    workspace = status;
    notifyListeners();
  }

  Future<void> workspaceOperation(String kind, {String? conv}) async {
    conv ??= selected;
    _workspaceBusy[conv] = kind;
    notifyListeners();
    try {
      dynamic job;
      for (var attempt = 0; attempt < 120; attempt++) {
        try {
          job = await request(
            '/orgs/$org/conversations/$conv/workspace',
            body: {'kind': kind},
          );
          break;
        } catch (e) {
          if (!e.toString().contains('WORKSPACE_BUSY')) rethrow;
          await Future<void>.delayed(const Duration(milliseconds: 500));
        }
      }
      if (job == null) {
        throw Exception(
          tr(
            '工作区仍在保存，请稍后重试。',
            'The workspace is still saving. Try again shortly.',
          ),
        );
      }
      for (var attempt = 0; attempt < 600; attempt++) {
        final status = Map<String, dynamic>.from(
          await request('/orgs/$org/conversations/$conv/workspace'),
        );
        if (selected == conv) workspace = status;
        notifyListeners();
        final jobs = (status['jobs'] as List).where(
          (j) => j['id'] == job['id'],
        );
        if (jobs.isNotEmpty) {
          if (jobs.first['state'] == 'completed') return;
          if (jobs.first['state'] == 'failed') {
            throw Exception(jobs.first['result']['error']);
          }
        }
        await Future<void>.delayed(const Duration(milliseconds: 500));
      }
      throw Exception(
        tr('任务仍在后台运行，请刷新状态。', 'The job is still running. Refresh later.'),
      );
    } finally {
      _workspaceBusy.remove(conv);
      notifyListeners();
      // Workspace archive/restore changes the conversation state without a
      // user-channel event.
      if (!_closed) unawaited(refreshConversations().catchError((_) {}));
    }
  }

  /// Requests that exchange credentials themselves; any other request whose
  /// access token expired renews it once.
  static const _exchanges = {
    '/auth/login',
    '/auth/register',
    '/auth/refresh',
    '/auth/logout',
  };

  Future<dynamic> request(
    String path, {
    Map<String, dynamic>? body,
    String? method,
  }) async {
    var response = await _send(path, body, method);
    if (response.statusCode == 401 &&
        token.isNotEmpty &&
        !_exchanges.contains(path) &&
        await _renew()) {
      response = await _send(path, body, method);
    }
    final data = response.body.isEmpty ? null : jsonDecode(response.body);
    if (response.statusCode >= 400) {
      throw Exception(data?['error'] ?? response.statusCode);
    }
    return data;
  }

  Future<http.Response> _send(
    String path,
    Map<String, dynamic>? body,
    String? method,
  ) {
    final headers = {
      'Content-Type': 'application/json',
      if (token.isNotEmpty) 'Authorization': 'Bearer $token',
    };
    final url = Uri.parse('$base$path');
    return (method == 'PATCH'
            ? _http.patch(url, headers: headers, body: jsonEncode(body))
            : method == 'DELETE'
            ? _http.delete(url, headers: headers)
            : body == null
            ? _http.get(url, headers: headers)
            : _http.post(url, headers: headers, body: jsonEncode(body)))
        .timeout(const Duration(seconds: 15));
  }

  /// Upload [size] bytes from [open] as [name] to the member's library. [open]
  /// is called again if the access token has to be renewed first.
  Future<Map<String, dynamic>> uploadFile(
    String name,
    int size,
    Stream<List<int>> Function() open,
  ) async {
    Future<http.StreamedResponse> once() {
      final query = Uri(queryParameters: {'name': name}).query;
      final request = http.StreamedRequest(
        'POST',
        Uri.parse('$base/orgs/$org/attachments?$query'),
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
      return _http.send(request);
    }

    var response = await once();
    if (response.statusCode == 401 && await _renew()) response = await once();
    final body = await response.stream.bytesToString();
    final data = body.isEmpty ? null : jsonDecode(body);
    if (response.statusCode >= 400) {
      throw Exception(data?['error'] ?? response.statusCode);
    }
    return Map<String, dynamic>.from(data);
  }

  /// A file's bytes from [path], e.g. its preview or content.
  Future<Uint8List> fileBytes(String path) async {
    Future<http.Response> once() => _http.get(
      Uri.parse('$base$path'),
      headers: {'Authorization': 'Bearer $token'},
    );
    var response = await once();
    if (response.statusCode == 401 && await _renew()) response = await once();
    if (response.statusCode >= 400) {
      String? code;
      try {
        code = jsonDecode(response.body)['error'];
      } catch (_) {}
      throw Exception(code ?? response.statusCode);
    }
    return response.bodyBytes;
  }

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

  /// Preview [files], starting at [index]: over the conversation, or else
  /// over the main area.
  void openPreview(
    List<Map<String, dynamic>> files, {
    int index = 0,
    bool inConversation = false,
  }) {
    if (files.isEmpty) return;
    previewFiles = List.unmodifiable(files);
    previewIndex = index.clamp(0, files.length - 1);
    previewInConversation = inConversation;
    notifyListeners();
  }

  void previewAt(int index) {
    final files = previewFiles;
    if (files == null || index < 0 || index >= files.length) return;
    previewIndex = index;
    notifyListeners();
  }

  void closePreview() {
    if (previewFiles == null) return;
    previewFiles = null;
    notifyListeners();
  }

  /// Show the file library instead of a conversation; the conversation
  /// closes as when a new chat starts.
  void openLibrary() {
    startDraft();
    libraryOpen = true;
    notifyListeners();
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

  /// Delete a file everywhere: every conversation that carried it loses it,
  /// and so does the next message.
  Future<void> deleteFile(String id) async {
    await request('/orgs/$org/attachments/$id', method: 'DELETE');
    _previews.remove(id);
    _originals.remove(id);
    final before = pendingFiles.length;
    pendingFiles.removeWhere((f) => f.id == id);
    if (pendingFiles.length != before) notifyListeners();
  }

  /// Upload a file for the next message.
  Future<void> attach(
    String name,
    int size,
    Stream<List<int>> Function() open, {
    Uint8List? thumbnail,
  }) async {
    final file = PendingFile(name: name, size: size, thumbnail: thumbnail);
    pendingFiles.add(file);
    notifyListeners();
    final limit = maxFileBytes;
    if (pendingFiles.length > maxFiles) {
      file.error = 'TOO_MANY_ATTACHMENTS';
    } else if (limit != null && size > limit) {
      file.error = 'ATTACHMENT_TOO_LARGE';
    } else {
      try {
        final uploaded = await uploadFile(name, size, open);
        file.id = uploaded['id'] as String;
        file.kind = uploaded['kind'] as String?;
        // Removed while it uploaded: nothing will send it.
        if (!pendingFiles.contains(file)) {
          unawaited(deleteFile(file.id!).catchError((_) {}));
        }
      } catch (e) {
        file.error = '$e'.replaceFirst('Exception: ', '');
      }
    }
    notifyListeners();
  }

  /// Add files of the library to the next message.
  void attachFromLibrary(Iterable<Map<String, dynamic>> files) {
    for (final file in files) {
      if (pendingFiles.any((f) => f.id == file['id'])) continue;
      pendingFiles.add(
        PendingFile(
          name: file['name'] as String,
          size: (file['size'] as num).toInt(),
          fromLibrary: true,
          id: file['id'] as String,
          kind: file['kind'] as String?,
        ),
      );
    }
    notifyListeners();
  }

  /// Take a file off the next message. A file uploaded for it is deleted; one
  /// from the library stays there.
  void detach(PendingFile file) {
    pendingFiles.remove(file);
    if (!file.fromLibrary && file.id != null) {
      unawaited(deleteFile(file.id!).catchError((_) {}));
    }
    notifyListeners();
  }

  /// Renew the session before it ends and carry the connection over to the new
  /// token, keeping its channels. Without a renewal the server closes the
  /// connection at expiry, and reconnecting ends the session.
  Future<void> _carryOver() async {
    final generation = _generation, socket = _socket;
    if (await _renew() &&
        _current(generation) &&
        socket != null &&
        identical(socket, _socket)) {
      _sendFrames([_core!.authRefresh(token: token)]);
    }
  }

  /// The session ran out and could not be renewed. The page signs in afresh;
  /// the cache, with any queued command, stays for the next sign-in.
  void _endSession() {
    if (sessionEnded) return;
    sessionEnded = true;
    _closed = true;
    _generation++;
    _retry?.cancel();
    _resend?.cancel();
    _heartbeat?.cancel();
    connection = 'offline';
    notifyListeners();
  }

  /// A new access token through the device's refresh cookie, shared by the
  /// requests that found theirs expired. False when the device is signed out.
  Future<bool> _renew() {
    if (!keepsSession) return Future.value(false);
    return _renewing ??= () async {
      try {
        final result = await request('/auth/refresh', body: {});
        if (_closed || result['user_id'] != user) return false;
        token = result['token'];
        expiresAtMs = (result['expires_at_ms'] as num?)?.toInt() ?? 0;
        return true;
      } catch (_) {
        return false;
      } finally {
        _renewing = null;
      }
    }();
  }

  Future<void> login(
    String server,
    String email,
    String password,
    bool register,
  ) async {
    base = sameSiteServer(server.replaceAll(RegExp(r'/+$'), ''));
    capabilities = Map<String, dynamic>.from(await request('/capabilities'));
    await _start(
      await request(
        register ? '/auth/register' : '/auth/login',
        body: {'email': email, 'password': password},
      ),
    );
  }

  /// Signs this device back in to [server] with its refresh cookie. False
  /// when it has none or it lapsed, and on platforms that keep no session.
  Future<bool> restore(String server) async {
    if (!keepsSession || server.isEmpty) return false;
    base = sameSiteServer(server.replaceAll(RegExp(r'/+$'), ''));
    try {
      capabilities = Map<String, dynamic>.from(await request('/capabilities'));
      await _start(await request('/auth/refresh', body: {}));
      return true;
    } catch (_) {
      token = '';
      return false;
    }
  }

  bool has(String conversation) =>
      conversations.any((c) => c['id'] == conversation);

  Future<void> _start(Map<String, dynamic> result) async {
    token = result['token'];
    user = result['user_id'];
    org = result['org_id'];
    email = result['email'] ?? '';
    expiresAtMs = (result['expires_at_ms'] as num?)?.toInt() ?? 0;
    _cache = await LocalCache.open('$base:$user:$org');
    _core = ClientCore(
      records: [
        for (final MapEntry(:key, :value) in (await _cache!.load()).entries)
          CacheRecord(key: key, value: value),
      ],
    );
    final restored =
        jsonDecode(_core!.viewJson())['channels'] as Map<String, dynamic>;
    for (final entry in restored.entries) {
      _items[entry.key] = (entry.value['items'] as Map<String, dynamic>).map(
        (id, value) => MapEntry(id, Map<String, dynamic>.from(value)),
      );
      _actions[entry.key] = (entry.value['actions'] as Map<String, dynamic>)
          .values
          .map((v) => Map<String, dynamic>.from(v))
          .toList();
      _runs[entry.key] = (entry.value['runs'] as Map<String, dynamic>).values
          .map((v) => Map<String, dynamic>.from(v))
          .toList();
    }
    await refreshConversations();
    await refreshProjects();
    await refreshModels();
    startDraft();
    await _schedule(() => _core!.subscribe(channel: 'user:$org:$user'));
    await reconnect();
    _network = Connectivity().onConnectivityChanged.listen((_) {
      if (!_suspended) reconnect();
    });
    _resend = Timer.periodic(const Duration(seconds: 5), (_) {
      if (_canSend) _sendFrames(_core!.retryPending());
    });
    _heartbeat = Timer.periodic(const Duration(seconds: 5), (_) {
      if (_canSend && _heartbeatExpired) {
        reconnect();
      }
    });
    notifyListeners();
  }

  Future<void> refreshConversations() async {
    final list = await request('/orgs/$org/conversations') as List;
    conversations
      ..clear()
      ..addAll(list.map((v) => Map<String, dynamic>.from(v)));
    notifyListeners();
  }

  /// A draft has no server conversation until its first message is sent.
  void startDraft({
    String? mode,
    String? harness,
    String? model,
    String? project,
  }) {
    _view++;
    libraryOpen = false;
    previewFiles = null;
    selected = '';
    workspace = {};
    commandCatalog = {};
    draftReasoning = null;
    temporary = false;
    searchFocusSeq = null;
    this.project = project;
    final defaults = _projectDefaults;
    this.mode = mode ?? defaults?['mode'] ?? this.mode;
    if (harness != null) this.harness = harness;
    if (harness == null && this.mode == 'work' && defaults?['mode'] == 'work') {
      this.harness = defaults?['harness'] ?? this.harness;
    }
    draftModel = model;
    _normalizeDraft();
    notifyListeners();
  }

  Map? get _projectDefaults =>
      projects.where((p) => p['id'] == project).firstOrNull?['defaults']
          as Map?;

  /// Keeps the draft on a model the server offers: the explicit pick, then the
  /// project's default, then this device's default, then the server default.
  void _normalizeDraft() {
    final harnesses = availableHarnesses;
    if (mode == 'work' && !harnesses.contains(harness)) {
      harness = harnesses.contains(prefs.defaultHarness)
          ? prefs.defaultHarness
          : harnesses.firstOrNull ?? 'codex';
    }
    bool valid(String? id) => modelEntry(mode, harness, id) != null;
    final defaults = _projectDefaults;
    String? projectModel;
    if (defaults != null && (defaults['mode'] ?? 'chat') == mode) {
      projectModel = defaults['model'] as String?;
      // Older projects stored a wire protocol; show that provider's model.
      if (projectModel == null &&
          mode == 'chat' &&
          defaults['protocol'] != null) {
        final provider = defaults['protocol'] == 'responses'
            ? 'openai'
            : 'anthropic';
        projectModel = chatModels
            .where((m) => m['provider'] == provider)
            .firstOrNull?['id'];
      }
    }
    final preferred = mode == 'work'
        ? prefs.defaultWorkModel
        : prefs.defaultChatModel;
    draftModel =
        [draftModel, projectModel, preferred].where(valid).firstOrNull ??
        defaultModel(mode, harness);
    if (!draftReasoningLevels.contains(draftReasoning)) draftReasoning = null;
  }

  /// Edits draft settings, then re-checks the model and reasoning choice.
  void updateDraft(void Function() change) {
    change();
    _normalizeDraft();
    notifyListeners();
  }

  int? searchFocusSeq;
  List<Map<String, dynamic>>? _searchItems;
  bool _searchHasMore = false;
  Future<void> selectSearch(String id, String? itemId, int? seq) async {
    await select(id);
    if (selected != id || itemId == null || seq == null) return;
    final view = _view;
    final body = await request(
      '/orgs/$org/conversations/$id/items?before_seq=${seq + 1}',
    );
    if (_closed || view != _view) return;
    if (view == _view) {
      searchFocusSeq = seq;
      _searchItems = (body['items'] as List)
          .map((v) => Map<String, dynamic>.from(v))
          .toList();
      _searchHasMore = body['has_more'] == true;
      notifyListeners();
    }
  }

  void clearSearchFocus() {
    searchFocusSeq = null;
    _searchItems = null;
    notifyListeners();
  }

  Future<void> select(String id) async {
    searchFocusSeq = null;
    final view = ++_view;
    libraryOpen = false;
    previewFiles = null;
    selected = id;
    workspace = {};
    commandCatalog = {};
    notifyListeners();
    // The serial queue keeps subscriptions in click order: the last one wins.
    await _schedule(() => _core!.subscribe(channel: 'conv:$id'));
    if (view != _view) return;
    await refreshCommands();
    if (view == _view && realWork) await refreshWorkspace();
    notifyListeners();
  }

  /// Send [text] with the files picked in the composer, or with the files
  /// in [only], leaving the composer's other files for the next message.
  Future<void> send(String text, {List<Map<String, dynamic>>? only}) async {
    final picked = only == null;
    final carried =
        only ??
        [
          for (final f in pendingFiles)
            if (f.id != null)
              {'id': f.id, 'name': f.name, 'size': f.size, 'kind': f.kind},
        ];
    final files = [for (final f in carried) f['id'] as String];
    if ((text.trim().isEmpty && files.isEmpty) || (picked && filesBusy)) {
      return;
    }
    searchFocusSeq = null;
    if (isDraft) return _createAndSend(text, carried, picked: picked);
    final ch = channel, steerable = realChat || realWork;
    final pending = _pendingConfig[ch]?.isNotEmpty == true
        ? Map<String, dynamic>.of(_pendingConfig[ch]!)
        : null;
    await _schedule(
      () => _remember(
        steerable &&
                (_runs[ch] ?? []).any(
                  (r) =>
                      r['read_only'] != true &&
                      (r['state'] == 'running' ||
                          r['state'] == 'requires_action'),
                )
            ? _core!.steer(channel: ch, text: text, attachmentIds: files)
            : _core!.sendText(
                channel: ch,
                text: text,
                configJson: pending == null ? null : jsonEncode(pending),
                attachmentIds: files,
              ),
        SentInput(channel: ch, text: text, files: carried),
      ),
    );
    _sent(files);
  }

  /// [update], with the message it queued kept under its command id before
  /// anything goes out, so its result always finds it.
  Update _remember(Update update, SentInput input) {
    final cid = update.cid;
    if (cid != null) _outbox[cid] = input;
    return update;
  }

  /// The commands the server finished with: accepted ones are forgotten, a
  /// refused message comes back for editing.
  void _results(String json) {
    for (final result in jsonDecode(json) as List) {
      final cid = result['cid'] as String;
      final sent = _outbox.remove(cid);
      if (result['accepted'] == true) continue;
      if (result['retryable'] == true) {
        // The same command goes again.
        if (sent != null) _outbox[cid] = sent;
        continue;
      }
      final code = result['code'] as String;
      // A refused setting must not ride along with this conversation's
      // later messages; other conversations keep theirs.
      if (_configErrors.contains(code)) {
        _pendingConfig.remove(result['channel']);
      }
      if (sent != null) {
        failedSend = sent.refused(code);
      } else {
        error = code;
      }
    }
  }

  @visibleForTesting
  void debugQueued(String cid, SentInput input) => _outbox[cid] = input;

  @visibleForTesting
  void debugResults(String json) => _results(json);

  @visibleForTesting
  Future<void> debugChanges(String json) => _apply(
    Update(
      changesJson: json,
      cacheWrites: const [],
      outboundFrames: const [],
      resultsJson: '[]',
      authExpiring: false,
    ),
  );

  @visibleForTesting
  void debugPendingConfig(String channel, Map<String, dynamic> patch) =>
      (_pendingConfig[channel] ??= {}).addAll(patch);

  /// Take back the refused message, to edit and send again.
  SentInput? takeFailedSend() {
    final failed = failedSend;
    failedSend = null;
    notifyListeners();
    return failed;
  }

  /// The files went with a message: the composer no longer holds them.
  void _sent(List<String> files) {
    if (files.isEmpty) return;
    pendingFiles.removeWhere((f) => files.contains(f.id));
    notifyListeners();
  }

  Future<void> _createAndSend(
    String text,
    List<Map<String, dynamic>> carried, {
    required bool picked,
  }) async {
    final files = [for (final f in carried) f['id'] as String];
    if (creating) return;
    creating = true;
    notifyListeners();
    try {
      final reasoning = draftReasoningLevels.contains(draftReasoning)
          ? draftReasoning
          : null;
      // Reasoning goes into the create body: a later UpdateRunConfig could
      // race the first message and would only apply from the next turn.
      final created = Map<String, dynamic>.from(
        await request(
          '/orgs/$org/conversations',
          body: {
            'title': conversationTitle(
              text.trim().isEmpty && picked && pendingFiles.isNotEmpty
                  ? pendingFiles.first.name
                  : text,
            ),
            'mode': mode,
            'project_id': project,
            if (temporary && canTemporary) 'temporary': true,
            // No wire protocol: the server derives it from the model.
            'run_config': {
              if (mode == 'work') 'harness': harness,
              'model': ?draftModel,
              'reasoning': ?reasoning,
            },
          },
        ),
      );
      // The user-channel event may have inserted it already.
      conversations
        ..removeWhere((c) => c['id'] == created['id'])
        ..insert(0, created);
      final id = created['id'] as String, ch = 'conv:$id';
      _view++;
      selected = id;
      workspace = {};
      commandCatalog = {};
      draftReasoning = null;
      temporary = false;
      notifyListeners();
      await _schedule(() => _core!.subscribe(channel: ch));
      await _schedule(
        () => _remember(
          _core!.sendText(channel: ch, text: text, attachmentIds: files),
          SentInput(channel: ch, text: text, files: carried),
        ),
      );
      _sent(files);
      unawaited(refreshCommands().catchError((_) {}));
      if (realWork) unawaited(refreshWorkspace().catchError((_) {}));
    } finally {
      creating = false;
      notifyListeners();
    }
  }

  Future<void> sideQuery(String text) async {
    if (isDraft) return;
    final ch = channel;
    await _schedule(() => _core!.sideQuery(channel: ch, text: text));
  }

  Future<void> answer(Map<String, dynamic> action, String text) async {
    if (isDraft) return;
    final ch = channel;
    await _schedule(
      () => _core!.answerQuestion(
        channel: ch,
        actionId: action['id'],
        runEpoch: action['run_epoch'],
        answer: text,
      ),
    );
  }

  Future<void> decide(Map<String, dynamic> action, String decision) async {
    if (isDraft) return;
    final ch = channel;
    await _schedule(
      () => _core!.decide(
        channel: ch,
        actionId: action['id'],
        runEpoch: action['run_epoch'],
        decision: decision,
      ),
    );
  }

  Future<void> cancel() async {
    if (isDraft) return;
    final ch = channel;
    await _schedule(() => _core!.cancel(channel: ch));
  }

  Future<void> _schedule(Update Function() update) {
    final task = _serial.then((_) async {
      if (!_closed) await _apply(update());
    });
    _serial = task.catchError((Object e) {
      if (_closed) return;
      error = '$e';
      notifyListeners();
    });
    return task;
  }

  Future<void> _apply(Update update) async {
    // The caller is serialized. Write the command queue/cursor before any network send; only
    // stored commands are ever sent again.
    if (update.cacheWrites.isNotEmpty) {
      await _cache!.write({for (final w in update.cacheWrites) w.key: w.value});
      if (_closed) return;
      _core!.committed();
    }
    if (_closed) return;
    var needsConversationRefresh = false;
    var needsCommandRefresh = false;
    for (final diff in jsonDecode(update.changesJson) as List) {
      final name = diff['channel'] as String;
      if (diff['revoked'] == true || diff['reset'] == true) {
        _historyVersion[name] = (_historyVersion[name] ?? 0) + 1;
        _historyExhausted.remove(name);
      }
      if (diff['revoked'] == true) {
        if (name == channel) {
          searchFocusSeq = null;
          _searchItems = null;
        }
        _items.remove(name);
        _actions.remove(name);
        _runs.remove(name);
        continue;
      }
      if (diff['reset'] == true) _items[name] = {};
      final items = _items.putIfAbsent(name, () => {});
      for (final item in diff['items'] as List) {
        items[item['id']] = Map<String, dynamic>.from(item);
        if (name == channel && item['kind'] == 'notice') {
          needsCommandRefresh = true;
        }
        if (name == 'user:$org:$user' && item['kind'] == 'conversation') {
          final index = conversations.indexWhere((c) => c['id'] == item['id']);
          if (index < 0) {
            // User-channel items carry a title, but not the conversation mode.
            needsConversationRefresh = true;
          } else {
            final conversation = conversations[index];
            conversation['title'] = item['text'];
            conversation['state'] = item['state'];
            // Items cached before pins were carried leave the pin alone.
            final pin = item['detail']?['kind']?['Conversation'];
            if (pin != null) conversation['pinned_at_ms'] = pin['pinned_at_ms'];
          }
        }
      }
      _actions[name] = (diff['actions'] as List)
          .map((v) => Map<String, dynamic>.from(v))
          .toList();
      final runs = (diff['runs'] as List? ?? [])
          .map((v) => Map<String, dynamic>.from(v))
          .toList();
      if (name == channel &&
          runs.any(
            (r) =>
                r['state'] == 'completed' &&
                (_runs[name] ?? [])
                        .where((old) => old['run_id'] == r['run_id'])
                        .firstOrNull?['state'] !=
                    'completed',
          )) {
        needsCommandRefresh = true;
      }
      _runs[name] = runs;
    }
    if (needsConversationRefresh) await refreshConversations();
    if (_closed) return;
    if (needsCommandRefresh) unawaited(refreshCommands().catchError((_) {}));
    if (update.error != null) error = update.error!;
    _results(update.resultsJson);
    if (update.authExpiring) unawaited(_carryOver());
    _sendFrames(update.outboundFrames);
    notifyListeners();
  }

  static const _configErrors = {
    'INVALID_CONFIG',
    'INVALID_REASONING',
    'INVALID_APPROVAL_MODE',
    'MODEL_NOT_ROUTABLE',
    'MODEL_UNSUPPORTED',
    'HARNESS_PROTOCOL_IMMUTABLE',
    'PLAN_UNSUPPORTED',
    'POLICY_DENIED',
  };

  void _drain() {
    _batch?.cancel();
    _batch = null;
    if (_frames.isEmpty || _closed) return;
    final batch = List<Uint8List>.of(_frames);
    _frames.clear();
    _serial = _serial
        .then((_) async {
          if (!_closed) await _apply(_core!.ingest(frames: batch));
        })
        .catchError((Object e) {
          if (_closed) return;
          error = '$e';
          notifyListeners();
        });
  }

  bool _current(int generation) =>
      !_closed && !_suspended && generation == _generation;

  bool get _canSend =>
      !_closed && !_suspended && connection == 'online' && _socket != null;

  bool get _heartbeatExpired =>
      DateTime.now().difference(_lastFrame).inSeconds > 55;

  void _sendFrames(Iterable<Uint8List> frames) {
    if (!_canSend) return;
    final socket = _socket!;
    for (final frame in frames) {
      socket.sink.add(frame);
    }
  }

  /// Detach before awaiting cancellation or close: queued work must not see
  /// a socket being closed, and an old close must not touch a new connection.
  Future<void> _closeSocket() async {
    final subscription = _subscription, socket = _socket;
    _subscription = null;
    _socket = null;
    try {
      await subscription?.cancel();
    } finally {
      await socket?.sink.close();
    }
  }

  Future<void> reconnect() async {
    if (_closed || _suspended || !signedIn || _connecting != null) return;
    _retry?.cancel();
    final generation = ++_generation;
    _connecting = generation;
    connection = 'connecting';
    notifyListeners();
    try {
      await _closeSocket();
      if (!_current(generation)) return;
      _drain();
      await _serial;
      if (!_current(generation)) return;
      final ticket = await request('/auth/ticket', body: {});
      if (!_current(generation)) return;
      final uri = Uri.parse('$base/ws').replace(
        scheme: base.startsWith('https:') ? 'wss' : 'ws',
        queryParameters: {'ticket': ticket['ticket']},
      );
      final socket = _connect(uri);
      _socket = socket;
      await socket.ready;
      if (!_current(generation)) {
        await socket.sink.close();
        return;
      }
      connection = 'online';
      _attempt = 0;
      _lastFrame = DateTime.now();
      _subscription = socket.stream.listen(
        (dynamic message) {
          if (!_current(generation)) return;
          _lastFrame = DateTime.now();
          _frames.add(Uint8List.fromList((message as List).cast<int>()));
          _batch ??= Timer(const Duration(milliseconds: 32), _drain);
        },
        onError: (Object e) {
          if (!_current(generation)) return;
          _connectionError(e);
          _disconnected(generation);
        },
        onDone: () => _disconnected(generation),
      );
      _sendFrames([_core!.hello(deviceId: _device)]);
    } catch (e) {
      if (!_current(generation)) return;
      // The ticket request renews an expired token itself; refused still, the
      // session is over rather than the network down.
      if ('$e'.contains('UNAUTHORIZED')) {
        return _endSession();
      }
      _connectionError(e);
      _disconnected(generation);
    } finally {
      if (_connecting == generation) _connecting = null;
      if (_current(generation)) notifyListeners();
    }
  }

  /// Transport failures show as the connection status; only server answers
  /// (for example an expired session) reach the error banner.
  void _connectionError(Object e) {
    if (e is http.ClientException ||
        e is TimeoutException ||
        e is WebSocketChannelException) {
      return;
    }
    error = '$e';
  }

  void _disconnected(int generation) {
    if (!_current(generation)) return;
    connection = 'retrying';
    _retry?.cancel();
    _retry = Timer(
      Duration(
        milliseconds: reconnectDelay(
          attempt: _attempt++,
          random: Random().nextInt(1 << 30),
        ),
      ),
      reconnect,
    );
    notifyListeners();
  }

  Future<void> suspend() async {
    if (_closed || _suspended) return;
    _suspended = true;
    _generation++;
    _connecting = null;
    _retry?.cancel();
    connection = 'background';
    final closing = _closeSocket();
    _drain();
    notifyListeners();
    await closing;
    await _serial;
  }

  Future<void> resume() async {
    _suspended = false;
    // Web focus/visibility changes also resume a page whose socket stayed open.
    if (_canSend && !_heartbeatExpired) return;
    await reconnect();
  }

  /// Closes the socket and drains the queue before revoking the session:
  /// a revoked session makes the server send SubscriptionRevoked, and the core
  /// would then drop cached history and queued commands for every channel.
  Future<void> logout() async {
    _closed = true;
    _generation++;
    _retry?.cancel();
    _batch?.cancel();
    _resend?.cancel();
    _heartbeat?.cancel();
    final closing = _closeSocket();
    await _network?.cancel();
    await closing;
    _frames.clear();
    await _serial;
    try {
      await request('/auth/logout', body: {});
    } catch (_) {
      // The local session ends regardless.
    }
  }

  @override
  void dispose() {
    _http.close();
    _closed = true;
    _generation++;
    _retry?.cancel();
    _batch?.cancel();
    _resend?.cancel();
    _heartbeat?.cancel();
    _network?.cancel();
    unawaited(_closeSocket());
    _cache?.close();
    _core?.dispose();
    super.dispose();
  }
}
