import 'dart:async';
import 'dart:convert';
import 'dart:math';

import 'package:flutter/foundation.dart';
import 'package:http/http.dart' as http;
import 'package:connectivity_plus/connectivity_plus.dart';
import 'package:web_socket_channel/web_socket_channel.dart';

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

/// Platform IO only. Protobuf, ordering, deduplication and projection live in Rust.
class HubController extends ChangeNotifier {
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
  bool _closed = false, _connecting = false;
  DateTime _lastFrame = DateTime.now();
  final String _device =
      'flutter-${DateTime.now().microsecondsSinceEpoch}-${Random().nextInt(1000000)}';

  bool get signedIn => token.isNotEmpty;
  bool get isDraft => selected.isEmpty;
  String get channel => 'conv:$selected';

  // Reserved: the server does not implement pin or delete yet. The instance
  // capabilities must advertise them before the UI offers either action.
  bool get canPin => capabilities['conversation_pin'] == true;
  bool get canDelete => capabilities['conversation_delete'] == true;
  bool get canTemporary => capabilities['conversation_temporary'] == true;

  List<Map<String, dynamic>> get pinned => conversations
      .where((c) => c['pinned'] == true && c['state'] == 'active')
      .toList();
  List<Map<String, dynamic>> get recents => conversations
      .where(
        (c) =>
            c['state'] == 'active' &&
            c['pinned'] != true &&
            c['temporary'] != true &&
            c['project_id'] == null,
      )
      .toList();
  List<Map<String, dynamic>> get archived =>
      conversations.where((c) => c['state'] == 'archived').toList();
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
  bool get hasOlder => !_historyExhausted.contains(channel);
  bool get historyFailed => _historyFailed.contains(channel);

  Future<void> loadOlder() async {
    final name = channel;
    final current = items;
    if (_closed || current.isEmpty || !hasOlder || historyLoading) return;
    final version = _historyVersion[name] ?? 0;
    final core = _core!;
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
      (_items[channel]?.values.toList() ?? [])..sort(
        (a, b) => (a['first_seq'] as num).compareTo(b['first_seq'] as num),
      );
  List<Map<String, dynamic>> get actions =>
      (_actions[channel] ?? []).where((a) => a['status'] == 'pending').toList();
  bool get running => (_runs[channel] ?? []).any(
    (r) => r['state'] == 'running' || r['state'] == 'requires_action',
  );
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

  /// Reserved contract: `PATCH .../settings {pinned}`.
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
  Future<void> deleteConversation(String conv) async {
    if (!canDelete) {
      throw Exception(tr('服务器暂不支持删除', 'Deleting is not available'));
    }
    await request('/orgs/$org/conversations/$conv', method: 'DELETE');
    await refreshConversations();
    if (conv == selected) startDraft();
  }

  Future<Map<String, dynamic>> me() async =>
      Map<String, dynamic>.from(await request('/me'));

  Map<String, dynamic> workspace = {};
  String workspaceBusy = '';
  Future<void> refreshWorkspace() async {
    if (!realWork) return;
    workspace = Map<String, dynamic>.from(
      await request('/orgs/$org/conversations/$selected/workspace'),
    );
    notifyListeners();
  }

  Future<void> workspaceOperation(String kind, {String? conv}) async {
    conv ??= selected;
    workspaceBusy = kind;
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
      workspaceBusy = '';
      notifyListeners();
      // Workspace archive/restore changes the conversation state without a
      // user-channel event.
      if (!_closed) unawaited(refreshConversations().catchError((_) {}));
    }
  }

  Future<dynamic> request(
    String path, {
    Map<String, dynamic>? body,
    String? method,
  }) async {
    final headers = {
      'Content-Type': 'application/json',
      if (token.isNotEmpty) 'Authorization': 'Bearer $token',
    };
    final url = Uri.parse('$base$path');
    final response =
        await (method == 'PATCH'
                ? http.patch(url, headers: headers, body: jsonEncode(body))
                : method == 'DELETE'
                ? http.delete(url, headers: headers)
                : body == null
                ? http.get(url, headers: headers)
                : http.post(url, headers: headers, body: jsonEncode(body)))
            .timeout(const Duration(seconds: 15));
    final data = response.body.isEmpty ? null : jsonDecode(response.body);
    if (response.statusCode >= 400) {
      throw Exception(data['error'] ?? response.statusCode);
    }
    return data;
  }

  Future<void> login(
    String server,
    String email,
    String password,
    bool register,
  ) async {
    base = server.replaceAll(RegExp(r'/+$'), '');
    capabilities = Map<String, dynamic>.from(await request('/capabilities'));
    final result = await request(
      register ? '/auth/register' : '/auth/login',
      body: {'email': email, 'password': password},
    );
    token = result['token'];
    user = result['user_id'];
    org = result['org_id'];
    this.email = email.trim().toLowerCase();
    expiresAtMs = (result['expires_at_ms'] as num?)?.toInt() ?? 0;
    _cache = await LocalCache.open('$base:$user:$org');
    _core = ClientCore(cacheJson: await _cache!.load());
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
      if (connection == 'online' && !_suspended) {
        for (final frame in _core!.retryPending()) {
          _socket?.sink.add(frame);
        }
      }
    });
    _heartbeat = Timer.periodic(const Duration(seconds: 5), (_) {
      if (connection == 'online' &&
          DateTime.now().difference(_lastFrame).inSeconds > 55) {
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
    selected = '';
    workspace = {};
    commandCatalog = {};
    draftReasoning = null;
    temporary = false;
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

  Future<void> select(String id) async {
    final view = ++_view;
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

  Future<void> send(String text) async {
    if (text.trim().isEmpty) return;
    if (isDraft) return _createAndSend(text);
    final ch = channel, steerable = realChat || realWork;
    final pending = _pendingConfig[ch]?.isNotEmpty == true
        ? Map<String, dynamic>.of(_pendingConfig[ch]!)
        : null;
    await _schedule(
      () =>
          steerable &&
              (_runs[ch] ?? []).any(
                (r) =>
                    r['read_only'] != true &&
                    (r['state'] == 'running' ||
                        r['state'] == 'requires_action'),
              )
          ? _core!.steer(channel: ch, text: text)
          : _core!.sendText(
              channel: ch,
              text: text,
              configJson: pending == null ? null : jsonEncode(pending),
            ),
    );
  }

  Future<void> _createAndSend(String text) async {
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
            'title': conversationTitle(text),
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
      await _schedule(() => _core!.sendText(channel: ch, text: text));
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
    final task = _serial.then((_) => _apply(update()));
    _serial = task.catchError((Object e) {
      error = '$e';
      notifyListeners();
    });
    return task;
  }

  Future<void> _apply(Update update) async {
    // The caller is serialized. Write the command queue/cursor before any network send.
    if (update.cacheJson != null) await _cache!.save(update.cacheJson!);
    var needsConversationRefresh = false;
    var needsCommandRefresh = false;
    for (final diff in jsonDecode(update.changesJson) as List) {
      final name = diff['channel'] as String;
      if (diff['revoked'] == true || diff['reset'] == true) {
        _historyVersion[name] = (_historyVersion[name] ?? 0) + 1;
        _historyExhausted.remove(name);
      }
      if (diff['revoked'] == true) {
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
            conversations[index]['title'] = item['text'];
            conversations[index]['state'] = item['state'];
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
    if (needsCommandRefresh) unawaited(refreshCommands().catchError((_) {}));
    if (update.error != null) {
      error = update.error!;
      // A rejected setting must not ride along with every later message.
      if (_configErrors.contains(update.error)) _pendingConfig.clear();
    }
    if (connection == 'online') {
      for (final bytes in update.outboundFrames) {
        _socket?.sink.add(bytes);
      }
    }
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
    _batch = null;
    if (_frames.isEmpty || _closed) return;
    final batch = List<Uint8List>.of(_frames);
    _frames.clear();
    _serial = _serial
        .then((_) => _apply(_core!.ingest(frames: batch)))
        .catchError((Object e) {
          error = '$e';
          notifyListeners();
        });
  }

  Future<void> reconnect() async {
    if (_closed || _suspended || !signedIn || _connecting) return;
    _connecting = true;
    _retry?.cancel();
    final generation = ++_generation;
    await _subscription?.cancel();
    await _socket?.sink.close();
    _drain();
    await _serial;
    connection = 'connecting';
    notifyListeners();
    try {
      final ticket = await request('/auth/ticket', body: {});
      final uri = Uri.parse('$base/ws').replace(
        scheme: base.startsWith('https:') ? 'wss' : 'ws',
        queryParameters: {'ticket': ticket['ticket']},
      );
      final socket = WebSocketChannel.connect(
        uri,
        protocols: ['ryzhub.v1.protobuf'],
      );
      _socket = socket;
      await socket.ready;
      if (_closed || generation != _generation) {
        await socket.sink.close();
        return;
      }
      connection = 'online';
      _attempt = 0;
      _lastFrame = DateTime.now();
      socket.sink.add(_core!.hello(deviceId: _device));
      _subscription = socket.stream.listen(
        (dynamic message) {
          if (generation != _generation) return;
          _lastFrame = DateTime.now();
          _frames.add(Uint8List.fromList((message as List).cast<int>()));
          _batch ??= Timer(const Duration(milliseconds: 32), _drain);
        },
        onError: (Object e) {
          _connectionError(e);
          _disconnected(generation);
        },
        onDone: () => _disconnected(generation),
      );
    } catch (e) {
      _connectionError(e);
      _disconnected(generation);
    } finally {
      _connecting = false;
      notifyListeners();
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
    if (_closed || _suspended || generation != _generation) return;
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
    _suspended = true;
    _generation++;
    _retry?.cancel();
    _drain();
    await _serial;
    await _subscription?.cancel();
    await _socket?.sink.close();
    connection = 'background';
    notifyListeners();
  }

  Future<void> resume() async {
    _suspended = false;
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
    await _network?.cancel();
    await _subscription?.cancel();
    await _socket?.sink.close();
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
    _closed = true;
    _generation++;
    _retry?.cancel();
    _batch?.cancel();
    _resend?.cancel();
    _heartbeat?.cancel();
    _subscription?.cancel();
    _network?.cancel();
    _socket?.sink.close();
    _cache?.close();
    _core?.dispose();
    super.dispose();
  }
}
