import 'dart:async';
import 'dart:convert';

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_markdown_plus/flutter_markdown_plus.dart';

import 'controller.dart';
import 'm3_widgets.dart';
import 'rust_loader.dart';

Future<void> main() async {
  WidgetsFlutterBinding.ensureInitialized();
  await initializeRust();
  runApp(const RyzHubApp());
}

class RyzHubApp extends StatelessWidget {
  const RyzHubApp({super.key});
  @override
  Widget build(BuildContext context) => MaterialApp(
    title: 'RyzHub',
    debugShowCheckedModeBanner: false,
    theme: ThemeData(
      colorScheme: ColorScheme.fromSeed(seedColor: const Color(0xff256358)),
      useMaterial3: true,
      fontFamily: 'RyzHub Sans',
      fontFamilyFallback: const [
        'Microsoft YaHei',
        'Noto Sans CJK SC',
        'PingFang SC',
      ],
      scaffoldBackgroundColor: const Color(0xfffafbf9),
      inputDecorationTheme: const InputDecorationTheme(
        border: OutlineInputBorder(
          borderRadius: BorderRadius.all(Radius.circular(14)),
        ),
      ),
    ),
    home: const HubPage(),
  );
}

class HubPage extends StatefulWidget {
  const HubPage({super.key});
  @override
  State<HubPage> createState() => _HubPageState();
}

class _HubPageState extends State<HubPage> with WidgetsBindingObserver {
  final hub = HubController();
  @override
  void initState() {
    super.initState();
    WidgetsBinding.instance.addObserver(this);
    hub.addListener(_changed);
  }

  void _changed() {
    if (mounted) setState(() {});
  }

  @override
  void didChangeAppLifecycleState(AppLifecycleState state) {
    if (state == AppLifecycleState.resumed && hub.signedIn) {
      hub.resume();
    } else if (state == AppLifecycleState.paused) {
      hub.suspend();
    }
  }

  @override
  void dispose() {
    WidgetsBinding.instance.removeObserver(this);
    hub.dispose();
    super.dispose();
  }

  Future<void> action(Future<void> Function() task) async {
    try {
      await task();
    } catch (e) {
      if (mounted) {
        ScaffoldMessenger.of(context)
            .showSnackBar(SnackBar(content: Text('操作失败：$e')));
      }
    }
  }

  Widget sidebar() => Material(
    color: const Color(0xffeef2ed),
    child: SafeArea(
      child: Padding(
        padding: const EdgeInsets.all(20),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.stretch,
          children: [
            const Row(
              children: [
                Icon(Icons.hub_outlined),
                SizedBox(width: 10),
                Text(
                  'RyzHub',
                  style: TextStyle(fontSize: 23, fontWeight: FontWeight.w700),
                ),
              ],
            ),
            const SizedBox(height: 6),
            const Text('个人空间', style: TextStyle(color: Color(0xff68746c))),
            const SizedBox(height: 30),
            FilledButton.icon(
              onPressed: () => action(hub.createConversation),
              icon: const Icon(Icons.add),
              label: const Text('新建对话'),
            ),
            const SizedBox(height: 14),
            SegmentedButton<String>(
              segments: const [
                ButtonSegment(value: 'chat', label: Text('Chat')),
                ButtonSegment(value: 'work', label: Text('Work')),
              ],
              selected: {hub.mode},
              onSelectionChanged: (value) =>
                  setState(() => hub.mode = value.first),
            ),
            const SizedBox(height: 10),
            DropdownButtonFormField<String>(
              key: ValueKey('path:${hub.mode}:${hub.harness}:${hub.protocol}'),
              isExpanded: true,
              initialValue: hub.mode == 'work' ? hub.harness : hub.protocol,
              decoration: const InputDecoration(
                labelText: '执行路径',
                isDense: true,
              ),
              items:
                  (hub.mode == 'work'
                          ? <String, String>{
                              'codex': 'Codex',
                              if ((hub.capabilities['harnesses'] as List? ?? [])
                                  .contains('claude'))
                                'claude': 'Claude Code',
                            }
                          : <String, String>{
                              'anthropic': 'Anthropic Messages',
                              if ((hub.capabilities['responses_model'] ?? '')
                                  .toString()
                                  .isNotEmpty)
                                'responses': 'OpenAI Responses',
                            })
                      .entries
                      .map(
                        (e) => DropdownMenuItem(
                          value: e.key,
                          child: Text(e.value),
                        ),
                      )
                      .toList(),
              onChanged: (v) => setState(() {
                if (hub.mode == 'work') {
                  hub.harness = v!;
                } else {
                  hub.protocol = v!;
                }
              }),
            ),
            const SizedBox(height: 10),
            DropdownButtonFormField<String>(
              key: ValueKey('project:${hub.project}'),
              initialValue: hub.project ?? '',
              isExpanded: true,
              decoration: const InputDecoration(
                labelText: '新对话所属项目',
                isDense: true,
              ),
              items: [
                const DropdownMenuItem(value: '', child: Text('无项目')),
                for (final p in hub.projects.where(
                  (p) => p['state'] == 'active',
                ))
                  DropdownMenuItem(
                    value: p['id'] as String,
                    child: Text(p['name']),
                  ),
              ],
              onChanged: (v) => setState(() {
                hub.project = v!.isEmpty ? null : v;
                final p = hub.projects.where((p) => p['id'] == v).firstOrNull;
                if (p != null) {
                  final d = p['defaults'];
                  hub.mode = d['mode'] ?? 'chat';
                  hub.harness = d['harness'] == 'claude' ? 'claude' : 'codex';
                  hub.protocol = d['protocol'] ?? 'anthropic';
                }
              }),
            ),
            TextButton.icon(
              onPressed: () => showDialog<void>(
                context: context,
                builder: (_) => ProjectsDialog(hub: hub),
              ),
              icon: const Icon(Icons.folder_outlined),
              label: const Text('管理项目'),
            ),
            const SizedBox(height: 10),
            const Text(
              '最近对话',
              style: TextStyle(fontSize: 12, color: Color(0xff68746c)),
            ),
            const SizedBox(height: 10),
            Expanded(
              child: ListView.builder(
                itemCount: hub.conversations.length,
                itemBuilder: (context, index) {
                  final c = hub.conversations[index];
                  return Padding(
                    padding: const EdgeInsets.only(bottom: 5),
                    child: ListTile(
                      dense: true,
                      shape: RoundedRectangleBorder(
                        borderRadius: BorderRadius.circular(12),
                      ),
                      selected: c['id'] == hub.selected,
                      selectedTileColor: const Color(0xffdce8df),
                      leading: Icon(
                        c['mode'] == 'work'
                            ? Icons.terminal
                            : Icons.chat_bubble_outline,
                        size: 18,
                      ),
                      title: Text(
                        c['title'],
                        maxLines: 1,
                        overflow: TextOverflow.ellipsis,
                      ),
                      onTap: () => action(() => hub.select(c['id'])),
                    ),
                  );
                },
              ),
            ),
            const Divider(),
            Text(
              hub.realWork
                  ? '${hub.capabilities['milestone']} · ${hub.agentLabel}'
                  : hub.realChat
                  ? '${hub.capabilities['milestone']} · ${hub.agentLabel}'
                  : 'M0 · 模拟 Agent',
              style: const TextStyle(fontSize: 12),
            ),
            const SizedBox(height: 6),
            Text(
              hub.realWork
                  ? '工作区可保存检查点，并在归档后恢复。'
                  : hub.realChat
                  ? '执行中发送可补充指令；旁问不改变主线。'
                  : '试试 /approve，体验多设备审批。',
              style: const TextStyle(fontSize: 12, color: Color(0xff68746c)),
            ),
          ],
        ),
      ),
    ),
  );
  @override
  Widget build(BuildContext context) {
    if (!hub.signedIn) return LoginPage(onLogin: hub.login);
    final wide = MediaQuery.sizeOf(context).width >= 840;
    final items = hub.items;
    return Scaffold(
      drawer: wide ? null : Drawer(child: sidebar()),
      body: Row(
        children: [
          if (wide) SizedBox(width: 272, child: sidebar()),
          Expanded(
            child: Column(
              children: [
                AppBar(
                  backgroundColor: Colors.transparent,
                  elevation: 0,
                  title: Text(
                    hub.conversations
                            .where((c) => c['id'] == hub.selected)
                            .firstOrNull?['title'] ??
                        '对话',
                    style: const TextStyle(fontSize: 17),
                  ),
                  actions: [
                    Chip(
                      avatar: Icon(
                        Icons.circle,
                        size: 9,
                        color: hub.connection == '已连接'
                            ? Colors.green
                            : Colors.orange,
                      ),
                      label: Text(
                        hub.connection,
                        style: const TextStyle(fontSize: 12),
                      ),
                    ),
                    IconButton(
                      tooltip: '重新连接',
                      onPressed: hub.reconnect,
                      icon: const Icon(Icons.refresh),
                    ),
                    const SizedBox(width: 12),
                  ],
                ),
                if (hub.error.isNotEmpty)
                  MaterialBanner(
                    content: Text(
                      hub.error == 'ACTION_ALREADY_RESOLVED'
                          ? '该审批已由其他设备处理。'
                          : hub.error == 'WORKSPACE_RECOVERY_REQUIRED'
                          ? '工作区需要恢复，请先点击“恢复工作区”。'
                          : hub.error,
                    ),
                    actions: [
                      TextButton(
                        onPressed: () => setState(() => hub.error = ''),
                        child: const Text('关闭'),
                      ),
                    ],
                  ),
                Expanded(
                  child: items.isEmpty
                      ? const Center(
                          child: Column(
                            mainAxisSize: MainAxisSize.min,
                            children: [
                              Icon(
                                Icons.auto_awesome_outlined,
                                size: 38,
                                color: Color(0xff256358),
                              ),
                              SizedBox(height: 20),
                              Text(
                                '从一个想法开始',
                                style: TextStyle(
                                  fontSize: 26,
                                  fontWeight: FontWeight.w600,
                                ),
                              ),
                              SizedBox(height: 12),
                              Text(
                                '发送消息，或在另一台设备打开同一对话。',
                                style: TextStyle(color: Color(0xff68746c)),
                              ),
                            ],
                          ),
                        )
                      : ListView.builder(
                          padding: const EdgeInsets.symmetric(
                            horizontal: 24,
                            vertical: 16,
                          ),
                          itemCount: items.length + 1,
                          itemBuilder: (context, index) {
                            if (index == 0) {
                              return Center(
                                child: TextButton(
                                  onPressed: hub.hasOlder && !hub.historyLoading
                                      ? () => action(hub.loadOlder)
                                      : null,
                                  child: Text(
                                    hub.historyLoading
                                        ? '加载中…'
                                        : hub.hasOlder
                                        ? '加载更早消息'
                                        : '已显示全部消息',
                                  ),
                                ),
                              );
                            }
                            final item = items[index - 1];
                            return Align(
                              alignment: Alignment.topCenter,
                              child: ConstrainedBox(
                                constraints: const BoxConstraints(
                                  maxWidth: 820,
                                ),
                                child: MessageCard(
                                  key: ValueKey(item['id']),
                                  item: item,
                                  agentLabel: hub.agentLabel,
                                  onFork: hub.canFork(item)
                                      ? () => action(() => hub.fork(item['id']))
                                      : null,
                                ),
                              ),
                            );
                          },
                        ),
                ),
                for (final approval in hub.actions)
                  Padding(
                    padding: const EdgeInsets.symmetric(
                      horizontal: 24,
                      vertical: 8,
                    ),
                    child: (approval['options'] as List).contains('__answer__')
                        ? QuestionCard(
                            key: ValueKey(approval['id']),
                            summary: approval['summary'],
                            onAnswer: (answer) =>
                                action(() => hub.answer(approval, answer)),
                          )
                        : Card(
                            child: Padding(
                              padding: const EdgeInsets.all(16),
                              child: Row(
                                children: [
                                  const Icon(Icons.shield_outlined),
                                  const SizedBox(width: 12),
                                  Expanded(child: Text(approval['summary'])),
                                  TextButton(
                                    onPressed: () => action(
                                      () => hub.decide(approval, 'deny'),
                                    ),
                                    child: const Text('拒绝'),
                                  ),
                                  FilledButton(
                                    onPressed: () => action(
                                      () => hub.decide(approval, 'allow'),
                                    ),
                                    child: const Text('允许'),
                                  ),
                                ],
                              ),
                            ),
                          ),
                  ),
                if (hub.realWork)
                  Padding(
                    padding: const EdgeInsets.symmetric(
                      horizontal: 24,
                      vertical: 8,
                    ),
                    child: Wrap(
                      spacing: 8,
                      crossAxisAlignment: WrapCrossAlignment.center,
                      children: [
                        if (hub.workspace['recovery_required'] == true)
                          const Text('工作区恢复未完成，请重试恢复后继续。'),
                        if (hub.workspace['checkpoint'] != null)
                          Text(
                            '已保存检查点 · 水位 ${hub.workspace['checkpoint']['channel_seq']}',
                          ),
                        if (hub.workspaceBusy.isNotEmpty)
                          const Text('工作区任务进行中…'),
                        for (final operation in const {
                          'checkpoint': '保存检查点',
                          'archive': '归档工作区',
                          'restore': '恢复工作区',
                        }.entries)
                          OutlinedButton(
                            onPressed: hub.workspaceBusy.isNotEmpty
                                ? null
                                : () => action(
                                    () => hub.workspaceOperation(operation.key),
                                  ),
                            child: Text(operation.value),
                          ),
                        IconButton(
                          tooltip: '刷新工作区',
                          onPressed: () => action(hub.refreshWorkspace),
                          icon: const Icon(Icons.refresh),
                        ),
                      ],
                    ),
                  ),
                CommandBar(hub: hub, action: action),
                SafeArea(
                  top: false,
                  child: Padding(
                    padding: const EdgeInsets.fromLTRB(24, 8, 24, 20),
                    child: Center(
                      child: ConstrainedBox(
                        constraints: const BoxConstraints(maxWidth: 820),
                        child: Composer(
                          onSend: (text) => action(() => hub.send(text)),
                          running: hub.running,
                          onCancel: () => action(hub.cancel),
                          onSideQuery:
                              hub.realChat || hub.supports('side_query')
                              ? (text) => action(() => hub.sideQuery(text))
                              : null,
                          steer:
                              (hub.realChat || hub.realWork) &&
                              hub.mainlineRunning,
                        ),
                      ),
                    ),
                  ),
                ),
              ],
            ),
          ),
        ],
      ),
    );
  }
}

class LoginPage extends StatefulWidget {
  final Future<void> Function(String, String, String, bool) onLogin;
  const LoginPage({super.key, required this.onLogin});
  @override
  State<LoginPage> createState() => _LoginPageState();
}

class _LoginPageState extends State<LoginPage> {
  final server = TextEditingController(text: 'http://127.0.0.1:8080');
  final email = TextEditingController(), password = TextEditingController();
  bool register = true, busy = false;
  String error = '';
  @override
  void dispose() {
    server.dispose();
    email.dispose();
    password.dispose();
    super.dispose();
  }

  Future<void> submit() async {
    setState(() {
      busy = true;
      error = '';
    });
    try {
      await widget.onLogin(server.text, email.text, password.text, register);
    } catch (e) {
      if (mounted) setState(() => error = '$e');
    } finally {
      if (mounted) setState(() => busy = false);
    }
  }

  @override
  Widget build(BuildContext context) => Scaffold(
    body: Center(
      child: SingleChildScrollView(
        padding: const EdgeInsets.all(28),
        child: SizedBox(
          width: 400,
          child: Column(
            crossAxisAlignment: CrossAxisAlignment.stretch,
            children: [
              const Icon(
                Icons.hub_outlined,
                size: 45,
                color: Color(0xff256358),
              ),
              const SizedBox(height: 20),
              const Text(
                '欢迎来到 RyzHub',
                textAlign: TextAlign.center,
                style: TextStyle(fontSize: 29, fontWeight: FontWeight.w700),
              ),
              const SizedBox(height: 10),
              const Text('一个空间，连接你的想法与行动。', textAlign: TextAlign.center),
              const SizedBox(height: 36),
              TextField(
                controller: server,
                decoration: const InputDecoration(labelText: '服务器地址'),
              ),
              const SizedBox(height: 16),
              TextField(
                controller: email,
                keyboardType: TextInputType.emailAddress,
                decoration: const InputDecoration(labelText: '邮箱'),
              ),
              const SizedBox(height: 16),
              TextField(
                controller: password,
                obscureText: true,
                decoration: const InputDecoration(labelText: '密码（至少 8 位）'),
                onSubmitted: (_) => busy ? null : submit(),
              ),
              const SizedBox(height: 20),
              if (error.isNotEmpty)
                Padding(
                  padding: const EdgeInsets.only(bottom: 16),
                  child: Text(error, style: const TextStyle(color: Colors.red)),
                ),
              FilledButton(
                onPressed: busy ? null : submit,
                child: Padding(
                  padding: const EdgeInsets.all(12),
                  child: Text(
                    busy
                        ? '连接中…'
                        : register
                        ? '创建个人空间'
                        : '登录',
                  ),
                ),
              ),
              TextButton(
                onPressed: busy
                    ? null
                    : () => setState(() => register = !register),
                child: Text(register ? '已有账号？登录' : '创建新账号'),
              ),
            ],
          ),
        ),
      ),
    ),
  );
}

class Composer extends StatefulWidget {
  final Future<void> Function(String) onSend;
  final VoidCallback onCancel;
  final bool running;
  final bool steer;
  final Future<void> Function(String)? onSideQuery;
  const Composer({
    super.key,
    required this.onSend,
    required this.onCancel,
    required this.running,
    this.steer = false,
    this.onSideQuery,
  });
  @override
  State<Composer> createState() => _ComposerState();
}

class _ComposerState extends State<Composer> {
  final input = TextEditingController();
  bool sending = false;
  bool sideQuery = false;
  @override
  void dispose() {
    input.dispose();
    super.dispose();
  }

  Future<void> send() async {
    if (sending ||
        input.text.trim().isEmpty ||
        !input.value.composing.isCollapsed) {
      return;
    }
    setState(() => sending = true);
    try {
      await (sideQuery && widget.onSideQuery != null
          ? widget.onSideQuery!(input.text)
          : widget.onSend(input.text));
      input.clear();
    } finally {
      if (mounted) setState(() => sending = false);
    }
  }

  @override
  Widget build(BuildContext context) => CallbackShortcuts(
    bindings: {
      const SingleActivator(LogicalKeyboardKey.enter, control: true): send,
    },
    child: Column(
      crossAxisAlignment: CrossAxisAlignment.end,
      children: [
        if (widget.onSideQuery != null)
          Row(
            children: [
              FilterChip(
                label: const Text('只读旁问'),
                selected: sideQuery,
                onSelected: (value) => setState(() => sideQuery = value),
              ),
              const SizedBox(width: 12),
              if (widget.steer && !sideQuery)
                const Expanded(
                  child: Text(
                    '补充指令将在下一次模型请求前采用',
                    style: TextStyle(fontSize: 11),
                  ),
                ),
              if (widget.running)
                IconButton(
                  tooltip: '停止',
                  onPressed: widget.onCancel,
                  icon: const Icon(Icons.stop_circle_outlined),
                ),
            ],
          ),
        TextField(
          controller: input,
          minLines: 2,
          maxLines: 7,
          textInputAction: TextInputAction.newline,
          decoration: InputDecoration(
            hintText: '写下你的想法…',
            filled: true,
            fillColor: Colors.white,
            suffixIcon: widget.running && widget.onSideQuery == null
                ? IconButton(
                    tooltip: '停止',
                    onPressed: widget.onCancel,
                    icon: const Icon(Icons.stop_circle_outlined),
                  )
                : IconButton(
                    tooltip: '发送',
                    onPressed: sending ? null : send,
                    icon: const Icon(Icons.arrow_upward),
                  ),
          ),
        ),
        const SizedBox(height: 6),
        const Text(
          'Ctrl + Enter 发送 · Enter 换行',
          style: TextStyle(fontSize: 11, color: Color(0xff68746c)),
        ),
      ],
    ),
  );
}

class MessageCard extends StatelessWidget {
  final Map<String, dynamic> item;
  final String agentLabel;
  final VoidCallback? onFork;
  const MessageCard({
    super.key,
    required this.item,
    this.agentLabel = '模拟 Agent',
    this.onFork,
  });
  @override
  Widget build(BuildContext context) {
    final text = item['text'] as String;
    final user = item['kind'] == 'user';
    final blocks = <Widget>[];
    var cursor = 0;
    for (final code in RegExp(
      r'```([^\n]*)\n([\s\S]*?)(?:```|$)',
    ).allMatches(text)) {
      if (code.start > cursor) {
        blocks.add(
          MarkdownBody(
            data: text.substring(cursor, code.start),
            selectable: true,
          ),
        );
      }
      blocks.add(
        Container(
          margin: const EdgeInsets.symmetric(vertical: 8),
          padding: const EdgeInsets.all(12),
          decoration: BoxDecoration(
            color: const Color(0xffedf0ec),
            borderRadius: BorderRadius.circular(10),
          ),
          child: Column(
            crossAxisAlignment: CrossAxisAlignment.stretch,
            children: [
              Row(
                children: [
                  Expanded(
                    child: Text(
                      code.group(1)!,
                      style: const TextStyle(fontSize: 12),
                    ),
                  ),
                  IconButton(
                    tooltip: '复制代码',
                    icon: const Icon(Icons.copy, size: 16),
                    onPressed: () =>
                        Clipboard.setData(ClipboardData(text: code.group(2)!)),
                  ),
                ],
              ),
              SelectableText(
                code.group(2)!,
                style: const TextStyle(
                  fontFamily: 'monospace',
                  fontFamilyFallback: ['RyzHub Sans'],
                ),
              ),
            ],
          ),
        ),
      );
      cursor = code.end;
    }
    if (cursor < text.length) {
      blocks.add(MarkdownBody(data: text.substring(cursor), selectable: true));
    }
    return Padding(
      padding: const EdgeInsets.only(bottom: 22),
      child: Container(
        padding: const EdgeInsets.all(20),
        decoration: BoxDecoration(
          color: user ? const Color(0xffedf2ec) : Colors.white,
          borderRadius: BorderRadius.circular(16),
        ),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.stretch,
          children: [
            Row(
              children: [
                Icon(
                  user ? Icons.person_outline : Icons.auto_awesome,
                  size: 17,
                  color: const Color(0xff256358),
                ),
                const SizedBox(width: 8),
                Text(
                  user
                      ? '你'
                      : switch (item['kind']) {
                          'tool' => '工具',
                          'reasoning' => '思考',
                          'plan' => '计划',
                          'file_change' => '文件变更',
                          'steer' => '补充指令',
                          'side_query' => '只读旁问',
                          'notice' => '系统通知',
                          _ => agentLabel,
                        },
                  style: const TextStyle(fontWeight: FontWeight.w600),
                ),
                const Spacer(),
                if (onFork != null)
                  IconButton(
                    tooltip: '从此处分叉',
                    onPressed: onFork,
                    icon: const Icon(Icons.fork_right, size: 16),
                  ),
                IconButton(
                  tooltip: '复制消息',
                  onPressed: () => Clipboard.setData(ClipboardData(text: text)),
                  icon: const Icon(Icons.copy_outlined, size: 16),
                ),
              ],
            ),
            ...blocks,
            if (item['state'] == 'streaming')
              const Padding(
                padding: EdgeInsets.only(top: 8),
                child: Text(
                  '生成中…',
                  style: TextStyle(fontSize: 12, color: Color(0xff68746c)),
                ),
              ),
            if (item['state'] == 'invalidated')
              const Text(
                '已失效 · 此操作晚于恢复的检查点',
                style: TextStyle(color: Colors.deepOrange),
              ),
            if (item['state'] == 'cancelled')
              const Text(
                '已停止',
                style: TextStyle(fontSize: 12, color: Color(0xff68746c)),
              ),
            if (item['kind'] == 'steer')
              Text(switch (item['state']) {
                'injected' || 'handed' || 'consumed' => '已转交执行器',
                'waiting' => '等待注入点',
                'cancelled' => '任务已结束，此补充指令未采用',
                _ => '已接收，等待下一次模型请求',
              }, style: const TextStyle(fontSize: 12)),
          ],
        ),
      ),
    );
  }
}

class QuestionCard extends StatefulWidget {
  final String summary;
  final Future<void> Function(String) onAnswer;
  const QuestionCard({
    super.key,
    required this.summary,
    required this.onAnswer,
  });
  @override
  State<QuestionCard> createState() => _QuestionCardState();
}

class _QuestionCardState extends State<QuestionCard> {
  late List<Map<String, dynamic>> questions;
  late List<TextEditingController> inputs;
  bool busy = false;
  @override
  void initState() {
    super.initState();
    readQuestions();
  }

  void readQuestions() {
    questions = [
      {'question': widget.summary},
    ];
    try {
      final decoded = jsonDecode(widget.summary);
      if (decoded is List &&
          decoded.isNotEmpty &&
          decoded.every(
            (q) => q is Map && q['id'] is String && q['question'] is String,
          )) {
        questions = decoded
            .map((q) => Map<String, dynamic>.from(q as Map))
            .toList();
      }
    } on FormatException {
      // Chat questions are plain text; native harness questions are structured.
    }
    inputs = questions.map((_) => TextEditingController()).toList();
  }

  @override
  void didUpdateWidget(covariant QuestionCard oldWidget) {
    super.didUpdateWidget(oldWidget);
    if (oldWidget.summary != widget.summary) {
      for (final input in inputs) {
        input.dispose();
      }
      readQuestions();
    }
  }

  @override
  void dispose() {
    for (final input in inputs) {
      input.dispose();
    }
    super.dispose();
  }

  Future<void> submit() async {
    if (busy ||
        inputs.any(
          (input) =>
              input.text.trim().isEmpty || !input.value.composing.isCollapsed,
        )) {
      return;
    }
    setState(() => busy = true);
    try {
      await widget.onAnswer(
        inputs.length == 1
            ? inputs.single.text
            : jsonEncode({
                for (var i = 0; i < questions.length; i++)
                  questions[i]['id'] as String: inputs[i].text,
              }),
      );
    } finally {
      if (mounted) setState(() => busy = false);
    }
  }

  @override
  Widget build(BuildContext context) => Card(
    child: Padding(
      padding: const EdgeInsets.all(16),
      child: Column(
        mainAxisSize: MainAxisSize.min,
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          ConstrainedBox(
            constraints: BoxConstraints(
              maxHeight:
                  MediaQuery.sizeOf(context).height *
                  (MediaQuery.sizeOf(context).width < 600 ? .22 : .32),
            ),
            child: SingleChildScrollView(
              child: Column(
                crossAxisAlignment: CrossAxisAlignment.stretch,
                children: [
                  for (var i = 0; i < questions.length; i++) ...[
                    if (i > 0) const SizedBox(height: 16),
                    Text(questions[i]['question'] as String),
                    if (questions[i]['options'] is List)
                      Wrap(
                        spacing: 8,
                        children: [
                          for (final option in questions[i]['options'] as List)
                            if (option is Map && option['label'] is String)
                              Tooltip(
                                message: option['description'] is String
                                    ? option['description'] as String
                                    : '',
                                child: OutlinedButton(
                                  onPressed: busy
                                      ? null
                                      : () => inputs[i].text =
                                            option['label'] as String,
                                  child: Text(option['label'] as String),
                                ),
                              ),
                        ],
                      ),
                    const SizedBox(height: 12),
                    TextField(
                      controller: inputs[i],
                      enabled: !busy,
                      decoration: InputDecoration(
                        labelText: inputs.length == 1 ? '回答' : '回答 ${i + 1}',
                      ),
                    ),
                  ],
                ],
              ),
            ),
          ),
          const SizedBox(height: 8),
          Align(
            alignment: Alignment.centerRight,
            child: FilledButton(
              onPressed: busy ? null : submit,
              child: const Text('提交回答'),
            ),
          ),
        ],
      ),
    ),
  );
}
