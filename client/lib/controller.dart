import 'dart:async';
import 'dart:convert';
import 'dart:math';

import 'package:flutter/foundation.dart';
import 'package:http/http.dart' as http;
import 'package:connectivity_plus/connectivity_plus.dart';
import 'package:web_socket_channel/web_socket_channel.dart';

import 'browser.dart';
import 'hub_api.dart';
import 'l10n.dart';
import 'prefs.dart';
import 'push.dart';
import 'rust/api.dart';
import 'storage/cache.dart';
import 'transport/event_stream.dart';

export 'hub_api.dart' show byteChunks, contentHash, defaultServerUrl;

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

/// An item of a timeline as the kernel hands it over: the item, and its text rendered into
/// blocks and citation chips.
class TimelineItem {
  Map<String, dynamic> item;
  List<dynamic> blocks;
  List<dynamic> chips;
  TimelineItem(this.item, [this.blocks = const [], this.chips = const []]);
}

/// The client's clock for the kernel's batches.
final _clock = Stopwatch()..start();
double _now() => _clock.elapsedMicroseconds / 1000;

/// Platform IO only. Protobuf, ordering, deduplication and projection live in Rust.
class HubController extends ChangeNotifier with HubApi {
  HubController() : _connect = _openSocket;

  @visibleForTesting
  HubController.testing({
    required ClientCore this._core,
    required LocalCache this._cache,
    required this._connect,
  });

  final WebSocketChannel Function(Uri) _connect;

  /// Large frames from the server come compressed where it can; the kernel
  /// tells them apart.
  static WebSocketChannel _openSocket(Uri uri) => WebSocketChannel.connect(
    uri,
    protocols: ['ryzhub.v1.protobuf+zstd', 'ryzhub.v1.protobuf'],
  );

  /// WebSocket attempts that failed in a row: from [_fallbackAfter] on, the
  /// next connection is an event stream, until one fails in turn.
  int _socketFailures = 0;
  static const _fallbackAfter = 3;
  bool get _useEventStream => _socketFailures >= _fallbackAfter;

  /// How this client is connected: `websocket` or `event-stream`.
  String transport = 'websocket';

  String user = '', selected = '', email = '';
  int expiresAtMs = 0;

  /// online | connecting | retrying | background | offline
  String connection = 'offline', error = '';
  String mode = 'chat', harness = 'codex';
  String? project, draftModel, draftReasoning;

  /// `GET /orgs/{org}/models`: Chat models, and Work models grouped by harness.
  /// The wire protocol is the server's business; it follows each model.
  Map<String, dynamic> orgModels = {};

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

  /// The harness lacks [name] and the platform stands in for it.
  bool emulated(String name) =>
      commandCatalog['capabilities']?[name] == 'emulated';
  bool canFork(Map<String, dynamic> item) =>
      supports('fork') &&
      !running &&
      item['kind'] == 'assistant' &&
      item['state'] == 'completed' &&
      (item['run_id'] ?? '').toString().isNotEmpty &&
      (settings['harness'] != 'codex' ||
          (commandCatalog['fork_points'] as List? ?? []).contains(item['id']));
  final List<Map<String, dynamic>> conversations = [];

  /// Timelines of the conversations shown: their items by id, and their rows.
  final Map<String, Map<String, TimelineItem>> _timeline = {};
  final Map<String, List<Map<String, dynamic>>> _rows = {};
  final Map<String, List<Map<String, dynamic>>> _actions = {};
  final Map<String, List<Map<String, dynamic>>> _runs = {};
  final Set<String> _historyLoading = {}, _historyExhausted = {};
  final Set<String> _historyFailed = {};
  final Map<String, int> _historyVersion = {};
  ClientCore? _core;
  LocalCache? _cache;
  WebSocketChannel? _socket;
  StreamSubscription<dynamic>? _subscription;
  StreamSubscription<List<ConnectivityResult>>? _network;
  bool _suspended = false;
  Timer? _retry, _flush, _resend, _heartbeat;
  Future<void> _serial = Future.value();
  int _generation = 0, _attempt = 0;
  bool _closed = false;
  int? _connecting;
  DateTime _lastFrame = DateTime.now();
  Future<bool>? _renewing;

  /// Files picked for the next message.
  final List<PendingFile> pendingFiles = [];
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
  String get userChannel => 'user:$org:$user';

  /// Conversations being created by command, by the command's id.
  final Map<String, String> _creating = {};

  /// Commands someone waits on, by command id: completed with the result.
  final Map<String, Completer<Map<String, dynamic>>> _waiting = {};

  /// How far the user has read each conversation, from their user channel.
  final Map<String, int> _readStates = {};

  /// Each channel's sequence as last received.
  final Map<String, int> _cursors = {};

  /// Read positions this device has sent, so each is sent once.
  final Map<String, int> _marked = {};
  Timer? _markRead;

  /// Unsent messages by conversation, as the user's devices last synced them.
  final Map<String, Map<String, dynamic>> drafts = {};

  /// This device's id, to tell its own draft writes from other devices'.
  String get device => _device;

  /// The conversation has news since the user last read it.
  bool unread(Map<String, dynamic> conversation) {
    final id = conversation['id'];
    if (id == selected) return false;
    final activity = (conversation['activity_seq'] as num?)?.toInt() ?? 0;
    return activity > (_readStates[id] ?? 0);
  }

  /// The open conversation shows its newest messages: it is read up to them.
  /// Sent once the view has settled there.
  void sawLatest() {
    if (isDraft || searchFocusSeq != null) return;
    final ch = channel, conv = selected;
    bool behind() {
      final seq = _cursors[ch] ?? 0;
      return seq > (_readStates[conv] ?? 0) && seq > (_marked[conv] ?? 0);
    }

    if (!behind()) return;
    _markRead?.cancel();
    _markRead = Timer(const Duration(milliseconds: 800), () {
      if (_closed || selected != conv || !behind()) return;
      final seq = _cursors[ch]!;
      _marked[conv] = seq;
      _schedule(() => _core!.markRead(channel: ch, readSeq: seq));
    });
  }

  /// Saves [text] as [conv]'s draft for the user's other devices; empty
  /// clears it. Unchanged drafts are not sent again.
  Future<void> saveDraft(String conv, String text) async {
    if (conv.isEmpty) return;
    if ((drafts[conv]?['text'] ?? '') == text) return;
    drafts[conv] = {'text': text, 'device_id': _device};
    try {
      await request(
        '/orgs/$org/conversations/$conv/draft',
        method: 'PUT',
        body: {'text': text, 'device_id': _device},
      );
    } catch (_) {
      // Kept here; the next edit sends it again.
      drafts.remove(conv);
    }
  }

  /// Undoes a memory change, through the command queue: retried until the
  /// server answers, and undone once whatever retries it.
  Future<void> undoMemory(String eventId) async {
    final result = await _await(
      () => _core!.undoMemoryUpdate(channel: userChannel, eventId: eventId),
    );
    if (result['accepted'] != true) throw Exception(result['code']);
  }

  /// Queues a command and waits for the server's answer to it.
  Future<Map<String, dynamic>> _await(Update Function() command) async {
    final waiting = Completer<Map<String, dynamic>>();
    await _schedule(() {
      final update = command();
      final cid = update.cid;
      if (cid != null) _waiting[cid] = waiting;
      return update;
    });
    return waiting.future.timeout(
      const Duration(seconds: 30),
      onTimeout: () => throw Exception(
        tr('服务器暂未响应，稍后会重试', 'No answer yet; it will be retried'),
      ),
    );
  }

  // The instance capabilities must advertise each action before the UI offers it.
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
      final page = body['items'] as List, more = body['has_more'] == true;
      if (focused != null) {
        if (!_closed && view == _view && searchFocusSeq == focused) {
          _focus([
            ...?_searchItems,
            ...page.map((v) => Map<String, dynamic>.from(v)),
          ]);
          _searchHasMore = more;
        }
        return;
      }
      await _serially(() async {
        if (core != _core ||
            !_timeline.containsKey(name) ||
            (_historyVersion[name] ?? 0) != version) {
          return;
        }
        await _apply(
          core.mergeHistory(channel: name, itemsJson: jsonEncode(page)),
        );
        if (!more) _historyExhausted.add(name);
      });
    } finally {
      _historyLoading.remove(name);
      if (!_closed) notifyListeners();
    }
  }

  List<Map<String, dynamic>> get items =>
      (searchFocusSeq != null
            ? [...?_searchItems]
            : [
                for (final e in _timeline[channel]?.values ?? <TimelineItem>[])
                  e.item,
              ])
        ..sort(
          (a, b) => (a['first_seq'] as num).compareTo(b['first_seq'] as num),
        );

  /// The rows the conversation shows: messages, and the steps of a run folded together.
  List<Map<String, dynamic>> get rows =>
      searchFocusSeq != null ? _searchRows : (_rows[channel] ?? const []);

  /// What a subagent did for the call [id], in order.
  List<Map<String, dynamic>> childrenOf(String id) {
    final entries = searchFocusSeq != null
        ? _searchEntries.values
        : _timeline[channel]?.values ?? const <TimelineItem>[];
    return [
      for (final e in entries)
        if (e.item['parent_item_id'] == id) e.item,
    ]..sort((a, b) => (a['first_seq'] as num).compareTo(b['first_seq'] as num));
  }

  /// An item the rows name, with its rendered text.
  TimelineItem? entry(String id) =>
      searchFocusSeq != null ? _searchEntries[id] : _timeline[channel]?[id];
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
      ? pathLabel(settings['harness'] as String? ?? 'codex')
      : realChat
      ? 'Chat'
      : tr('模拟 Agent', 'Mock agent');
  Future<void> refreshModels() async {
    orgModels = Map<String, dynamic>.from(await request('/orgs/$org/models'));
    for (final g in workGroups) {
      if (g['label'] is String) harnessLabels[g['harness']] = g['label'];
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

  /// Delete a file everywhere: every conversation that carried it loses it,
  /// and so does the next message.
  @override
  Future<void> deleteFile(String id) async {
    await super.deleteFile(id);
    final before = pendingFiles.length;
    pendingFiles.removeWhere((f) => f.id == id);
    if (pendingFiles.length != before) notifyListeners();
  }

  /// Upload a file for the next message. Given [reuse], a file of the library
  /// with the same content is found first, and [reuse] decides whether the
  /// message takes that file instead.
  Future<void> attach(
    String name,
    int size,
    Stream<List<int>> Function() open, {
    Uint8List? thumbnail,
    Future<bool> Function(Map<String, dynamic> existing)? reuse,
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
        final existing = reuse == null ? null : await sameFile(open);
        if (existing != null &&
            pendingFiles.contains(file) &&
            await reuse!(existing)) {
          final at = pendingFiles.indexOf(file);
          if (at < 0) return;
          if (pendingFiles.any((f) => f.id == existing['id'])) {
            pendingFiles.removeAt(at);
          } else {
            pendingFiles[at] = _fromLibrary(existing, thumbnail: thumbnail);
          }
          notifyListeners();
          return;
        }
        // Removed while it was checked: nothing will send it.
        if (!pendingFiles.contains(file)) return;
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
      pendingFiles.add(_fromLibrary(file));
    }
    notifyListeners();
  }

  PendingFile _fromLibrary(Map<String, dynamic> file, {Uint8List? thumbnail}) =>
      PendingFile(
        name: file['name'] as String,
        size: (file['size'] as num).toInt(),
        thumbnail: thumbnail,
        fromLibrary: true,
        id: file['id'] as String,
        kind: file['kind'] as String?,
      );

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
  /// connection at expiry, and reconnecting renews or ends the session.
  Future<void> _carryOver() async {
    final generation = _generation, socket = _socket;
    if (await renew() &&
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
  /// requests that found theirs expired. False when the device is signed out;
  /// a server out of reach throws.
  @override
  Future<bool> renew() => _renewing ??= () async {
    try {
      final result = await request('/auth/refresh', body: {});
      if (_closed || result['user_id'] != user) return false;
      token = result['token'];
      expiresAtMs = (result['expires_at_ms'] as num?)?.toInt() ?? 0;
      return true;
    } catch (e) {
      if ('$e'.contains('UNAUTHORIZED')) return false;
      rethrow;
    } finally {
      _renewing = null;
    }
  }();

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

  /// Signs in with the code a sign-in with another account (OIDC, GitHub) came back with.
  Future<void> loginWithCode(String server, String code) async {
    base = sameSiteServer(server.replaceAll(RegExp(r'/+$'), ''));
    capabilities = Map<String, dynamic>.from(await request('/capabilities'));
    await _start(await request('/auth/oauth/exchange', body: {'code': code}));
  }

  /// Signs this device back in to [server] with its refresh cookie. False
  /// when it has none or it lapsed; a server out of reach throws.
  Future<bool> restore(String server) async {
    if (server.isEmpty) return false;
    base = sameSiteServer(server.replaceAll(RegExp(r'/+$'), ''));
    try {
      capabilities = Map<String, dynamic>.from(await request('/capabilities'));
      await _start(await request('/auth/refresh', body: {}));
      return true;
    } catch (e) {
      token = '';
      if ('$e'.contains('UNAUTHORIZED')) return false;
      rethrow;
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
    // A conversation's cached timeline shows when it opens.
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
    _resetSelection('');
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
      _focus([
        for (final v in body['items'] as List) Map<String, dynamic>.from(v),
      ]);
      _searchHasMore = body['has_more'] == true;
      notifyListeners();
    }
  }

  /// Shows conversation [id], or the draft when empty, in place of what was
  /// shown. Returns the new view's number: answers for an older one are
  /// dropped.
  int _resetSelection(String id) {
    searchFocusSeq = null;
    _focus(null);
    libraryOpen = false;
    previewFiles = null;
    selected = id;
    workspace = {};
    commandCatalog = {};
    return ++_view;
  }

  void clearSearchFocus() {
    searchFocusSeq = null;
    _focus(null);
    notifyListeners();
  }

  List<Map<String, dynamic>> _searchRows = const [];
  Map<String, TimelineItem> _searchEntries = const {};

  /// Shows [items], a stretch of the conversation apart from its live timeline.
  void _focus(List<Map<String, dynamic>>? items) {
    _searchItems = items;
    if (items == null) {
      _searchRows = const [];
      _searchEntries = const {};
      return;
    }
    final snapshot = jsonDecode(timelineSnapshot(itemsJson: jsonEncode(items)));
    _searchRows = [
      for (final row in snapshot['rows'] as List)
        Map<String, dynamic>.from(row),
    ];
    _searchEntries = {
      for (final change in snapshot['items'] as List)
        change['id'] as String: TimelineItem(
          Map<String, dynamic>.from(change['item']),
          change['blocks']?['blocks'] ?? const [],
          change['chips'] ?? const [],
        ),
    };
  }

  Future<void> select(String id) async {
    final view = _resetSelection(id);
    notifyListeners();
    // The serial queue keeps subscriptions in click order: the last one wins.
    await _schedule(() => _core!.subscribe(channel: 'conv:$id'));
    if (view != _view) return;
    await refreshCommands();
    if (view == _view && realWork) await refreshWorkspace();
    notifyListeners();
  }

  /// Counts the messages sent from here; the timeline shows each one.
  int sends = 0;

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
    sends++;
    notifyListeners();
    if (isDraft) return _createByCommand(text, carried, picked: picked);
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
      if (result['retryable'] != true) {
        _waiting.remove(cid)?.complete(Map<String, dynamic>.from(result));
        final created = _creating.remove(cid);
        // Refused, the conversation shown while it was created is not there.
        if (created != null && result['accepted'] != true) {
          conversations.removeWhere(
            (c) => c['id'] == created && c['creating'] == true,
          );
          if (selected == created) startDraft();
        }
      }
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

  /// Creates the conversation and sends its first message as one command,
  /// which waits in the queue while offline. The conversation shows at once.
  Future<void> _createByCommand(
    String text,
    List<Map<String, dynamic>> carried, {
    required bool picked,
  }) async {
    final files = [for (final f in carried) f['id'] as String];
    final reasoning = draftReasoningLevels.contains(draftReasoning)
        ? draftReasoning
        : null;
    final id = newConversationId(), ch = 'conv:$id';
    final title = conversationTitle(
      text.trim().isEmpty && picked && pendingFiles.isNotEmpty
          ? pendingFiles.first.name
          : text,
    );
    final settings = {
      if (mode == 'work') 'harness': harness,
      'model': ?draftModel,
      'reasoning': ?reasoning,
    };
    final temporary = this.temporary && canTemporary;
    // Shown until the server's list replaces it.
    conversations.insert(0, {
      'id': id,
      'title': title,
      'mode': mode,
      'state': 'active',
      'project_id': project,
      'temporary': temporary,
      'run_config': settings,
      'creating': true,
    });
    final projectId = project;
    _resetSelection(id);
    draftReasoning = null;
    this.temporary = false;
    notifyListeners();
    await _schedule(() {
      final update = _core!.createRun(
        channel: userChannel,
        conversationId: id,
        mode: mode,
        runConfigJson: jsonEncode(settings),
        projectId: projectId,
        temporary: temporary,
        title: title,
        text: text,
        attachmentIds: files,
      );
      final cid = update.cid;
      if (cid != null) _creating[cid] = id;
      return _remember(
        update,
        SentInput(channel: ch, text: text, files: carried),
      );
    });
    // Queued after the CreateRun, it is held until the conversation exists.
    await _schedule(() => _core!.subscribe(channel: ch));
    _sent(files);
  }

  /// Runs a plugin's command here: its prompt, filled in by the server, as a message.
  Future<void> invokeMenuCommand(
    String installation,
    String command,
    Map<String, dynamic> params,
  ) async {
    if (isDraft) return;
    final ch = channel;
    await _schedule(
      () => _core!.invokeMenuCommand(
        channel: ch,
        installationId: installation,
        command: command,
        paramsJson: jsonEncode(params),
        attachmentIds: const [],
      ),
    );
  }

  /// Calls a plugin tool for an MCP App or a declarative component of tool item `item`.
  Future<dynamic> pluginToolCall(
    String item,
    String tool,
    Map<String, dynamic> arguments,
  ) => request(
    '/orgs/$org/conversations/$selected/tool-calls',
    body: {'item_id': item, 'tool': tool, 'arguments': arguments},
    timeout: const Duration(minutes: 2),
  );

  /// Opens the MCP App of tool item `item`: its page, and the call's arguments
  /// and result.
  Future<dynamic> pluginApp(String item) => request(
    '/orgs/$org/conversations/$selected/apps',
    body: {'item_id': item},
    timeout: const Duration(seconds: 60),
  );

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

  /// Applies [update] once what is queued before it has applied, so kernel
  /// calls take effect in the order made.
  Future<void> _schedule(Update Function() update) =>
      _serially(() => _apply(update()));

  /// Runs [step] after the steps queued before it; a failure shows as the
  /// error.
  Future<void> _serially(Future<void> Function() step) {
    final task = _serial.then((_) async {
      if (!_closed) await step();
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
          _focus(null);
        }
        _timeline.remove(name);
        _rows.remove(name);
        _actions.remove(name);
        _runs.remove(name);
        continue;
      }
      final cursor = diff['cursor'];
      if (cursor is int) _cursors[name] = cursor;
      if (name == userChannel) {
        final reads = diff['read_states'];
        if (reads is Map) {
          for (final MapEntry(:key, :value) in reads.entries) {
            _readStates[key as String] = (value as num).toInt();
          }
        }
        final synced = diff['drafts'];
        if (synced is Map) {
          for (final MapEntry(:key, :value) in synced.entries) {
            drafts[key as String] = Map<String, dynamic>.from(value as Map);
          }
        }
      }
      final timeline = diff['timeline'];
      if (timeline is Map) {
        for (final item in _applyTimeline(name, timeline)) {
          if (name == channel && item['kind'] == 'notice') {
            needsCommandRefresh = true;
          }
        }
      }
      for (final item in diff['items'] as List? ?? const []) {
        if (name == 'user:$org:$user' && item['kind'] == 'conversation') {
          final index = conversations.indexWhere((c) => c['id'] == item['id']);
          if (index < 0) {
            // User-channel items carry a title, but not the conversation mode.
            needsConversationRefresh = true;
          } else {
            final conversation = conversations[index];
            conversation['title'] = item['text'];
            conversation['state'] = item['state'];
            final detail = item['detail']['kind']['Conversation'];
            conversation['pinned_at_ms'] = detail['pinned_at_ms'];
            conversation['activity_seq'] = detail['activity_seq'] ?? 0;
            conversation.remove('creating');
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
    if (update.authExpiring) unawaited(_carryOver().catchError((_) {}));
    _sendFrames(update.outboundFrames);
    // Changes wait in the kernel to cross with what follows them.
    final wait = update.flushInMs;
    if (wait != null && _flush == null) {
      _flush = Timer(Duration(milliseconds: wait), _drain);
    }
    notifyListeners();
  }

  /// Applies a timeline's diffs; returns the items that came whole.
  List<Map<String, dynamic>> _applyTimeline(String name, Map timeline) {
    final entries = _timeline.putIfAbsent(name, () => {});
    final rows = _rows.putIfAbsent(name, () => []);
    for (final op in timeline['rows'] as List) {
      switch (op['op']) {
        case 'reset':
          rows
            ..clear()
            ..addAll([
              for (final row in op['rows'] as List)
                Map<String, dynamic>.from(row),
            ]);
        case 'insert':
          rows.insert(op['index'], Map<String, dynamic>.from(op['row']));
        case 'set':
          rows[op['index']] = Map<String, dynamic>.from(op['row']);
        case 'remove':
          rows.removeAt(op['index']);
      }
    }
    for (final id in timeline['removed'] as List? ?? const []) {
      entries.remove(id);
    }
    final whole = <Map<String, dynamic>>[];
    for (final change in timeline['items'] as List) {
      final id = change['id'] as String;
      var entry = entries[id];
      final item = change['item'];
      if (item != null) {
        final map = Map<String, dynamic>.from(item);
        whole.add(map);
        if (entry == null) {
          entry = entries[id] = TimelineItem(map);
        } else {
          entry.item = map;
        }
      }
      if (entry == null) continue;
      final append = change['append'];
      if (append is String) {
        entry.item = {...entry.item, 'text': '${entry.item['text']}$append'};
      }
      final blocks = change['blocks'];
      if (blocks is Map) {
        final from = (blocks['from'] as num).toInt();
        // Blocks before `from` are unchanged: they keep their widgets.
        entry.blocks = [
          ...entry.blocks.take(from),
          ...blocks['blocks'] as List,
        ];
      }
      final chips = change['chips'];
      if (chips is List) entry.chips = chips;
      // A running command's output so far, shown with its call.
      final output = change['output'], more = change['output_append'];
      if (output is String || more is String) {
        final live = output is String
            ? output
            : '${entry.item['live_output'] ?? ''}$more';
        entry.item = {...entry.item, 'live_output': live};
      }
    }
    return whole;
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

  /// Hands over what waits in the kernel's batch now.
  void _drain() {
    _flush?.cancel();
    _flush = null;
    if (_closed || _core == null) return;
    unawaited(_schedule(() => _core!.flush(nowMs: _now())));
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
      final ticket = await this.ticket();
      if (!_current(generation)) return;
      final stream = _useEventStream;
      final WebSocketChannel socket;
      if (stream) {
        socket = EventStreamChannel(
          Uri.parse(
            '$base/stream',
          ).replace(queryParameters: {'ticket': ticket, 'device_id': _device}),
          post: postFrame,
        );
      } else {
        socket = _connect(
          Uri.parse('$base/ws').replace(
            scheme: base.startsWith('https:') ? 'wss' : 'ws',
            queryParameters: {'ticket': ticket},
          ),
        );
      }
      _socket = socket;
      try {
        await socket.ready;
      } catch (_) {
        // A failed event stream gives WebSockets another try.
        _socketFailures = stream ? 0 : _socketFailures + 1;
        rethrow;
      }
      if (!stream) _socketFailures = 0;
      transport = stream ? 'event-stream' : 'websocket';
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
          final frame = Uint8List.fromList((message as List).cast<int>());
          unawaited(
            _schedule(() => _core!.ingest(frames: [frame], nowMs: _now())),
          );
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
    _flush?.cancel();
    _resend?.cancel();
    _heartbeat?.cancel();
    final closing = _closeSocket();
    await _network?.cancel();
    await closing;
    await _serial;
    await Push.stop(this);
    try {
      await request('/auth/logout', body: {});
    } catch (_) {
      // The local session ends regardless.
    }
  }

  @override
  void dispose() {
    closeApi();
    _closed = true;
    _generation++;
    _retry?.cancel();
    _flush?.cancel();
    _markRead?.cancel();
    _resend?.cancel();
    _heartbeat?.cancel();
    _network?.cancel();
    unawaited(_closeSocket());
    _cache?.close();
    _core?.dispose();
    super.dispose();
  }
}
