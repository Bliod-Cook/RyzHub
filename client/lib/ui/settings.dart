import 'dart:convert';

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';

import '../browser.dart';
import '../controller.dart';
import '../l10n.dart';
import '../prefs.dart';
import '../theme.dart';
import 'common.dart';
import 'dialogs.dart';
import 'icons.dart';
import 'login.dart';
import 'model_picker.dart';
import 'memories.dart';
import 'plugins.dart';

class _Section {
  final String id, group;
  final IconData icon;
  final String Function() title;
  final List<String> keywords;
  const _Section(this.id, this.group, this.icon, this.title, this.keywords);
}

final _sections = [
  _Section(
    'memories',
    'personal',
    Icons.psychology_outlined,
    () => tr('记忆', 'Memory'),
    ['记忆', 'memory', '历史', 'history'],
  ),
  _Section(
    'general',
    'personal',
    Icons.settings_outlined,
    () => tr('通用', 'General'),
    ['语言', 'language', '发送', 'send', 'enter', '快捷键'],
  ),
  _Section(
    'appearance',
    'personal',
    Icons.light_mode_outlined,
    () => tr('外观', 'Appearance'),
    ['主题', 'theme', '深色', 'dark', '浅色', 'light', '强调色', 'accent', 'color'],
  ),
  _Section(
    'personalization',
    'personal',
    Icons.tune,
    () => tr('个性化', 'Personalization'),
    ['默认', 'default', '模式', 'mode', '执行路径', 'path', 'harness', 'protocol'],
  ),
  _Section(
    'keyboard',
    'personal',
    Icons.keyboard_outlined,
    () => tr('键盘快捷键', 'Keyboard shortcuts'),
    ['快捷键', 'shortcut', 'keyboard', 'ctrl'],
  ),
  _Section(
    'account',
    'account',
    Icons.person_outline,
    () => tr('账户', 'Account'),
    [
      '邮箱',
      'email',
      '组织',
      'organization',
      '会话',
      'session',
      '退出',
      'log out',
      'logout',
      '安全',
      'security',
    ],
  ),
  _Section('usage', 'account', Icons.data_usage, () => tr('用量', 'Usage'), [
    'token',
    '模型',
    'model',
    '用量',
    'usage',
  ]),
  _Section(
    'data',
    'account',
    Icons.inventory_2_outlined,
    () => tr('数据控制', 'Data controls'),
    ['归档', 'archive', '导出', 'export', '数据', 'data'],
  ),
  _Section(
    'plugins',
    'integrations',
    Icons.extension_outlined,
    () => tr('插件', 'Plugins'),
    ['插件', 'plugin', 'mcp', '工具', 'tool', 'skill', '技能', '命令', 'command'],
  ),
  _Section(
    'credentials',
    'integrations',
    Icons.key_outlined,
    () => tr('模型凭据', 'Model credentials'),
    ['api', 'key', '密钥', 'credential', 'anthropic', 'openai', 'byok'],
  ),
  _Section(
    'server',
    'integrations',
    Icons.dns_outlined,
    () => tr('服务器与策略', 'Server and policy'),
    [
      '服务器',
      'server',
      '连接',
      'connection',
      '策略',
      'policy',
      '能力',
      'capabilities',
      'agent',
    ],
  ),
];

String _groupTitle(String group) => switch (group) {
  'personal' => tr('个人', 'Personal'),
  'account' => tr('账户与数据', 'Account and data'),
  _ => tr('集成', 'Integrations'),
};

class SettingsPage extends StatefulWidget {
  final HubController hub;
  final HubActions app;
  final String? initial;
  const SettingsPage({
    super.key,
    required this.hub,
    required this.app,
    this.initial,
  });
  @override
  State<SettingsPage> createState() => _SettingsPageState();
}

class _SettingsPageState extends State<SettingsPage> {
  late String? section = widget.initial ?? (_narrow ? null : 'general');
  String query = '';
  bool navCollapsed = false;

  bool get _narrow =>
      WidgetsBinding
              .instance
              .platformDispatcher
              .views
              .first
              .physicalSize
              .width /
          WidgetsBinding
              .instance
              .platformDispatcher
              .views
              .first
              .devicePixelRatio <
      840;

  List<_Section> get matches {
    final q = query.trim().toLowerCase();
    if (q.isEmpty) return _sections;
    return _sections
        .where(
          (s) =>
              s.title().toLowerCase().contains(q) ||
              s.keywords.any((k) => k.toLowerCase().contains(q)),
        )
        .toList();
  }

  Widget nav(BuildContext context, {required bool narrow}) {
    final c = HubColors.of(context);
    final list = matches;
    return Material(
      color: c.sidebar,
      child: SafeArea(
        right: false,
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.stretch,
          children: [
            Padding(
              padding: const EdgeInsets.fromLTRB(
                Space.s8,
                Space.s8,
                Space.s8,
                Space.s4,
              ),
              child: Row(
                children: [
                  if (!narrow)
                    IconButton(
                      tooltip: tr('收起侧边栏', 'Collapse sidebar'),
                      icon: const HubIcon(HubIcons.sidebarExpanded),
                      onPressed: () => setState(() => navCollapsed = true),
                    ),
                ],
              ),
            ),
            NavRow(
              icon: Icons.arrow_back,
              label: tr('返回应用', 'Back to app'),
              onTap: () => Navigator.pop(context),
            ),
            Padding(
              padding: const EdgeInsets.fromLTRB(
                Space.s8,
                Space.s8,
                Space.s8,
                Space.s4,
              ),
              child: SizedBox(
                height: 38,
                child: TextField(
                  onChanged: (v) => setState(() => query = v),
                  style: const TextStyle(fontSize: 14),
                  decoration: InputDecoration(
                    hintText: tr('搜索', 'Search'),
                    prefixIcon: const Icon(Icons.search, size: 18),
                    prefixIconConstraints: const BoxConstraints(minWidth: 36),
                    filled: true,
                    fillColor: c.hover,
                    contentPadding: const EdgeInsets.symmetric(
                      vertical: Space.s8,
                    ),
                    border: OutlineInputBorder(
                      borderRadius: BorderRadius.circular(10),
                      borderSide: BorderSide.none,
                    ),
                    enabledBorder: OutlineInputBorder(
                      borderRadius: BorderRadius.circular(10),
                      borderSide: BorderSide.none,
                    ),
                    focusedBorder: OutlineInputBorder(
                      borderRadius: BorderRadius.circular(10),
                      borderSide: BorderSide(color: c.border),
                    ),
                  ),
                ),
              ),
            ),
            Expanded(
              child: ListView(
                padding: const EdgeInsets.only(bottom: Space.s16),
                children: [
                  for (final group in ['personal', 'account', 'integrations'])
                    if (list.any((s) => s.group == group)) ...[
                      SectionLabel(_groupTitle(group)),
                      for (final s in list.where((s) => s.group == group))
                        NavRow(
                          icon: s.icon,
                          label: s.title(),
                          selected: !narrow && section == s.id,
                          onTap: () => setState(() => section = s.id),
                        ),
                    ],
                  if (list.isEmpty)
                    EmptyHint(tr('没有匹配的设置', 'No matching settings')),
                ],
              ),
            ),
          ],
        ),
      ),
    );
  }

  Widget detail(BuildContext context, {required bool narrow}) {
    final c = HubColors.of(context);
    final s = _sections.firstWhere(
      (s) => s.id == section,
      orElse: () => _sections.first,
    );
    return SafeArea(
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          SizedBox(
            height: 52,
            child: Row(
              children: [
                const SizedBox(width: 8),
                if (narrow)
                  IconButton(
                    tooltip: tr('返回', 'Back'),
                    icon: const Icon(Icons.arrow_back),
                    onPressed: () => setState(() => section = null),
                  )
                else if (navCollapsed)
                  IconButton(
                    tooltip: tr('打开侧边栏', 'Open sidebar'),
                    icon: const HubIcon(HubIcons.sidebarCollapsed),
                    onPressed: () => setState(() => navCollapsed = false),
                  ),
              ],
            ),
          ),
          Expanded(
            child: SingleChildScrollView(
              padding: const EdgeInsets.fromLTRB(
                Space.s20,
                0,
                Space.s20,
                Space.s40,
              ),
              child: Center(
                child: ConstrainedBox(
                  constraints: const BoxConstraints(maxWidth: 600),
                  child: Column(
                    crossAxisAlignment: CrossAxisAlignment.stretch,
                    children: [
                      Semantics(
                        container: true,
                        header: true,
                        child: Text(
                          s.title(),
                          style: TextStyle(
                            fontSize: 24,
                            fontWeight: FontWeight.w500,
                            color: c.text,
                          ),
                        ),
                      ),
                      const SizedBox(height: 20),
                      KeyedSubtree(
                        key: ValueKey(s.id),
                        child: switch (s.id) {
                          'memories' => MemorySettings(hub: widget.hub),
                          'general' => const _General(),
                          'appearance' => const _Appearance(),
                          'personalization' => _Personalization(
                            hub: widget.hub,
                          ),
                          'keyboard' => const _Keyboard(),
                          'account' => _Account(
                            hub: widget.hub,
                            app: widget.app,
                          ),
                          'usage' => _Usage(hub: widget.hub),
                          'data' => _Data(hub: widget.hub, app: widget.app),
                          'credentials' => _Credentials(hub: widget.hub),
                          'plugins' => PluginSettings(hub: widget.hub),
                          _ => _Server(hub: widget.hub),
                        },
                      ),
                    ],
                  ),
                ),
              ),
            ),
          ),
        ],
      ),
    );
  }

  @override
  Widget build(BuildContext context) => ListenableBuilder(
    listenable: Listenable.merge([widget.hub, prefs]),
    builder: (context, _) {
      final c = HubColors.of(context);
      final narrow = MediaQuery.sizeOf(context).width < 840;
      return Scaffold(
        body: narrow
            ? section == null
                  ? nav(context, narrow: true)
                  : detail(context, narrow: true)
            : Row(
                children: [
                  if (!navCollapsed)
                    Container(
                      width: 260,
                      decoration: BoxDecoration(
                        border: Border(right: BorderSide(color: c.border)),
                      ),
                      child: nav(context, narrow: false),
                    ),
                  Expanded(child: detail(context, narrow: false)),
                ],
              ),
      );
    },
  );
}

class SettingsCard extends StatelessWidget {
  final List<Widget> children;
  final String? title;
  const SettingsCard({super.key, required this.children, this.title});
  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    return Padding(
      padding: const EdgeInsets.only(bottom: Space.s20),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          if (title != null)
            Padding(
              padding: const EdgeInsets.fromLTRB(
                Space.s4,
                0,
                Space.s4,
                Space.s8,
              ),
              child: Text(
                title!,
                style: TextStyle(fontSize: 13, color: c.subtle),
              ),
            ),
          Container(
            padding: const EdgeInsets.symmetric(horizontal: Space.s16),
            decoration: BoxDecoration(
              borderRadius: BorderRadius.circular(16),
              border: Border.all(color: c.border),
            ),
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.stretch,
              children: [
                for (final (i, child) in children.indexed) ...[
                  if (i > 0) Divider(color: c.border),
                  child,
                ],
              ],
            ),
          ),
        ],
      ),
    );
  }
}

class SettingRow extends StatelessWidget {
  final String title;
  final String? subtitle;
  final Widget? trailing;
  const SettingRow({
    super.key,
    required this.title,
    this.subtitle,
    this.trailing,
  });
  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    return Padding(
      padding: const EdgeInsets.symmetric(vertical: Space.s12),
      child: Row(
        children: [
          Expanded(
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                Text(title, style: TextStyle(fontSize: 14, color: c.text)),
                if (subtitle != null) ...[
                  const SizedBox(height: 2),
                  Text(
                    subtitle!,
                    style: TextStyle(fontSize: 12, color: c.subtle),
                  ),
                ],
              ],
            ),
          ),
          if (trailing != null) ...[const SizedBox(width: 16), trailing!],
        ],
      ),
    );
  }
}

/// A pill dropdown like the web settings' select.
class ChoicePill<T> extends StatelessWidget {
  final String tooltip;
  final T value;
  final List<(T, String)> options;
  final ValueChanged<T> onChanged;
  const ChoicePill({
    super.key,
    required this.tooltip,
    required this.value,
    required this.options,
    required this.onChanged,
  });
  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    final label =
        options.where((o) => o.$1 == value).firstOrNull?.$2 ?? '$value';
    return PopupMenuButton<T>(
      tooltip: tooltip,
      position: PopupMenuPosition.under,
      // Clear of the button and its highlight.
      offset: const Offset(0, 6),
      onSelected: onChanged,
      itemBuilder: (_) => [
        for (final (v, text) in options)
          HubMenuItem(
            value: v,
            height: 38,
            child: Row(
              children: [
                Expanded(child: Text(text)),
                if (v == value) ...[
                  const SizedBox(width: 12),
                  Icon(Icons.check, size: 16, color: c.text),
                ],
              ],
            ),
          ),
      ],
      child: Container(
        height: 34,
        padding: const EdgeInsets.only(left: Space.s14, right: Space.s8),
        decoration: BoxDecoration(
          borderRadius: BorderRadius.circular(17),
          border: Border.all(color: c.border),
        ),
        child: Row(
          mainAxisSize: MainAxisSize.min,
          children: [
            Text(label, style: TextStyle(fontSize: 13, color: c.text)),
            const SizedBox(width: 4),
            Icon(Icons.expand_more, size: 16, color: c.subtle),
          ],
        ),
      ),
    );
  }
}

class _Loader<T> extends StatefulWidget {
  final Future<T> Function() load;
  final Widget Function(BuildContext context, T value, VoidCallback reload)
  builder;
  const _Loader({super.key, required this.load, required this.builder});
  @override
  State<_Loader<T>> createState() => _LoaderState<T>();
}

class _LoaderState<T> extends State<_Loader<T>> {
  late Future<T> future = widget.load();
  void reload() => setState(() => future = widget.load());
  @override
  Widget build(BuildContext context) => FutureBuilder<T>(
    future: future,
    builder: (context, snap) {
      final c = HubColors.of(context);
      if (snap.hasError) {
        return SettingsCard(
          children: [
            SettingRow(
              title: tr('加载失败', 'Failed to load'),
              subtitle: errorText(snap.error!),
              trailing: OutlinedButton(
                onPressed: reload,
                child: Text(tr('重试', 'Retry')),
              ),
            ),
          ],
        );
      }
      if (!snap.hasData) {
        return Padding(
          padding: const EdgeInsets.all(Space.s24),
          child: Center(
            child: SizedBox(
              width: 18,
              height: 18,
              child: CircularProgressIndicator(strokeWidth: 2, color: c.subtle),
            ),
          ),
        );
      }
      return widget.builder(context, snap.data as T, reload);
    },
  );
}

class _General extends StatelessWidget {
  const _General();
  @override
  Widget build(BuildContext context) => SettingsCard(
    children: [
      SettingRow(
        title: tr('语言', 'Language'),
        subtitle: tr(
          '界面显示语言，仅保存在此设备',
          'Language for the app UI, saved on this device',
        ),
        trailing: ChoicePill<String>(
          tooltip: tr('语言', 'Language'),
          value: prefs.language,
          options: languageOptions(),
          onChanged: (v) => prefs.update((p) => p.language = v),
        ),
      ),
      SettingRow(
        title: tr('发送快捷键', 'Send shortcut'),
        subtitle: tr('Ctrl + Enter 始终可以发送', 'Ctrl + Enter always sends'),
        trailing: ChoicePill<bool>(
          tooltip: tr('发送快捷键', 'Send shortcut'),
          value: prefs.enterToSend,
          options: [(false, 'Ctrl + Enter'), (true, 'Enter')],
          onChanged: (v) => prefs.update((p) => p.enterToSend = v),
        ),
      ),
    ],
  );
}

class _Appearance extends StatelessWidget {
  const _Appearance();
  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    final names = {
      'orange': tr('橙色', 'Orange'),
      'blue': tr('蓝色', 'Blue'),
      'green': tr('绿色', 'Green'),
      'purple': tr('紫色', 'Purple'),
      'pink': tr('粉色', 'Pink'),
    };
    return SettingsCard(
      children: [
        SettingRow(
          title: tr('主题', 'Theme'),
          subtitle: tr(
            '跟随系统或固定为浅色、深色',
            'Follow the system, or always light or dark',
          ),
          trailing: ChoicePill<String>(
            tooltip: tr('主题', 'Theme'),
            value: prefs.theme,
            options: [
              ('system', tr('跟随系统', 'System')),
              ('light', tr('浅色', 'Light')),
              ('dark', tr('深色', 'Dark')),
            ],
            onChanged: (v) => prefs.update((p) => p.theme = v),
          ),
        ),
        SettingRow(
          title: tr('强调色', 'Accent color'),
          subtitle: names[prefs.accent],
          trailing: Wrap(
            spacing: 8,
            children: [
              for (final MapEntry(:key, :value) in accents.entries)
                Semantics(
                  button: true,
                  selected: prefs.accent == key,
                  label: names[key],
                  child: Tooltip(
                    message: names[key],
                    child: InkWell(
                      customBorder: const CircleBorder(),
                      onTap: () => prefs.update((p) => p.accent = key),
                      child: Container(
                        width: 26,
                        height: 26,
                        padding: const EdgeInsets.all(Space.s3),
                        decoration: BoxDecoration(
                          shape: BoxShape.circle,
                          border: Border.all(
                            color: prefs.accent == key
                                ? c.text
                                : Colors.transparent,
                            width: 1.5,
                          ),
                        ),
                        child: DecoratedBox(
                          decoration: BoxDecoration(
                            color: value,
                            shape: BoxShape.circle,
                          ),
                        ),
                      ),
                    ),
                  ),
                ),
            ],
          ),
        ),
      ],
    );
  }
}

class _Personalization extends StatelessWidget {
  final HubController hub;
  const _Personalization({required this.hub});

  Widget serverDefault() => Text(tr('服务器默认', 'Server default'));

  Widget chatModel() {
    final model = hub.modelEntry('chat', null, prefs.defaultChatModel) != null
        ? prefs.defaultChatModel
        : hub.defaultModel('chat');
    if (model == null) return serverDefault();
    return ModelPicker(
      mode: 'chat',
      chat: hub.chatModels,
      work: const [],
      model: model,
      onPick: (_, id, _) => prefs.update((p) => p.defaultChatModel = id),
    );
  }

  Widget workModel() {
    final harnesses = hub.availableHarnesses;
    final harness = harnesses.contains(prefs.defaultHarness)
        ? prefs.defaultHarness
        : harnesses.firstOrNull;
    final model =
        hub.modelEntry('work', harness, prefs.defaultWorkModel) != null
        ? prefs.defaultWorkModel
        : hub.defaultModel('work', harness);
    if (harness == null || model == null) return serverDefault();
    return ModelPicker(
      mode: 'work',
      chat: const [],
      work: hub.workGroups,
      harness: harness,
      model: model,
      onPick: (h, id, _) => prefs.update((p) {
        p.defaultHarness = h ?? p.defaultHarness;
        p.defaultWorkModel = id;
      }),
    );
  }

  @override
  Widget build(BuildContext context) => SettingsCard(
    title: tr('新对话默认设置', 'New chat defaults'),
    children: [
      SettingRow(
        title: tr('默认模式', 'Default mode'),
        subtitle: tr('点击“新建对话”时使用', 'Used when you start a new chat'),
        trailing: ChoicePill<String>(
          tooltip: tr('默认模式', 'Default mode'),
          value: prefs.defaultMode,
          options: const [('chat', 'Chat'), ('work', 'Work')],
          onChanged: (v) => prefs.update((p) => p.defaultMode = v),
        ),
      ),
      SettingRow(
        title: tr('默认 Chat 模型', 'Default Chat model'),
        subtitle: tr(
          '上游格式由服务端按模型决定',
          'The server picks the wire format for the model',
        ),
        trailing: chatModel(),
      ),
      SettingRow(
        title: tr('默认 Work 模型', 'Default Work model'),
        subtitle: tr('先选执行器，再选模型', 'Choose a harness, then its model'),
        trailing: workModel(),
      ),
    ],
  );
}

class _Keyboard extends StatelessWidget {
  const _Keyboard();
  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    Widget keys(List<String> list) => Wrap(
      spacing: 4,
      children: [
        for (final k in list)
          Container(
            padding: const EdgeInsets.symmetric(
              horizontal: Space.s8,
              vertical: Space.s3,
            ),
            decoration: BoxDecoration(
              color: c.bubble,
              borderRadius: BorderRadius.circular(6),
              border: Border.all(color: c.border),
            ),
            child: Text(k, style: TextStyle(fontSize: 12, color: c.muted)),
          ),
      ],
    );
    return SettingsCard(
      children: [
        for (final (label, list) in [
          (tr('新建对话', 'New chat'), ['Ctrl', 'Shift', 'O']),
          (tr('搜索对话', 'Search chats'), ['Ctrl', 'K']),
          (tr('收起或展开侧边栏', 'Toggle sidebar'), ['Ctrl', 'Shift', 'S']),
          (tr('打开设置', 'Open settings'), ['Ctrl', ',']),
          (
            tr('发送消息', 'Send message'),
            prefs.enterToSend ? ['Enter'] : ['Ctrl', 'Enter'],
          ),
          (
            tr('换行', 'New line'),
            prefs.enterToSend ? ['Shift', 'Enter'] : ['Enter'],
          ),
        ])
          SettingRow(title: label, trailing: keys(list)),
      ],
    );
  }
}

String _time(Object? value) {
  final t = value is num
      ? DateTime.fromMillisecondsSinceEpoch(value.toInt())
      : DateTime.tryParse('$value');
  if (t == null) return '—';
  final l = t.toLocal();
  String two(int v) => v.toString().padLeft(2, '0');
  return '${l.year}-${two(l.month)}-${two(l.day)} ${two(l.hour)}:${two(l.minute)}';
}

class _Account extends StatelessWidget {
  final HubController hub;
  final HubActions app;
  const _Account({required this.hub, required this.app});
  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    return _Loader<Map<String, dynamic>>(
      load: hub.me,
      builder: (context, me, _) => Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          SettingsCard(
            children: [
              SettingRow(
                title: tr('邮箱', 'Email'),
                trailing: Text(hub.email, style: TextStyle(color: c.muted)),
              ),
              SettingRow(
                title: tr('用户 ID', 'User ID'),
                trailing: SelectableText(
                  '${me['user_id']}',
                  style: TextStyle(fontSize: 12, color: c.muted),
                ),
              ),
              SettingRow(
                title: tr('组织', 'Organizations'),
                subtitle: [
                  for (final o in me['organizations'] as List? ?? [])
                    '${o['name']}${o['id'] == hub.org ? tr('（当前）', ' (current)') : ''}',
                ].join('\n'),
              ),
              if (me['is_instance_admin'] == true)
                SettingRow(
                  title: tr('实例管理员', 'Instance admin'),
                  trailing: Icon(Icons.verified_user_outlined, color: c.accent),
                ),
            ],
          ),
          SettingsCard(
            title: tr('安全与登录', 'Security and login'),
            children: [
              SettingRow(
                title: tr('服务器', 'Server'),
                trailing: Text(hub.base, style: TextStyle(color: c.muted)),
              ),
              SettingRow(
                title: tr('会话到期时间', 'Session expires'),
                subtitle: tr(
                  '令牌只保存在内存中，不写入磁盘',
                  'The token is kept in memory and never written to disk',
                ),
                trailing: Text(
                  hub.expiresAtMs == 0 ? '—' : _time(hub.expiresAtMs),
                  style: TextStyle(color: c.muted),
                ),
              ),
              SettingRow(
                title: tr('退出登录', 'Log out'),
                subtitle: tr('结束此设备上的会话', 'End the session on this device'),
                trailing: OutlinedButton(
                  style: OutlinedButton.styleFrom(foregroundColor: c.danger),
                  onPressed: app.logout,
                  child: Text(tr('退出登录', 'Log out')),
                ),
              ),
            ],
          ),
          _Identities(hub: hub),
        ],
      ),
    );
  }
}

/// The other accounts the member signs in with: connect more, remove those not needed.
class _Identities extends StatefulWidget {
  final HubController hub;
  const _Identities({required this.hub});
  @override
  State<_Identities> createState() => _IdentitiesState();
}

class _IdentitiesState extends State<_Identities> {
  Map<String, dynamic>? data;
  String error = '';
  HubController get hub => widget.hub;

  @override
  void initState() {
    super.initState();
    load();
  }

  Future<void> load() async {
    try {
      final loaded = await hub.request('/me/identities');
      if (mounted) setState(() => data = Map<String, dynamic>.from(loaded));
    } catch (e) {
      if (mounted) setState(() => error = errorText(e));
    }
  }

  Future<void> act(Future<void> Function() task) async {
    setState(() => error = '');
    try {
      await task();
    } catch (e) {
      if (mounted) setState(() => error = errorText(e));
    }
    await load();
  }

  /// Adds an account: the provider signs the member in and returns here.
  Future<void> connect(String provider) async {
    final ticket = await hub.request('/auth/ticket', body: {});
    final returned = await signInElsewhere(
      (returnTo) =>
          '${hub.base}/auth/oauth/$provider/start?return_to=${Uri.encodeQueryComponent(returnTo)}'
          '&link=${Uri.encodeQueryComponent('${ticket['ticket']}')}',
    );
    final failed = returned['login_error'];
    if (failed != null) throw Exception(failed);
  }

  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    final shown = data;
    final providers = (shown?['providers'] as List? ?? []).cast<Map>();
    final identities = (shown?['identities'] as List? ?? []).cast<Map>();
    if (shown == null || (providers.isEmpty && identities.isEmpty)) {
      return const SizedBox.shrink();
    }
    String label(String id) =>
        '${providers.where((p) => p['id'] == id).firstOrNull?['label'] ?? id}';
    return SettingsCard(
      title: tr('其他登录方式', 'Other ways to sign in'),
      children: [
        for (final identity in identities)
          SettingRow(
            title: label('${identity['provider']}'),
            subtitle: '${identity['email'] ?? identity['subject']}',
            trailing: TextButton(
              style: TextButton.styleFrom(foregroundColor: c.danger),
              onPressed: () => act(
                () => hub.request(
                  '/me/identities/${Uri.encodeComponent('${identity['provider']}')}/${Uri.encodeComponent('${identity['subject']}')}',
                  method: 'DELETE',
                ),
              ),
              child: Text(tr('移除', 'Remove')),
            ),
          ),
        for (final provider in providers)
          if (!identities.any((i) => i['provider'] == provider['id']))
            SettingRow(
              title: '${provider['label']}',
              trailing: OutlinedButton(
                onPressed: () => act(() => connect('${provider['id']}')),
                child: Text(tr('连接', 'Connect')),
              ),
            ),
        if (error.isNotEmpty)
          Padding(
            padding: const EdgeInsets.only(bottom: Space.s8),
            child: Text(error, style: TextStyle(color: c.danger)),
          ),
      ],
    );
  }
}

class _Usage extends StatelessWidget {
  final HubController hub;
  const _Usage({required this.hub});
  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    return _Loader<List<dynamic>>(
      load: () async => await hub.request('/orgs/${hub.org}/usage') as List,
      builder: (context, rows, reload) {
        num sum(String key) =>
            rows.fold<num>(0, (a, r) => a + ((r[key] as num?) ?? 0));
        return Column(
          crossAxisAlignment: CrossAxisAlignment.stretch,
          children: [
            SettingsCard(
              children: [
                SettingRow(
                  title: tr('最近请求', 'Recent requests'),
                  subtitle: tr('最多显示最近 100 条', 'Up to the latest 100'),
                  trailing: Text('${rows.length}'),
                ),
                SettingRow(
                  title: tr('输入 tokens', 'Input tokens'),
                  trailing: Text('${sum('input_tokens')}'),
                ),
                SettingRow(
                  title: tr('输出 tokens', 'Output tokens'),
                  trailing: Text('${sum('output_tokens')}'),
                ),
              ],
            ),
            SettingsCard(
              title: tr('明细', 'Details'),
              children: [
                if (rows.isEmpty)
                  SettingRow(title: tr('暂无用量记录', 'No usage yet')),
                for (final r in rows)
                  SettingRow(
                    title: '${r['model']}',
                    subtitle:
                        '${_time(r['created_at'])} · ${r['status']}${r['estimated'] == true ? tr(' · 估算', ' · estimated') : ''}',
                    trailing: Text(
                      '${r['input_tokens']} / ${r['output_tokens']}',
                      style: TextStyle(fontSize: 13, color: c.muted),
                    ),
                  ),
              ],
            ),
            Align(
              alignment: Alignment.centerLeft,
              child: TextButton.icon(
                onPressed: reload,
                icon: const Icon(Icons.refresh, size: 16),
                label: Text(tr('刷新', 'Refresh')),
              ),
            ),
          ],
        );
      },
    );
  }
}

class _Data extends StatelessWidget {
  final HubController hub;
  final HubActions app;
  const _Data({required this.hub, required this.app});
  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    final archived = hub.archived;
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        SettingsCard(
          title: tr('已归档对话', 'Archived chats'),
          children: [
            if (archived.isEmpty)
              SettingRow(title: tr('没有已归档的对话', 'No archived chats')),
            for (final x in archived)
              SettingRow(
                title: x['title'] ?? '',
                subtitle: x['mode'] == 'work'
                    ? tr(
                        'Work 对话：打开后通过“恢复工作区”恢复',
                        'Work chat: open it and restore the workspace',
                      )
                    : null,
                trailing: Wrap(
                  spacing: 6,
                  children: [
                    TextButton(
                      onPressed: () {
                        Navigator.pop(context);
                        app.openConversation(x['id']);
                      },
                      child: Text(tr('打开', 'Open')),
                    ),
                    if (x['mode'] != 'work')
                      OutlinedButton(
                        onPressed: () => app.run(() => hub.unarchive(x['id'])),
                        child: Text(tr('取消归档', 'Unarchive')),
                      ),
                  ],
                ),
              ),
          ],
        ),
        SettingsCard(
          children: [
            SettingRow(
              title: tr('导出当前对话', 'Export current chat'),
              subtitle: hub.isDraft
                  ? tr('先打开一个对话', 'Open a chat first')
                  : tr('以 JSON 复制到剪贴板', 'Copies JSON to the clipboard'),
              trailing: OutlinedButton(
                onPressed: hub.isDraft
                    ? null
                    : () => app.run(() async {
                        final data = await hub.exportConversation(hub.selected);
                        await Clipboard.setData(
                          ClipboardData(
                            text: const JsonEncoder.withIndent('  ')
                                .convert(data),
                          ),
                        );
                      }),
                child: Text(tr('导出', 'Export')),
              ),
            ),
            SettingRow(
              title: tr('本机缓存', 'Local cache'),
              subtitle: tr(
                '消息历史与离线队列按服务器、用户和组织分区缓存在此设备',
                'History and the offline queue are cached on this device per server, user and organization',
              ),
              trailing: Icon(Icons.offline_pin_outlined, color: c.subtle),
            ),
          ],
        ),
      ],
    );
  }
}

class _Credentials extends StatefulWidget {
  final HubController hub;
  const _Credentials({required this.hub});
  @override
  State<_Credentials> createState() => _CredentialsState();
}

class _CredentialsState extends State<_Credentials> {
  final name = TextEditingController(), key = TextEditingController();
  final loader = GlobalKey<_LoaderState<List<dynamic>>>();
  String provider = 'anthropic', error = '';
  bool organization = false, busy = false;
  HubController get hub => widget.hub;

  @override
  void dispose() {
    name.dispose();
    key.dispose();
    super.dispose();
  }

  Future<void> run(Future<void> Function() task) async {
    setState(() {
      busy = true;
      error = '';
    });
    try {
      await task();
      loader.currentState?.reload();
    } catch (e) {
      error = errorText(e);
    }
    if (mounted) setState(() => busy = false);
  }

  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        _Loader<List<dynamic>>(
          key: loader,
          load: () async =>
              await hub.request('/orgs/${hub.org}/credentials') as List,
          builder: (context, rows, _) => SettingsCard(
            children: [
              if (rows.isEmpty)
                SettingRow(
                  title: tr('还没有自带的模型凭据', 'No model credentials yet'),
                  subtitle: tr(
                    '未配置时使用平台凭据',
                    'Platform credentials are used by default',
                  ),
                ),
              for (final r in rows)
                SettingRow(
                  title: '${r['name']}',
                  subtitle:
                      '${r['provider']} · ${r['scope'] == 'organization' ? tr('组织', 'Organization') : tr('个人', 'Personal')}',
                  trailing: TextButton(
                    style: TextButton.styleFrom(foregroundColor: c.danger),
                    onPressed: busy
                        ? null
                        : () async {
                            if (await confirmDialog(
                              context,
                              title: tr('吊销凭据？', 'Revoke credential?'),
                              message: tr(
                                '“${r['name']}”将无法再用于模型请求。',
                                '"${r['name']}" can no longer be used for model requests.',
                              ),
                              confirm: tr('吊销', 'Revoke'),
                              danger: true,
                            )) {
                              await run(
                                () => hub.request(
                                  '/orgs/${hub.org}/credentials/${r['id']}',
                                  method: 'DELETE',
                                ),
                              );
                            }
                          },
                    child: Text(tr('吊销', 'Revoke')),
                  ),
                ),
            ],
          ),
        ),
        SettingsCard(
          title: tr('添加凭据', 'Add a credential'),
          children: [
            Padding(
              padding: const EdgeInsets.symmetric(vertical: Space.s14),
              child: Column(
                crossAxisAlignment: CrossAxisAlignment.stretch,
                children: [
                  TextField(
                    controller: name,
                    decoration: InputDecoration(labelText: tr('名称', 'Name')),
                  ),
                  const SizedBox(height: 10),
                  TextField(
                    controller: key,
                    obscureText: true,
                    style: obscuredStyle,
                    decoration: InputDecoration(
                      labelText: tr('API 密钥', 'API key'),
                    ),
                  ),
                  const SizedBox(height: 4),
                  SettingRow(
                    title: tr('提供方', 'Provider'),
                    trailing: ChoicePill<String>(
                      tooltip: tr('提供方', 'Provider'),
                      value: provider,
                      options: const [
                        ('anthropic', 'Anthropic'),
                        ('openai', 'OpenAI'),
                        ('gemini', 'Gemini'),
                      ],
                      onChanged: (v) => setState(() => provider = v),
                    ),
                  ),
                  SettingRow(
                    title: tr('组织共享', 'Share with organization'),
                    subtitle: tr('需要组织管理员权限', 'Requires organization admin'),
                    trailing: Switch(
                      value: organization,
                      onChanged: (v) => setState(() => organization = v),
                    ),
                  ),
                  if (error.isNotEmpty)
                    Padding(
                      padding: const EdgeInsets.only(bottom: Space.s8),
                      child: Text(error, style: TextStyle(color: c.danger)),
                    ),
                  Align(
                    alignment: Alignment.centerRight,
                    child: FilledButton(
                      onPressed: busy
                          ? null
                          : () => run(() async {
                              await hub.request(
                                '/orgs/${hub.org}/credentials',
                                body: {
                                  'name': name.text,
                                  'api_key': key.text,
                                  'provider': provider,
                                  'organization': organization,
                                },
                              );
                              name.clear();
                              key.clear();
                            }),
                      child: Text(tr('添加', 'Add')),
                    ),
                  ),
                ],
              ),
            ),
          ],
        ),
      ],
    );
  }
}

class _Server extends StatelessWidget {
  final HubController hub;
  const _Server({required this.hub});
  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    final caps = hub.capabilities;
    String show(Object? v) => switch (v) {
      null => '—',
      List l => l.isEmpty ? '—' : l.join(', '),
      Map m => jsonEncode(m),
      _ => '$v',
    };
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        SettingsCard(
          children: [
            SettingRow(
              title: tr('连接状态', 'Connection'),
              subtitle: connectionLabel(hub.connection),
              trailing: OutlinedButton.icon(
                onPressed: hub.reconnect,
                icon: const Icon(Icons.sync, size: 16),
                label: Text(tr('重连', 'Reconnect now')),
              ),
            ),
            SettingRow(
              title: tr('服务器地址', 'Server URL'),
              trailing: Text(hub.base, style: TextStyle(color: c.muted)),
            ),
            SettingRow(
              title: tr('当前 Agent', 'Current agent'),
              trailing: Text(
                hub.agentLabel,
                style: TextStyle(color: c.muted),
              ),
            ),
          ],
        ),
        SettingsCard(
          title: tr('能力', 'Capabilities'),
          children: [
            for (final key in ['chat', 'work'])
              SettingRow(
                title: key == 'chat' ? 'Chat' : 'Work',
                trailing: Text(
                  caps[key] == 'mock' ? tr('模拟', 'Mock') : tr('已启用', 'Enabled'),
                  style: TextStyle(fontSize: 13, color: c.muted),
                ),
              ),
          ],
        ),
        SettingsCard(
          title: tr(
            '可用模型（上游格式由服务端按模型决定）',
            'Models (the server picks each wire format)',
          ),
          children: [
            if (hub.chatModels.isEmpty && hub.workGroups.isEmpty)
              SettingRow(title: tr('服务器未提供模型目录', 'The server lists no models')),
            for (final (path, models) in [
              ('Chat', hub.chatModels),
              for (final g in hub.workGroups)
                (
                  pathLabel(g['harness']),
                  [
                    for (final m in g['models'] as List)
                      Map<String, dynamic>.from(m),
                  ],
                ),
            ])
              for (final m in models)
                SettingRow(
                  title: m['id'],
                  subtitle: '$path · ${providerLabel(m['provider'] ?? '')}',
                  trailing: m['default'] == true
                      ? Text(
                          tr('默认', 'Default'),
                          style: TextStyle(fontSize: 12, color: c.subtle),
                        )
                      : null,
                ),
          ],
        ),
        _Loader<dynamic>(
          load: () => hub.request('/orgs/${hub.org}/policy'),
          builder: (context, policy, _) => SettingsCard(
            title: tr('组织策略（只读）', 'Organization policy (read-only)'),
            children: [
              if (policy is! Map || policy.isEmpty)
                SettingRow(title: tr('没有额外限制', 'No extra restrictions')),
              if (policy is Map)
                for (final MapEntry(:key, :value) in policy.entries)
                  SettingRow(
                    title: '$key',
                    trailing: Flexible(
                      child: Text(
                        show(value),
                        textAlign: TextAlign.right,
                        style: TextStyle(fontSize: 13, color: c.muted),
                      ),
                    ),
                  ),
            ],
          ),
        ),
      ],
    );
  }
}
