import 'dart:async';
import 'dart:convert';
import 'dart:math';

import 'package:flutter/foundation.dart';
import 'package:http/http.dart' as http;
import 'package:connectivity_plus/connectivity_plus.dart';
import 'package:web_socket_channel/web_socket_channel.dart';

import 'rust/api.dart';
import 'storage/cache.dart';

/// Platform IO only. Protobuf, ordering, deduplication and projection live in Rust.
class HubController extends ChangeNotifier {
  String base = 'http://127.0.0.1:8080';
  String token = '', user = '', org = '', selected = '';
  String connection = '未连接', error = '';
  String mode = 'chat', harness = 'codex', protocol = 'anthropic';
  String? project;
  List<Map<String, dynamic>> projects = [];
  Map<String, dynamic> commandCatalog = {};
  Map<String, dynamic> get settings =>
      Map<String, dynamic>.from(commandCatalog['settings'] ?? {});
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
  String get channel => 'conv:$selected';
  bool get historyLoading => _historyLoading.contains(channel);
  bool get hasOlder => !_historyExhausted.contains(channel);

  Future<void> loadOlder() async {
    final name = channel;
    final current = items;
    if (_closed || current.isEmpty || !hasOlder || historyLoading) return;
    final version = _historyVersion[name] ?? 0;
    final core = _core!;
    _historyLoading.add(name);
    notifyListeners();
    try {
      final page = await request(
        '/orgs/$org/conversations/$selected/items?before_seq=${current.first['first_seq']}',
      ) as List;
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
        if (page.length < 100) _historyExhausted.add(name);
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
      ? (settings['protocol'] == 'responses'
            ? 'Chat · Responses'
            : 'Chat · Anthropic')
      : '模拟 Agent';
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
    notifyListeners();
  }

  Future<void> configure(Map<String, dynamic> patch) async {
    await _schedule(
      () => _core!.configure(channel: channel, configJson: jsonEncode(patch)),
    );
  }

  Future<void> compact() => _schedule(() => _core!.compact(channel: channel));
  Future<void> fork(String item) =>
      _schedule(() => _core!.fork(channel: channel, itemId: item));
  Future<void> moveProject(String? id) async {
    final result = await request(
      '/orgs/$org/conversations/$selected/project',
      body: {'project_id': id},
    );
    if (result['memory_notice'] != null) error = result['memory_notice'];
    await refreshConversations();
    notifyListeners();
  }

  Map<String, dynamic> workspace = {};
  String workspaceBusy = '';
  Future<void> refreshWorkspace() async {
    if (!realWork) return;
    workspace = Map<String, dynamic>.from(
      await request('/orgs/$org/conversations/$selected/workspace'),
    );
    notifyListeners();
  }

  Future<void> workspaceOperation(String kind) async {
    final conv = selected;
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
      if (job == null) throw Exception('工作区仍在保存，请稍后重试。');
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
      throw Exception('任务仍在后台运行，请刷新状态。');
    } finally {
      workspaceBusy = '';
      notifyListeners();
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
    if (conversations.isEmpty) {
      await createConversation();
    } else {
      await select(conversations.first['id']);
    }
    await _schedule(() => _core!.subscribe(channel: 'user:$org:$user'));
    await reconnect();
    _network = Connectivity().onConnectivityChanged.listen((_) {
      if (!_suspended) reconnect();
    });
    _resend = Timer.periodic(const Duration(seconds: 5), (_) {
      if (connection == '已连接' && !_suspended) {
        for (final frame in _core!.retryPending()) {
          _socket?.sink.add(frame);
        }
      }
    });
    _heartbeat = Timer.periodic(const Duration(seconds: 5), (_) {
      if (connection == '已连接' &&
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

  Future<void> createConversation() async {
    final created = await request(
      '/orgs/$org/conversations',
      body: {
        'title': '新对话 ${conversations.length + 1}',
        'mode': mode,
        'project_id': project,
        'run_config': {
          'harness': mode == 'work' ? harness : 'chat',
          'protocol': protocol,
        },
      },
    );
    conversations.removeWhere((c) => c['id'] == created['id']);
    conversations.insert(0, Map<String, dynamic>.from(created));
    await select(created['id']);
  }

  Future<void> select(String id) async {
    selected = id;
    workspace = {};
    commandCatalog = {};
    await refreshCommands();
    await _schedule(() => _core!.subscribe(channel: channel));
    if (realWork) await refreshWorkspace();
    notifyListeners();
  }

  Future<void> send(String text) async {
    if (text.trim().isEmpty) return;
    await _schedule(
      () => (realChat || realWork) && mainlineRunning
          ? _core!.steer(channel: channel, text: text)
          : _core!.sendText(channel: channel, text: text),
    );
  }

  Future<void> sideQuery(String text) =>
      _schedule(() => _core!.sideQuery(channel: channel, text: text));
  Future<void> answer(Map<String, dynamic> action, String text) => _schedule(
    () => _core!.answerQuestion(
      channel: channel,
      actionId: action['id'],
      runEpoch: action['run_epoch'],
      answer: text,
    ),
  );

  Future<void> decide(Map<String, dynamic> action, String decision) =>
      _schedule(
        () => _core!.decide(
          channel: channel,
          actionId: action['id'],
          runEpoch: action['run_epoch'],
          decision: decision,
        ),
      );
  Future<void> cancel() => _schedule(() => _core!.cancel(channel: channel));

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
    if (update.error != null) error = update.error!;
    if (connection == '已连接') {
      for (final bytes in update.outboundFrames) {
        _socket?.sink.add(bytes);
      }
    }
    notifyListeners();
  }

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
    connection = '连接中';
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
      connection = '已连接';
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
          error = '$e';
          _disconnected(generation);
        },
        onDone: () => _disconnected(generation),
      );
    } catch (e) {
      error = '$e';
      _disconnected(generation);
    } finally {
      _connecting = false;
      notifyListeners();
    }
  }

  void _disconnected(int generation) {
    if (_closed || _suspended || generation != _generation) return;
    connection = '重连中';
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
    connection = '后台';
    notifyListeners();
  }

  Future<void> resume() async {
    _suspended = false;
    await reconnect();
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
