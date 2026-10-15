import 'dart:convert';

import 'package:file_picker/file_picker.dart';
import 'package:flutter/material.dart';
import 'package:url_launcher/url_launcher.dart';

import '../controller.dart';
import '../l10n.dart';
import '../theme.dart';
import 'dialogs.dart';
import 'settings.dart';

String trustLabel(String? trust) => switch (trust) {
  'signed' => tr('已签名', 'Signed'),
  'official' => tr('官方审核', 'Reviewed'),
  _ => tr('未验证', 'Unverified'),
};

/// The plugin's trust tier, shown wherever it is installed or used.
class TrustBadge extends StatelessWidget {
  final String? trust;
  const TrustBadge(this.trust, {super.key});
  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    final color = switch (trust) {
      'signed' || 'official' => c.success,
      _ => c.warning,
    };
    return Container(
      padding: const EdgeInsets.symmetric(
        horizontal: Space.s6,
        vertical: Space.s1,
      ),
      decoration: BoxDecoration(
        border: Border.all(color: color),
        borderRadius: BorderRadius.circular(4),
      ),
      child: Text(
        trustLabel(trust),
        style: TextStyle(fontSize: 11, color: color),
      ),
    );
  }
}

/// Settings › Plugins: what the member has installed, and installing more.
class PluginSettings extends StatefulWidget {
  final HubController hub;
  const PluginSettings({super.key, required this.hub});
  @override
  State<PluginSettings> createState() => _PluginSettingsState();
}

class _PluginSettingsState extends State<PluginSettings> {
  List<dynamic> plugins = [];
  bool loading = true, enabled = true;
  String error = '';
  HubController get hub => widget.hub;

  @override
  void initState() {
    super.initState();
    load();
  }

  Future<void> load() async {
    try {
      final data = await hub.plugins();
      if (!mounted) return;
      setState(() {
        plugins = data['plugins'] as List;
        enabled = data['enabled'] != false;
        loading = false;
        error = '';
      });
    } catch (e) {
      if (mounted) {
        setState(() {
          loading = false;
          error = errorText(e);
        });
      }
    }
  }

  Future<void> act(Future<dynamic> Function() task) async {
    setState(() => error = '');
    try {
      await task();
    } catch (e) {
      if (mounted) setState(() => error = errorText(e));
    }
    await load();
  }

  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    if (loading) {
      return const Center(child: CircularProgressIndicator());
    }
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        if (error.isNotEmpty)
          Padding(
            padding: const EdgeInsets.only(bottom: Space.s12),
            child: SelectableText(error, style: TextStyle(color: c.danger)),
          ),
        SettingsCard(
          title: tr('已安装', 'Installed'),
          children: [
            if (plugins.isEmpty)
              SettingRow(
                title: tr('还没有插件', 'No plugins yet'),
                subtitle: tr(
                  '插件为对话带来外部工具、技能和命令',
                  'Plugins bring outside tools, skills and commands to conversations',
                ),
              ),
            for (final p in plugins)
              _PluginTile(
                plugin: Map<String, dynamic>.from(p),
                hub: hub,
                act: act,
              ),
          ],
        ),
        Align(
          alignment: Alignment.centerLeft,
          child: FilledButton.icon(
            icon: const Icon(Icons.add, size: 18),
            label: Text(tr('安装插件', 'Install a plugin')),
            onPressed: !enabled
                ? null
                : () async {
                    final installed = await showDialog<bool>(
                      context: context,
                      builder: (_) => InstallPluginDialog(hub: hub),
                    );
                    if (installed == true) await load();
                  },
          ),
        ),
      ],
    );
  }
}

class _PluginTile extends StatelessWidget {
  final Map<String, dynamic> plugin;
  final HubController hub;
  final Future<void> Function(Future<dynamic> Function()) act;
  const _PluginTile({
    required this.plugin,
    required this.hub,
    required this.act,
  });

  String get id => '${plugin['id']}';

  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    final tools = (plugin['tools'] as List? ?? []).cast<Map>();
    final waiting =
        plugin['state'] == 'needs_reconfirm' ||
        plugin['pending_version'] != null;
    final connected = (plugin['connected'] as List? ?? []).cast<String>();
    final servers = (plugin['servers'] as List? ?? []).cast<Map>();
    final settings = (plugin['user_config'] as Map? ?? {});
    return Theme(
      data: Theme.of(context).copyWith(dividerColor: Colors.transparent),
      child: ExpansionTile(
        tilePadding: EdgeInsets.zero,
        childrenPadding: const EdgeInsets.only(bottom: Space.s12),
        expandedCrossAxisAlignment: CrossAxisAlignment.start,
        title: Row(
          children: [
            Flexible(
              child: Text(
                '${plugin['name']}',
                overflow: TextOverflow.ellipsis,
                style: TextStyle(color: c.text),
              ),
            ),
            const SizedBox(width: 8),
            TrustBadge(plugin['trust'] as String?),
            if (plugin['blocked'] == true) ...[
              const SizedBox(width: 8),
              Text(
                tr('已被停用', 'Stopped by admin'),
                style: TextStyle(fontSize: 12, color: c.danger),
              ),
            ] else if (waiting) ...[
              const SizedBox(width: 8),
              Text(
                tr('有变更待确认', 'Changes to confirm'),
                style: TextStyle(fontSize: 12, color: c.warning),
              ),
            ],
          ],
        ),
        subtitle: Text(
          [
            if ('${plugin['version']}'.isNotEmpty) 'v${plugin['version']}',
            tr('${tools.length} 个工具', '${tools.length} tools'),
            if ('${plugin['description']}'.isNotEmpty)
              '${plugin['description']}',
          ].join(' · '),
          maxLines: 2,
          overflow: TextOverflow.ellipsis,
          style: TextStyle(fontSize: 12, color: c.muted),
        ),
        trailing: Switch(
          value: plugin['enabled'] == true,
          onChanged: (v) => act(() => hub.updatePlugin(id, {'enabled': v})),
        ),
        children: [
          for (final warning in (plugin['warnings'] as List? ?? []))
            Text(
              '⚠ $warning',
              style: TextStyle(fontSize: 12, color: c.warning),
            ),
          for (final tool in tools)
            Padding(
              padding: const EdgeInsets.symmetric(vertical: Space.s2),
              child: Text(
                [
                  '${tool['tool']}',
                  if (tool['read_only'] == true) tr('只读', 'read-only'),
                  if (tool['confirmed'] != true)
                    tool['new'] == true
                        ? tr('新工具，待确认', 'new, to confirm')
                        : tr('描述已变更，待确认', 'description changed, to confirm'),
                ].join(' · '),
                style: TextStyle(
                  fontSize: 13,
                  color: tool['confirmed'] == true ? c.text : c.warning,
                ),
              ),
            ),
          const SizedBox(height: 8),
          Wrap(
            spacing: 8,
            runSpacing: 8,
            children: [
              if (waiting)
                FilledButton(
                  onPressed: () async {
                    final ok = await showDialog<bool>(
                      context: context,
                      builder: (_) => _ConfirmChanges(plugin: plugin),
                    );
                    if (ok == true) {
                      await act(() => hub.confirmPlugin(id));
                    }
                  },
                  child: Text(tr('查看并确认变更', 'Review and confirm')),
                ),
              for (final server in servers.where((s) => s['enabled'] == true))
                OutlinedButton(
                  onPressed: () => connected.contains(server['name'])
                      ? act(() => hub.disconnectPlugin(id, '${server['name']}'))
                      : act(() async {
                          final started = await hub.connectPlugin(
                            id,
                            '${server['name']}',
                          );
                          await launchUrl(
                            Uri.parse(started['url']),
                            webOnlyWindowName: '_blank',
                          );
                        }),
                  child: Text(
                    connected.contains(server['name'])
                        ? tr(
                            '断开 ${server['name']} 账号',
                            'Disconnect ${server['name']}',
                          )
                        : tr(
                            '连接 ${server['name']} 账号',
                            'Connect ${server['name']}',
                          ),
                  ),
                ),
              if (settings.isNotEmpty)
                OutlinedButton(
                  onPressed: () async {
                    final values = await showDialog<_Settings>(
                      context: context,
                      builder: (_) => _SettingsDialog(plugin: plugin),
                    );
                    if (values != null) {
                      await act(
                        () => hub.updatePlugin(id, {
                          'config': values.config,
                          'secrets': values.secrets,
                        }),
                      );
                    }
                  },
                  child: Text(tr('设置', 'Settings')),
                ),
              OutlinedButton(
                onPressed: () => act(() => hub.refreshPlugin(id)),
                child: Text(tr('刷新工具', 'Refresh tools')),
              ),
              if ((plugin['source'] as Map?)?['kind'] != 'upload')
                OutlinedButton(
                  onPressed: () => act(() => hub.upgradePlugin(id)),
                  child: Text(tr('检查更新', 'Check for updates')),
                ),
              TextButton(
                style: TextButton.styleFrom(foregroundColor: c.danger),
                onPressed: () async {
                  if (await confirmDialog(
                    context,
                    title: tr('卸载插件？', 'Uninstall plugin?'),
                    message: tr(
                      '“${plugin['name']}”的设置与已连接的账号会一并删除。',
                      'Its settings and connected accounts are deleted too.',
                    ),
                    confirm: tr('卸载', 'Uninstall'),
                    danger: true,
                  )) {
                    await act(() => hub.uninstallPlugin(id));
                  }
                },
                child: Text(tr('卸载', 'Uninstall')),
              ),
            ],
          ),
        ],
      ),
    );
  }
}

/// A waiting version, or changed tools, in full before the member accepts them.
class _ConfirmChanges extends StatelessWidget {
  final Map<String, dynamic> plugin;
  const _ConfirmChanges({required this.plugin});
  @override
  Widget build(BuildContext context) {
    final pending = plugin['pending_version'] as Map?;
    final changed = (plugin['tools'] as List? ?? [])
        .cast<Map>()
        .where((t) => t['confirmed'] != true)
        .toList();
    return AlertDialog(
      title: Text(tr('确认插件变更', 'Confirm plugin changes')),
      content: SizedBox(
        width: 560,
        child: SingleChildScrollView(
          child: Column(
            crossAxisAlignment: CrossAxisAlignment.stretch,
            children: [
              if (pending != null)
                PluginDescription(preview: Map<String, dynamic>.from(pending)),
              if (changed.isNotEmpty) ...[
                const SizedBox(height: 12),
                Text(
                  tr('变更的工具', 'Changed tools'),
                  style: const TextStyle(fontWeight: FontWeight.w600),
                ),
                for (final tool in changed)
                  Padding(
                    padding: const EdgeInsets.only(top: Space.s6),
                    child: SelectableText(
                      '${tool['tool']}: ${tool['description'] ?? ''}',
                    ),
                  ),
              ],
            ],
          ),
        ),
      ),
      actions: [
        TextButton(
          onPressed: () => Navigator.pop(context, false),
          child: Text(tr('取消', 'Cancel')),
        ),
        FilledButton(
          onPressed: () => Navigator.pop(context, true),
          child: Text(tr('确认', 'Confirm')),
        ),
      ],
    );
  }
}

/// Everything a plugin asks for and brings, as the member confirms it: its servers,
/// permissions and settings, and the full text of its skills and commands, which are prompts.
class PluginDescription extends StatelessWidget {
  final Map<String, dynamic> preview;
  const PluginDescription({super.key, required this.preview});
  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    final heading = TextStyle(fontWeight: FontWeight.w600, color: c.text);
    final permissions = preview['permissions'] as Map? ?? {};
    final rules = permissions['tools'] as Map? ?? {};
    final ui = preview['ui'] as Map? ?? {};
    Widget block(String text) => Container(
      width: double.infinity,
      margin: const EdgeInsets.only(top: Space.s6),
      padding: const EdgeInsets.all(Space.s10),
      decoration: BoxDecoration(
        color: c.code,
        borderRadius: BorderRadius.circular(6),
      ),
      child: SelectableText(
        text,
        style: const TextStyle(fontFamily: '0xProto', fontSize: 12),
      ),
    );
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        Row(
          children: [
            Flexible(
              child: Text(
                '${preview['name']}',
                style: heading.copyWith(fontSize: 16),
              ),
            ),
            const SizedBox(width: 8),
            TrustBadge(preview['trust'] as String?),
          ],
        ),
        if ('${preview['description'] ?? ''}'.isNotEmpty)
          Padding(
            padding: const EdgeInsets.only(top: Space.s4),
            child: Text(
              '${preview['description']}',
              style: TextStyle(color: c.muted),
            ),
          ),
        if (preview['trust'] == 'unverified')
          Padding(
            padding: const EdgeInsets.only(top: Space.s6),
            child: Text(
              tr(
                '未经签名或官方审核。请确认你信任它的来源。',
                'Neither signed nor reviewed. Make sure you trust where it comes from.',
              ),
              style: TextStyle(color: c.warning, fontSize: 12),
            ),
          ),
        for (final warning in (preview['warnings'] as List? ?? []))
          Text('⚠ $warning', style: TextStyle(fontSize: 12, color: c.warning)),
        const SizedBox(height: 12),
        Text(tr('连接的服务', 'Servers it connects to'), style: heading),
        for (final s in (preview['servers'] as List? ?? []))
          Padding(
            padding: const EdgeInsets.only(top: Space.s4),
            child: SelectableText(
              s['enabled'] == true
                  ? '${s['name']}: ${s['url']}'
                  : '${s['name']}: ${tr('（本地，不启用）', '(local, not run)')}',
              style: const TextStyle(fontSize: 13),
            ),
          ),
        const SizedBox(height: 12),
        Text(tr('权限', 'Permissions'), style: heading),
        Padding(
          padding: const EdgeInsets.only(top: Space.s4),
          child: Text(
            [
              '${tr('工具默认', 'Tools by default')}: ${rules['default'] ?? 'ask'}',
              '${tr('只读工具', 'Read-only tools')}: ${rules['readOnly'] ?? 'allow'}',
              for (final e in (rules['tools'] as Map? ?? {}).entries)
                '${e.key}: ${e.value}',
              if (ui['mcpApps'] == true)
                tr('可显示网页界面（MCP Apps）', 'May show web pages (MCP Apps)'),
              if (ui['declarative'] == true)
                tr('可显示卡片与表单', 'May show cards and forms'),
            ].join('\n'),
            style: const TextStyle(fontSize: 13),
          ),
        ),
        if ((preview['tools'] as List? ?? []).isNotEmpty) ...[
          const SizedBox(height: 12),
          Text(tr('工具', 'Tools'), style: heading),
          for (final t in (preview['tools'] as List))
            Padding(
              padding: const EdgeInsets.only(top: Space.s4),
              child: SelectableText(
                '${t['name']}: ${t['description'] ?? ''}',
                style: const TextStyle(fontSize: 13),
              ),
            ),
        ],
        for (final u in (preview['unlisted'] as List? ?? []))
          Text(
            u['reason'] == 'needs_connection'
                ? tr(
                    '${u['server']} 需要连接账号后才能列出工具',
                    '${u['server']} lists its tools once an account is connected',
                  )
                : tr(
                    '${u['server']} 暂时无法访问',
                    '${u['server']} cannot be reached now',
                  ),
            style: TextStyle(fontSize: 12, color: c.muted),
          ),
        for (final skill in (preview['skills'] as List? ?? [])) ...[
          const SizedBox(height: 12),
          Text('${tr('技能', 'Skill')} ${skill['name']}', style: heading),
          block('${skill['body']}'),
        ],
        for (final command in (preview['commands'] as List? ?? [])) ...[
          const SizedBox(height: 12),
          Text('${tr('命令', 'Command')} /${command['name']}', style: heading),
          block('${command['prompt']}'),
        ],
      ],
    );
  }
}

class _Settings {
  final Map<String, dynamic> config, secrets;
  const _Settings(this.config, this.secrets);
}

/// The plugin's settings as a form; secret ones are never shown back, only replaced.
class SettingsForm extends StatefulWidget {
  final Map<String, dynamic> fields;
  final Map<String, dynamic> values;
  final List<String> secretsSet;
  final void Function(Map<String, dynamic> config, Map<String, dynamic> secrets)
  onChanged;
  const SettingsForm({
    super.key,
    required this.fields,
    this.values = const {},
    this.secretsSet = const [],
    required this.onChanged,
  });
  @override
  State<SettingsForm> createState() => _SettingsFormState();
}

class _SettingsFormState extends State<SettingsForm> {
  final config = <String, dynamic>{}, secrets = <String, dynamic>{};
  final controllers = <String, TextEditingController>{};

  @override
  void initState() {
    super.initState();
    for (final MapEntry(:key, :value) in widget.fields.entries) {
      final field = value as Map;
      if (field['type'] == 'boolean') {
        config[key] = widget.values[key] ?? field['default'] ?? false;
      } else {
        controllers[key] = TextEditingController(
          text: field['secret'] == true
              ? ''
              : '${widget.values[key] ?? field['default'] ?? ''}',
        );
      }
    }
  }

  @override
  void dispose() {
    for (final c in controllers.values) {
      c.dispose();
    }
    super.dispose();
  }

  void changed() {
    for (final MapEntry(:key, :value) in controllers.entries) {
      final field = widget.fields[key] as Map;
      final text = value.text;
      if (field['secret'] == true) {
        if (text.isNotEmpty) secrets[key] = text;
      } else if (text.isEmpty) {
        config.remove(key);
      } else {
        config[key] = field['type'] == 'number'
            ? num.tryParse(text) ?? text
            : text;
      }
    }
    widget.onChanged(config, secrets);
  }

  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        for (final MapEntry(:key, :value) in widget.fields.entries)
          Padding(
            padding: const EdgeInsets.only(top: Space.s8),
            child: (value as Map)['type'] == 'boolean'
                ? SwitchListTile(
                    contentPadding: EdgeInsets.zero,
                    title: Text('${value['title'] ?? key}'),
                    subtitle: '${value['description'] ?? ''}'.isEmpty
                        ? null
                        : Text('${value['description']}'),
                    value: config[key] == true,
                    onChanged: (v) {
                      setState(() => config[key] = v);
                      changed();
                    },
                  )
                : TextField(
                    controller: controllers[key],
                    obscureText: value['secret'] == true,
                    keyboardType: value['type'] == 'number'
                        ? TextInputType.number
                        : null,
                    onChanged: (_) => changed(),
                    decoration: InputDecoration(
                      labelText:
                          '${value['title'] ?? key}${value['required'] == true ? ' *' : ''}',
                      helperText: [
                        if ('${value['description'] ?? ''}'.isNotEmpty)
                          '${value['description']}',
                        if (value['secret'] == true)
                          widget.secretsSet.contains(key)
                              ? tr('已设置；留空则保持不变', 'Set; leave empty to keep it')
                              : tr(
                                  '保存在密钥库，只发给该插件的服务',
                                  'Kept in the vault and sent only to this plugin',
                                ),
                      ].join('\n'),
                      helperMaxLines: 3,
                      helperStyle: TextStyle(color: c.muted),
                    ),
                  ),
          ),
      ],
    );
  }
}

class _SettingsDialog extends StatefulWidget {
  final Map<String, dynamic> plugin;
  const _SettingsDialog({required this.plugin});
  @override
  State<_SettingsDialog> createState() => _SettingsDialogState();
}

class _SettingsDialogState extends State<_SettingsDialog> {
  var config = <String, dynamic>{}, secrets = <String, dynamic>{};
  @override
  Widget build(BuildContext context) => AlertDialog(
    title: Text(
      tr('${widget.plugin['name']} 设置', '${widget.plugin['name']} settings'),
    ),
    content: SizedBox(
      width: 460,
      child: SingleChildScrollView(
        child: SettingsForm(
          fields: Map<String, dynamic>.from(
            widget.plugin['user_config'] as Map,
          ),
          values: Map<String, dynamic>.from(
            widget.plugin['config'] as Map? ?? {},
          ),
          secretsSet: (widget.plugin['secrets_set'] as List? ?? [])
              .cast<String>(),
          onChanged: (c, s) {
            config = Map.of(c);
            secrets = Map.of(s);
          },
        ),
      ),
    ),
    actions: [
      TextButton(
        onPressed: () => Navigator.pop(context),
        child: Text(tr('取消', 'Cancel')),
      ),
      FilledButton(
        onPressed: () => Navigator.pop(context, _Settings(config, secrets)),
        child: Text(tr('保存', 'Save')),
      ),
    ],
  );
}

/// Install: name a source, see everything the plugin asks for, then install.
class InstallPluginDialog extends StatefulWidget {
  final HubController hub;
  const InstallPluginDialog({super.key, required this.hub});
  @override
  State<InstallPluginDialog> createState() => _InstallPluginDialogState();
}

class _InstallPluginDialogState extends State<InstallPluginDialog> {
  String kind = 'url', error = '';
  final address = TextEditingController(),
      reference = TextEditingController(),
      name = TextEditingController();
  String? uploadName;
  List<int>? uploadBytes;
  List<dynamic>? catalog;
  String? chosen;
  Map<String, dynamic>? preview;
  var config = <String, dynamic>{}, secrets = <String, dynamic>{};
  bool busy = false;
  HubController get hub => widget.hub;

  @override
  void dispose() {
    address.dispose();
    reference.dispose();
    name.dispose();
    super.dispose();
  }

  Future<void> run(Future<void> Function() task) async {
    setState(() {
      busy = true;
      error = '';
    });
    try {
      await task();
    } catch (e) {
      error = errorText(e);
    }
    if (mounted) setState(() => busy = false);
  }

  Future<void> loadCatalog() => run(() async {
    final data = await hub.pluginCatalog();
    catalog = data['plugins'] as List;
  });

  Map<String, dynamic>? get source => switch (kind) {
    'url' => {'kind': 'url', 'url': address.text.trim()},
    'git' => {
      'kind': 'git',
      'url': address.text.trim(),
      'reference': reference.text.trim(),
    },
    'mcp' => {
      'kind': 'mcp',
      'url': address.text.trim(),
      'name': name.text.trim(),
    },
    'upload' =>
      uploadName == null ? null : {'kind': 'upload', 'file_name': uploadName},
    _ => chosen == null ? null : {'kind': 'catalog', 'name': chosen},
  };

  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    final shown = preview;
    return AlertDialog(
      title: Text(
        shown == null
            ? tr('安装插件', 'Install a plugin')
            : tr('确认安装', 'Confirm installation'),
      ),
      content: SizedBox(
        width: 580,
        child: SingleChildScrollView(
          child: shown != null
              ? Column(
                  crossAxisAlignment: CrossAxisAlignment.stretch,
                  children: [
                    PluginDescription(preview: shown),
                    if ((shown['user_config'] as Map? ?? {}).isNotEmpty) ...[
                      const SizedBox(height: 16),
                      Text(
                        tr('设置', 'Settings'),
                        style: const TextStyle(fontWeight: FontWeight.w600),
                      ),
                      SettingsForm(
                        fields: Map<String, dynamic>.from(
                          shown['user_config'] as Map,
                        ),
                        onChanged: (c, s) {
                          config = Map.of(c);
                          secrets = Map.of(s);
                        },
                      ),
                    ],
                    if (error.isNotEmpty)
                      Padding(
                        padding: const EdgeInsets.only(top: Space.s12),
                        child: SelectableText(
                          error,
                          style: TextStyle(color: c.danger),
                        ),
                      ),
                  ],
                )
              : Column(
                  crossAxisAlignment: CrossAxisAlignment.stretch,
                  children: [
                    ChoicePill<String>(
                      tooltip: tr('来源', 'Source'),
                      value: kind,
                      options: [
                        ('url', tr('插件包地址', 'Package URL')),
                        ('git', 'Git'),
                        ('mcp', tr('MCP 服务地址', 'MCP server URL')),
                        ('upload', tr('上传', 'Upload')),
                        ('catalog', tr('官方目录', 'Catalog')),
                      ],
                      onChanged: (v) {
                        setState(() => kind = v);
                        if (v == 'catalog' && catalog == null) loadCatalog();
                      },
                    ),
                    const SizedBox(height: 12),
                    if (kind == 'url' || kind == 'git' || kind == 'mcp')
                      TextField(
                        controller: address,
                        decoration: InputDecoration(
                          labelText: switch (kind) {
                            'git' => tr('仓库地址（HTTPS）', 'Repository (HTTPS)'),
                            'mcp' => tr(
                              'MCP 服务地址（HTTPS）',
                              'MCP server (HTTPS)',
                            ),
                            _ => tr(
                              'zip 或 tar.gz 的地址（HTTPS）',
                              'zip or tar.gz address (HTTPS)',
                            ),
                          },
                        ),
                      ),
                    if (kind == 'git')
                      TextField(
                        controller: reference,
                        decoration: InputDecoration(
                          labelText: tr(
                            '分支、标签或提交（可选）',
                            'Branch, tag or commit (optional)',
                          ),
                        ),
                      ),
                    if (kind == 'mcp')
                      TextField(
                        controller: name,
                        decoration: InputDecoration(
                          labelText: tr('名称（可选）', 'Name (optional)'),
                        ),
                      ),
                    if (kind == 'upload')
                      Row(
                        children: [
                          OutlinedButton(
                            onPressed: () async {
                              final picked = await FilePicker.pickFiles(
                                dialogTitle: tr('选择插件包', 'Choose a package'),
                              );
                              if (picked.isEmpty) return;
                              final bytes = await picked.first.readAsBytes();
                              setState(() {
                                uploadName = picked.first.name;
                                uploadBytes = bytes;
                              });
                            },
                            child: Text(tr('选择文件', 'Choose a file')),
                          ),
                          const SizedBox(width: 12),
                          Flexible(
                            child: Text(
                              uploadName ?? '',
                              overflow: TextOverflow.ellipsis,
                            ),
                          ),
                        ],
                      ),
                    if (kind == 'catalog')
                      catalog == null
                          ? const LinearProgressIndicator()
                          : catalog!.isEmpty
                          ? Text(
                              tr('实例没有配置官方目录', 'The instance has no catalog'),
                              style: TextStyle(color: c.muted),
                            )
                          : RadioGroup<String>(
                              groupValue: chosen,
                              onChanged: (v) => setState(() => chosen = v),
                              child: Column(
                                children: [
                                  for (final entry in catalog!)
                                    RadioListTile<String>(
                                      value: '${entry['name']}',
                                      title: Text('${entry['name']}'),
                                      subtitle: Text(
                                        '${entry['description'] ?? ''}',
                                      ),
                                    ),
                                ],
                              ),
                            ),
                    if (error.isNotEmpty)
                      Padding(
                        padding: const EdgeInsets.only(top: Space.s12),
                        child: SelectableText(
                          error,
                          style: TextStyle(color: c.danger),
                        ),
                      ),
                  ],
                ),
        ),
      ),
      actions: [
        TextButton(
          onPressed: () => Navigator.pop(context),
          child: Text(tr('取消', 'Cancel')),
        ),
        if (shown == null)
          FilledButton(
            onPressed: busy || source == null
                ? null
                : () => run(() async {
                    final data = await hub.previewPlugin(
                      source!,
                      archive: kind == 'upload' && uploadBytes != null
                          ? base64Encode(uploadBytes!)
                          : null,
                    );
                    preview = Map<String, dynamic>.from(data);
                  }),
            child: busy
                ? const SizedBox(
                    width: 16,
                    height: 16,
                    child: CircularProgressIndicator(strokeWidth: 2),
                  )
                : Text(tr('查看', 'Review')),
          )
        else
          FilledButton(
            onPressed: busy
                ? null
                : () => run(() async {
                    await hub.installPlugin(
                      shown['version_id'],
                      config,
                      secrets,
                    );
                    if (context.mounted) Navigator.pop(context, true);
                  }),
            child: Text(tr('信任并安装', 'Trust and install')),
          ),
      ],
    );
  }
}

/// The plugins of the open conversation, each on or off here.
Future<void> showConversationPlugins(BuildContext context, HubController hub) =>
    showDialog(
      context: context,
      builder: (_) => _ConversationPlugins(hub: hub),
    );

class _ConversationPlugins extends StatefulWidget {
  final HubController hub;
  const _ConversationPlugins({required this.hub});
  @override
  State<_ConversationPlugins> createState() => _ConversationPluginsState();
}

class _ConversationPluginsState extends State<_ConversationPlugins> {
  List<dynamic>? plugins;
  String error = '';
  HubController get hub => widget.hub;
  @override
  void initState() {
    super.initState();
    load(hub.conversationPlugins(hub.selected));
  }

  Future<void> load(Future<dynamic> request) async {
    try {
      final data = await request;
      if (mounted) setState(() => plugins = data['plugins'] as List);
    } catch (e) {
      if (mounted) setState(() => error = errorText(e));
    }
  }

  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    return AlertDialog(
      title: Text(tr('本对话的插件', 'Plugins in this conversation')),
      content: SizedBox(
        width: 420,
        child: plugins == null
            ? (error.isEmpty
                  ? const LinearProgressIndicator()
                  : Text(error, style: TextStyle(color: c.danger)))
            : plugins!.isEmpty
            ? Text(
                tr(
                  '还没有安装插件，可在设置中安装。',
                  'No plugins installed. Install them in Settings.',
                ),
              )
            : Column(
                mainAxisSize: MainAxisSize.min,
                children: [
                  for (final p in plugins!)
                    SwitchListTile(
                      contentPadding: EdgeInsets.zero,
                      title: Text('${p['name']}'),
                      subtitle: p['state'] == 'pending'
                          ? Text(tr('尚未确认', 'Not confirmed yet'))
                          : p['conversation'] == null
                          ? Text(
                              tr('跟随默认', 'Following the default'),
                              style: TextStyle(color: c.muted),
                            )
                          : null,
                      value: p['enabled'] == true,
                      onChanged: p['state'] == 'pending'
                          ? null
                          : (v) => load(
                              hub.setConversationPlugin(
                                hub.selected,
                                '${p['id']}',
                                v,
                              ),
                            ),
                    ),
                  Padding(
                    padding: const EdgeInsets.only(top: Space.s8),
                    child: Text(
                      tr('改动从下一次回复起生效。', 'Changes apply from the next reply.'),
                      style: TextStyle(fontSize: 12, color: c.muted),
                    ),
                  ),
                ],
              ),
      ),
      actions: [
        TextButton(
          onPressed: () => Navigator.pop(context),
          child: Text(tr('完成', 'Done')),
        ),
      ],
    );
  }
}

/// A plugin command's parameters, asked before it runs. None: it was cancelled.
Future<Map<String, dynamic>?> askCommandParams(
  BuildContext context,
  Map command,
) async {
  final params = Map<String, dynamic>.from(command['params'] as Map? ?? {});
  if (params.isEmpty) return {};
  final controllers = {for (final k in params.keys) k: TextEditingController()};
  final result = await showDialog<Map<String, dynamic>>(
    context: context,
    builder: (context) => AlertDialog(
      title: Text('/${command['plugin']}:${command['name']}'),
      content: SizedBox(
        width: 420,
        child: Column(
          mainAxisSize: MainAxisSize.min,
          children: [
            if ('${command['description'] ?? ''}'.isNotEmpty)
              Text('${command['description']}'),
            for (final MapEntry(:key, :value) in params.entries)
              TextField(
                controller: controllers[key],
                autofocus: key == params.keys.first,
                keyboardType: (value as Map)['type'] == 'number'
                    ? TextInputType.number
                    : null,
                decoration: InputDecoration(
                  labelText: '${value['title'] ?? key}',
                  helperText: '${value['description'] ?? ''}'.isEmpty
                      ? null
                      : '${value['description']}',
                ),
              ),
          ],
        ),
      ),
      actions: [
        TextButton(
          onPressed: () => Navigator.pop(context),
          child: Text(tr('取消', 'Cancel')),
        ),
        FilledButton(
          onPressed: () => Navigator.pop(context, {
            for (final MapEntry(:key, :value) in params.entries)
              if (controllers[key]!.text.isNotEmpty)
                key: (value as Map)['type'] == 'number'
                    ? num.tryParse(controllers[key]!.text) ??
                          controllers[key]!.text
                    : value['type'] == 'boolean'
                    ? controllers[key]!.text == 'true'
                    : controllers[key]!.text,
          }),
          child: Text(tr('运行', 'Run')),
        ),
      ],
    ),
  );
  for (final controller in controllers.values) {
    controller.dispose();
  }
  return result;
}

/// A plugin server's question (MCP elicitation): a form built from its schema, or a page the
/// member opens. The answer is the form as JSON, or `accept` / `decline`.
class ElicitationCard extends StatefulWidget {
  final String summary;
  final Map<String, dynamic> form;
  final Future<void> Function(String) onAnswer;
  const ElicitationCard({
    super.key,
    required this.summary,
    required this.form,
    required this.onAnswer,
  });
  @override
  State<ElicitationCard> createState() => _ElicitationCardState();
}

class _ElicitationCardState extends State<ElicitationCard> {
  final values = <String, dynamic>{};
  final controllers = <String, TextEditingController>{};
  bool busy = false;

  Map<String, dynamic> get properties => Map<String, dynamic>.from(
    (widget.form['requestedSchema'] as Map?)?['properties'] as Map? ?? {},
  );
  List<String> get required =>
      ((widget.form['requestedSchema'] as Map?)?['required'] as List? ?? [])
          .cast<String>();

  @override
  void initState() {
    super.initState();
    for (final MapEntry(:key, :value) in properties.entries) {
      final field = value as Map;
      if (field['type'] == 'boolean') {
        values[key] = field['default'] == true;
      } else if (field['enum'] is! List) {
        controllers[key] = TextEditingController(
          text: field['default'] == null ? '' : '${field['default']}',
        );
      } else if (field['default'] != null) {
        values[key] = field['default'];
      }
    }
  }

  @override
  void dispose() {
    for (final controller in controllers.values) {
      controller.dispose();
    }
    super.dispose();
  }

  Future<void> answer(String text) async {
    setState(() => busy = true);
    try {
      await widget.onAnswer(text);
    } finally {
      if (mounted) setState(() => busy = false);
    }
  }

  void submit() {
    final content = <String, dynamic>{...values};
    for (final MapEntry(:key, :value) in controllers.entries) {
      final text = value.text.trim();
      if (text.isEmpty) continue;
      final type = (properties[key] as Map)['type'];
      content[key] = type == 'number' || type == 'integer'
          ? num.tryParse(text) ?? text
          : text;
    }
    if (required.any((name) => !content.containsKey(name))) return;
    answer(jsonEncode(content));
  }

  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    final url = widget.form['mode'] == 'url'
        ? '${widget.form['url'] ?? ''}'
        : null;
    return Container(
      padding: const EdgeInsets.all(Space.s16),
      decoration: BoxDecoration(
        color: c.composer,
        borderRadius: BorderRadius.circular(18),
        border: Border.all(color: c.border),
      ),
      child: Column(
        mainAxisSize: MainAxisSize.min,
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          Row(
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              Icon(Icons.extension_outlined, size: 18, color: c.accent),
              const SizedBox(width: 10),
              Expanded(
                child: Text(widget.summary, style: TextStyle(color: c.text)),
              ),
            ],
          ),
          if (url != null)
            Padding(
              padding: const EdgeInsets.only(top: Space.s10),
              child: SelectableText(
                url,
                style: TextStyle(color: c.muted, fontSize: 12),
              ),
            ),
          for (final MapEntry(:key, :value) in properties.entries)
            Padding(
              padding: const EdgeInsets.only(top: Space.s10),
              child: switch ((value as Map)['type']) {
                'boolean' => Row(
                  children: [
                    Expanded(child: Text('${value['title'] ?? key}')),
                    Switch(
                      value: values[key] == true,
                      onChanged: busy
                          ? null
                          : (v) => setState(() => values[key] = v),
                    ),
                  ],
                ),
                _ when value['enum'] is List => DropdownButtonFormField<Object>(
                  initialValue: values[key],
                  decoration: InputDecoration(
                    labelText:
                        '${value['title'] ?? key}${required.contains(key) ? ' *' : ''}',
                  ),
                  items: [
                    for (final option in value['enum'] as List)
                      DropdownMenuItem(value: option, child: Text('$option')),
                  ],
                  onChanged: busy
                      ? null
                      : (v) => setState(() => values[key] = v),
                ),
                _ => TextField(
                  controller: controllers[key],
                  enabled: !busy,
                  keyboardType:
                      value['type'] == 'number' || value['type'] == 'integer'
                      ? TextInputType.number
                      : null,
                  decoration: InputDecoration(
                    labelText:
                        '${value['title'] ?? key}${required.contains(key) ? ' *' : ''}',
                    helperText: value['description'] is String
                        ? value['description'] as String
                        : null,
                  ),
                ),
              },
            ),
          const SizedBox(height: 12),
          Wrap(
            alignment: WrapAlignment.end,
            spacing: 8,
            runSpacing: 8,
            children: [
              OutlinedButton(
                onPressed: busy ? null : () => answer('decline'),
                child: Text(tr('拒绝', 'Decline')),
              ),
              if (url != null) ...[
                OutlinedButton(
                  onPressed: () =>
                      launchUrl(Uri.parse(url), webOnlyWindowName: '_blank'),
                  child: Text(tr('打开页面', 'Open the page')),
                ),
                FilledButton(
                  onPressed: busy ? null : () => answer('accept'),
                  child: Text(tr('已完成', 'Done')),
                ),
              ] else
                FilledButton(
                  onPressed: busy ? null : submit,
                  child: Text(tr('提交', 'Submit')),
                ),
            ],
          ),
        ],
      ),
    );
  }
}
