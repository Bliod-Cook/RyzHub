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
import 'login.dart';
import 'model_picker.dart';
import 'memories.dart';
import 'plugins.dart';
import 'sectioned.dart';

final _sections = [
  PageSection(
    'memories',
    'personal',
    Icons.psychology_outlined,
    () => tr('记忆', 'Memory'),
    ['记忆', 'memory', '历史', 'history'],
  ),
  PageSection(
    'general',
    'personal',
    Icons.settings_outlined,
    () => tr('通用', 'General'),
    ['语言', 'language', '发送', 'send', 'enter', '快捷键'],
  ),
  PageSection(
    'appearance',
    'personal',
    Icons.light_mode_outlined,
    () => tr('外观', 'Appearance'),
    ['主题', 'theme', '深色', 'dark', '浅色', 'light', '强调色', 'accent', 'color'],
  ),
  PageSection(
    'personalization',
    'personal',
    Icons.tune,
    () => tr('个性化', 'Personalization'),
    ['默认', 'default', '模式', 'mode', '执行路径', 'path', 'harness'],
  ),
  PageSection(
    'keyboard',
    'personal',
    Icons.keyboard_outlined,
    () => tr('键盘快捷键', 'Keyboard shortcuts'),
    ['快捷键', 'shortcut', 'keyboard', 'ctrl'],
  ),
  PageSection(
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
  PageSection('usage', 'account', Icons.data_usage, () => tr('用量', 'Usage'), [
    'token',
    '模型',
    'model',
    '用量',
    'usage',
  ]),
  PageSection(
    'data',
    'account',
    Icons.inventory_2_outlined,
    () => tr('数据控制', 'Data controls'),
    ['归档', 'archive', '导出', 'export', '数据', 'data'],
  ),
  PageSection(
    'plugins',
    'integrations',
    Icons.extension_outlined,
    () => tr('插件', 'Plugins'),
    ['插件', 'plugin', 'mcp', '工具', 'tool', 'skill', '技能', '命令', 'command'],
  ),
  PageSection(
    'credentials',
    'integrations',
    Icons.key_outlined,
    () => tr('模型凭据', 'Model credentials'),
    ['api', 'key', '密钥', 'credential', 'anthropic', 'openai', 'byok'],
  ),
  PageSection(
    'server',
    'integrations',
    Icons.dns_outlined,
    () => tr('服务器', 'Server'),
    ['服务器', 'server', '连接', 'connection', '重连', 'reconnect'],
  ),
];

class SettingsPage extends StatelessWidget {
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
  Widget build(BuildContext context) => ListenableBuilder(
    listenable: Listenable.merge([hub, prefs]),
    builder: (context, _) => SectionedPage(
      back: tr('返回应用', 'Back to app'),
      initial: initial,
      home: 'general',
      groups: [
        ('personal', () => tr('个人', 'Personal')),
        ('account', () => tr('账户与数据', 'Account and data')),
        ('integrations', () => tr('集成', 'Integrations')),
      ],
      sections: _sections,
      content: (context, s) => switch (s.id) {
        'memories' => MemorySettings(hub: hub),
        'general' => const _General(),
        'appearance' => const _Appearance(),
        'personalization' => _Personalization(hub: hub),
        'keyboard' => const _Keyboard(),
        'account' => _Account(hub: hub, app: app),
        'usage' => _Usage(hub: hub),
        'data' => _Data(hub: hub, app: app),
        'credentials' => _Credentials(hub: hub),
        'plugins' => PluginSettings(hub: hub),
        _ => _Server(hub: hub),
      },
    ),
  );
}

class _General extends StatelessWidget {
  const _General();
  @override
  Widget build(BuildContext context) => SettingsCard(
    children: [
      SettingRow(
        title: tr('语言', 'Language'),
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
          trailing: Wrap(
            spacing: Space.gap,
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
                        padding: const EdgeInsets.all(3),
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
        trailing: ChoicePill<String>(
          tooltip: tr('默认模式', 'Default mode'),
          value: prefs.defaultMode,
          options: const [('chat', 'Chat'), ('work', 'Work')],
          onChanged: (v) => prefs.update((p) => p.defaultMode = v),
        ),
      ),
      SettingRow(
        title: tr('默认 Chat 模型', 'Default Chat model'),
        trailing: chatModel(),
      ),
      SettingRow(
        title: tr('默认 Work 模型', 'Default Work model'),
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
              horizontal: Space.gap,
              vertical: 3,
            ),
            decoration: BoxDecoration(
              color: c.bubble,
              borderRadius: BorderRadius.circular(Radii.small),
              border: Border.all(color: c.border),
            ),
            child: Text(
              k,
              style: TextStyle(fontSize: FontSizes.caption, color: c.muted),
            ),
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
    return Loader<Map<String, dynamic>>(
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
                  style: TextStyle(fontSize: FontSizes.caption, color: c.muted),
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
                trailing: Text(
                  hub.expiresAtMs == 0 ? '—' : _time(hub.expiresAtMs),
                  style: TextStyle(color: c.muted),
                ),
              ),
              SettingRow(
                title: tr('退出登录', 'Log out'),
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
  final loader = GlobalKey<LoaderState<dynamic>>();
  String error = '';
  HubController get hub => widget.hub;

  Future<void> act(Future<void> Function() task) async {
    setState(() => error = '');
    try {
      await task();
    } catch (e) {
      if (mounted) setState(() => error = errorText(e));
    }
    loader.currentState?.reload();
  }

  /// Adds an account: the provider signs the member in and returns here.
  Future<void> connect(String provider) async {
    final ticket = await hub.ticket();
    final returned = await signInElsewhere(
      (returnTo) =>
          '${hub.base}/auth/oauth/$provider/start?return_to=${Uri.encodeQueryComponent(returnTo)}'
          '&link=${Uri.encodeQueryComponent(ticket)}',
    );
    final failed = returned['login_error'];
    if (failed != null) throw Exception(failed);
  }

  @override
  Widget build(BuildContext context) => Loader<dynamic>(
    key: loader,
    load: hub.identities,
    builder: (context, data, _) => _identities(context, data as Map),
  );

  Widget _identities(BuildContext context, Map data) {
    final c = HubColors.of(context);
    final providers = (data['providers'] as List? ?? []).cast<Map>();
    final identities = (data['identities'] as List? ?? []).cast<Map>();
    if (providers.isEmpty && identities.isEmpty) {
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
                () => hub.removeIdentity(
                  '${identity['provider']}',
                  '${identity['subject']}',
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
            padding: const EdgeInsets.only(bottom: Space.gap),
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
    return Loader<List<dynamic>>(
      load: hub.usage,
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
                      style: TextStyle(
                        fontSize: FontSizes.label,
                        color: c.muted,
                      ),
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
  final loader = GlobalKey<LoaderState<List<dynamic>>>();
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
        Loader<List<dynamic>>(
          key: loader,
          load: hub.credentials,
          builder: (context, rows, _) => SettingsCard(
            children: [
              if (rows.isEmpty)
                SettingRow(title: tr('还没有自带的模型凭据', 'No model credentials yet')),
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
                              await run(() => hub.revokeCredential(r['id']));
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
              padding: const EdgeInsets.symmetric(vertical: Space.inset),
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
                    style: codeStyle,
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
                      padding: const EdgeInsets.only(bottom: Space.gap),
                      child: Text(error, style: TextStyle(color: c.danger)),
                    ),
                  Align(
                    alignment: Alignment.centerRight,
                    child: FilledButton(
                      onPressed: busy
                          ? null
                          : () => run(() async {
                              await hub.addCredential(
                                name: name.text,
                                apiKey: key.text,
                                provider: provider,
                                organization: organization,
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
    return SettingsCard(
      children: [
        SettingRow(
          title: tr('服务器地址', 'Server URL'),
          trailing: Text(hub.base, style: TextStyle(color: c.muted)),
        ),
        SettingRow(
          title: tr('连接状态', 'Connection'),
          subtitle: connectionLabel(hub.connection),
          trailing: OutlinedButton.icon(
            onPressed: hub.reconnect,
            icon: const Icon(Icons.sync, size: 16),
            label: Text(tr('重连', 'Reconnect')),
          ),
        ),
      ],
    );
  }
}
