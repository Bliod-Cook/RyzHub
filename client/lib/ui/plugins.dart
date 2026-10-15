import 'dart:convert';

import 'package:file_picker/file_picker.dart';
import 'package:flutter/material.dart';
import 'package:url_launcher/url_launcher.dart';

import '../controller.dart';
import '../l10n.dart';
import '../theme.dart';
import 'common.dart';
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
      padding: const EdgeInsets.symmetric(horizontal: 6, vertical: 1),
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
  final loader = GlobalKey<LoaderState<dynamic>>();
  String error = '';
  HubController get hub => widget.hub;

  Future<void> act(Future<dynamic> Function() task) async {
    setState(() => error = '');
    try {
      await task();
    } catch (e) {
      if (mounted) setState(() => error = errorText(e));
    }
    loader.currentState?.reload();
  }

  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    return Loader<dynamic>(
      key: loader,
      load: hub.plugins,
      builder: (context, data, reload) => Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          if (error.isNotEmpty)
            Padding(
              padding: const EdgeInsets.only(bottom: 12),
              child: SelectableText(error, style: TextStyle(color: c.danger)),
            ),
          SettingsCard(
            title: tr('已安装', 'Installed'),
            children: [
              if ((data['plugins'] as List).isEmpty)
                SettingRow(title: tr('还没有插件', 'No plugins yet')),
              for (final p in data['plugins'] as List)
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
              onPressed: data['enabled'] == false
                  ? null
                  : () async {
                      final installed = await showDialog<bool>(
                        context: context,
                        builder: (_) => InstallPluginDialog(hub: hub),
                      );
                      if (installed == true) reload();
                    },
            ),
          ),
        ],
      ),
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
        childrenPadding: const EdgeInsets.only(bottom: 12),
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
            const SizedBox(width: Space.gap),
            TrustBadge(plugin['trust'] as String?),
            if (plugin['blocked'] == true) ...[
              const SizedBox(width: Space.gap),
              Text(
                tr('已被停用', 'Stopped by admin'),
                style: TextStyle(fontSize: FontSizes.caption, color: c.danger),
              ),
            ] else if (waiting) ...[
              const SizedBox(width: Space.gap),
              Text(
                tr('有变更待确认', 'Changes to confirm'),
                style: TextStyle(fontSize: FontSizes.caption, color: c.warning),
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
          style: TextStyle(fontSize: FontSizes.caption, color: c.muted),
        ),
        trailing: Switch(
          value: plugin['enabled'] == true,
          onChanged: (v) => act(() => hub.updatePlugin(id, {'enabled': v})),
        ),
        children: [
          for (final warning in (plugin['warnings'] as List? ?? []))
            Text(
              '⚠ $warning',
              style: TextStyle(fontSize: FontSizes.caption, color: c.warning),
            ),
          for (final tool in tools)
            Padding(
              padding: const EdgeInsets.symmetric(vertical: 2),
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
                  fontSize: FontSizes.label,
                  color: tool['confirmed'] == true ? c.text : c.warning,
                ),
              ),
            ),
          const SizedBox(height: Space.gap),
          Wrap(
            spacing: Space.gap,
            runSpacing: Space.gap,
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
                    padding: const EdgeInsets.only(top: 6),
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
    final denied = (preview['permissions']?['deny'] as List? ?? []).join(', ');
    final permissions = [
      if (denied.isNotEmpty) '${tr('不提供的工具', 'Tools never offered')}: $denied',
      if (preview['ui']?['mcpApps'] == true)
        tr('可显示网页界面（MCP Apps）', 'May show web pages (MCP Apps)'),
    ];
    Widget block(String text) => Container(
      width: double.infinity,
      margin: const EdgeInsets.only(top: 6),
      padding: const EdgeInsets.all(10),
      decoration: BoxDecoration(
        color: c.code,
        borderRadius: BorderRadius.circular(Radii.small),
      ),
      child: SelectableText(
        text,
        style: codeStyle.copyWith(fontSize: FontSizes.caption),
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
                style: heading.copyWith(fontSize: FontSizes.title),
              ),
            ),
            const SizedBox(width: Space.gap),
            TrustBadge(preview['trust'] as String?),
          ],
        ),
        if ('${preview['description'] ?? ''}'.isNotEmpty)
          Padding(
            padding: const EdgeInsets.only(top: 4),
            child: Text(
              '${preview['description']}',
              style: TextStyle(color: c.muted),
            ),
          ),
        if (preview['trust'] == 'unverified')
          Padding(
            padding: const EdgeInsets.only(top: 6),
            child: Text(
              tr('请确认你信任它的来源', 'Make sure you trust its source'),
              style: TextStyle(color: c.warning, fontSize: FontSizes.caption),
            ),
          ),
        for (final warning in (preview['warnings'] as List? ?? []))
          Text(
            '⚠ $warning',
            style: TextStyle(fontSize: FontSizes.caption, color: c.warning),
          ),
        const SizedBox(height: 12),
        Text(tr('连接的服务', 'Servers it connects to'), style: heading),
        for (final s in (preview['servers'] as List? ?? []))
          Padding(
            padding: const EdgeInsets.only(top: 4),
            child: SelectableText(
              s['enabled'] == true
                  ? '${s['name']}: ${s['url']}'
                  : '${s['name']}: ${tr('（本地，不启用）', '(local, not run)')}',
              style: const TextStyle(fontSize: FontSizes.label),
            ),
          ),
        if (permissions.isNotEmpty) ...[
          const SizedBox(height: 12),
          Text(tr('权限', 'Permissions'), style: heading),
          Padding(
            padding: const EdgeInsets.only(top: 4),
            child: Text(
              permissions.join('\n'),
              style: const TextStyle(fontSize: FontSizes.label),
            ),
          ),
        ],
        if ((preview['tools'] as List? ?? []).isNotEmpty) ...[
          const SizedBox(height: 12),
          Text(tr('工具', 'Tools'), style: heading),
          for (final t in (preview['tools'] as List))
            Padding(
              padding: const EdgeInsets.only(top: 4),
              child: SelectableText(
                '${t['name']}: ${t['description'] ?? ''}',
                style: const TextStyle(fontSize: FontSizes.label),
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
            style: TextStyle(fontSize: FontSizes.caption, color: c.muted),
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

/// A plugin's settings, the secret ones apart.
class _Settings {
  final Map<String, dynamic> config, secrets;
  const _Settings(this.config, this.secrets);

  /// [values] given for the settings [fields].
  factory _Settings.of(Map fields, Map<String, dynamic> values) {
    bool secret(String key) => (fields[key] as Map?)?['secret'] == true;
    return _Settings(
      {
        for (final MapEntry(:key, :value) in values.entries)
          if (!secret(key)) key: value,
      },
      {
        for (final MapEntry(:key, :value) in values.entries)
          if (secret(key)) key: value,
      },
    );
  }
}

/// Fields for a JSON schema's [properties]: a switch for a boolean, a menu for
/// a choice, else a text field. A required field's label ends in *, and a
/// secret one is never shown back. [onChanged] gets what is given, from the
/// start: numbers as numbers, empty text left out.
class SchemaForm extends StatefulWidget {
  final Map<String, dynamic> properties;
  final List<String> required;
  final Map<String, dynamic> values;

  /// Secret fields that already have a value.
  final List<String> secretsSet;
  final bool enabled;
  final ValueChanged<Map<String, dynamic>> onChanged;
  const SchemaForm({
    super.key,
    required this.properties,
    required this.onChanged,
    this.required = const [],
    this.values = const {},
    this.secretsSet = const [],
    this.enabled = true,
  });
  @override
  State<SchemaForm> createState() => _SchemaFormState();
}

class _SchemaFormState extends State<SchemaForm> {
  /// Switches and menus by field.
  final picked = <String, dynamic>{};
  final texts = <String, TextEditingController>{};

  Map<String, dynamic> get values => {
    ...picked,
    for (final MapEntry(:key, :value) in texts.entries)
      if (value.text.trim().isNotEmpty)
        key: switch ((widget.properties[key] as Map)['type']) {
          'number' ||
          'integer' => num.tryParse(value.text.trim()) ?? value.text.trim(),
          _ => value.text.trim(),
        },
  };

  @override
  void initState() {
    super.initState();
    for (final MapEntry(:key, :value) in widget.properties.entries) {
      final field = value as Map;
      final initial = field['secret'] == true
          ? null
          : widget.values[key] ?? field['default'];
      if (field['type'] == 'boolean') {
        picked[key] = initial == true;
      } else if (field['enum'] is List) {
        if (initial != null) picked[key] = initial;
      } else {
        texts[key] = TextEditingController(text: '${initial ?? ''}');
      }
    }
    widget.onChanged(values);
  }

  @override
  void dispose() {
    for (final text in texts.values) {
      text.dispose();
    }
    super.dispose();
  }

  void pick(String key, Object? value) {
    setState(() => picked[key] = value);
    widget.onChanged(values);
  }

  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        for (final MapEntry(:key, :value) in widget.properties.entries)
          Padding(
            padding: const EdgeInsets.only(top: Space.gap),
            child: field(c, key, value as Map),
          ),
      ],
    );
  }

  Widget field(HubColors c, String key, Map field) {
    final label =
        '${field['title'] ?? key}${widget.required.contains(key) ? ' *' : ''}';
    final description = '${field['description'] ?? ''}';
    final enabled = widget.enabled;
    if (field['type'] == 'boolean') {
      return SettingRow(
        title: label,
        subtitle: description.isEmpty ? null : description,
        trailing: Switch(
          value: picked[key] == true,
          onChanged: enabled ? (v) => pick(key, v) : null,
        ),
      );
    }
    if (field['enum'] is List) {
      return DropdownButtonFormField<Object>(
        initialValue: picked[key],
        decoration: InputDecoration(
          labelText: label,
          helperText: description.isEmpty ? null : description,
        ),
        items: [
          for (final option in field['enum'] as List)
            DropdownMenuItem(value: option, child: Text('$option')),
        ],
        onChanged: enabled ? (v) => pick(key, v) : null,
      );
    }
    final secret = field['secret'] == true;
    final helper = [
      if (description.isNotEmpty) description,
      if (secret)
        widget.secretsSet.contains(key)
            ? tr('已设置；留空则保持不变', 'Set; leave empty to keep it')
            : tr(
                '保存在密钥库，只发给该插件的服务',
                'Kept in the vault, sent only to this plugin',
              ),
    ].join('\n');
    return TextField(
      controller: texts[key],
      enabled: enabled,
      obscureText: secret,
      style: secret ? codeStyle : null,
      keyboardType: field['type'] == 'number' || field['type'] == 'integer'
          ? TextInputType.number
          : null,
      onChanged: (_) => widget.onChanged(values),
      decoration: InputDecoration(
        labelText: label,
        helperText: helper.isEmpty ? null : helper,
        helperMaxLines: 3,
        helperStyle: TextStyle(color: c.muted),
      ),
    );
  }
}

/// The required fields of a plugin's settings.
List<String> _requiredSettings(Map fields) => [
  for (final MapEntry(:key, :value) in fields.entries)
    if ((value as Map)['required'] == true) '$key',
];

class _SettingsDialog extends StatefulWidget {
  final Map<String, dynamic> plugin;
  const _SettingsDialog({required this.plugin});
  @override
  State<_SettingsDialog> createState() => _SettingsDialogState();
}

class _SettingsDialogState extends State<_SettingsDialog> {
  late final fields = Map<String, dynamic>.from(
    widget.plugin['user_config'] as Map,
  );
  var values = <String, dynamic>{};
  @override
  Widget build(BuildContext context) => AlertDialog(
    title: Text(
      tr('${widget.plugin['name']} 设置', '${widget.plugin['name']} settings'),
    ),
    content: SizedBox(
      width: 460,
      child: SingleChildScrollView(
        child: SchemaForm(
          properties: fields,
          required: _requiredSettings(fields),
          values: Map<String, dynamic>.from(
            widget.plugin['config'] as Map? ?? {},
          ),
          secretsSet: (widget.plugin['secrets_set'] as List? ?? [])
              .cast<String>(),
          onChanged: (v) => values = v,
        ),
      ),
    ),
    actions: [
      TextButton(
        onPressed: () => Navigator.pop(context),
        child: Text(tr('取消', 'Cancel')),
      ),
      FilledButton(
        onPressed: () => Navigator.pop(context, _Settings.of(fields, values)),
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
  var settings = <String, dynamic>{};
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
                      SchemaForm(
                        properties: Map<String, dynamic>.from(
                          shown['user_config'] as Map,
                        ),
                        required: _requiredSettings(
                          shown['user_config'] as Map,
                        ),
                        onChanged: (v) => settings = v,
                      ),
                    ],
                    if (error.isNotEmpty)
                      Padding(
                        padding: const EdgeInsets.only(top: 12),
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
                        padding: const EdgeInsets.only(top: 12),
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
                    final values = _Settings.of(
                      shown['user_config'] as Map? ?? {},
                      settings,
                    );
                    await hub.installPlugin(
                      shown['version_id'],
                      values.config,
                      values.secrets,
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
  final loader = GlobalKey<LoaderState<dynamic>>();
  String error = '';
  HubController get hub => widget.hub;

  Future<void> toggle(String plugin, bool enabled) async {
    setState(() => error = '');
    try {
      await hub.setConversationPlugin(hub.selected, plugin, enabled);
    } catch (e) {
      if (mounted) setState(() => error = errorText(e));
    }
    loader.currentState?.reload();
  }

  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    return AlertDialog(
      title: Text(tr('本对话的插件', 'Plugins in this conversation')),
      content: SizedBox(
        width: 420,
        child: Loader<dynamic>(
          key: loader,
          load: () => hub.conversationPlugins(hub.selected),
          builder: (context, data, _) {
            final plugins = data['plugins'] as List;
            if (plugins.isEmpty) {
              return Text(
                tr(
                  '还没有安装插件，可在设置中安装。',
                  'No plugins installed. Install them in Settings.',
                ),
              );
            }
            return Column(
              mainAxisSize: MainAxisSize.min,
              children: [
                for (final p in plugins)
                  SwitchListTile(
                    contentPadding: EdgeInsets.zero,
                    title: Text('${p['name']}'),
                    subtitle: p['conversation'] == null
                        ? Text(
                            tr('跟随默认', 'Following the default'),
                            style: TextStyle(color: c.muted),
                          )
                        : null,
                    value: p['enabled'] == true,
                    onChanged: (v) => toggle('${p['id']}', v),
                  ),
                if (error.isNotEmpty)
                  Text(error, style: TextStyle(color: c.danger)),
              ],
            );
          },
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
  var values = <String, dynamic>{};
  return showDialog<Map<String, dynamic>>(
    context: context,
    builder: (context) => AlertDialog(
      title: Text('/${command['plugin']}:${command['name']}'),
      content: SizedBox(
        width: 420,
        child: Column(
          mainAxisSize: MainAxisSize.min,
          crossAxisAlignment: CrossAxisAlignment.stretch,
          children: [
            if ('${command['description'] ?? ''}'.isNotEmpty)
              Text('${command['description']}'),
            SchemaForm(properties: params, onChanged: (v) => values = v),
          ],
        ),
      ),
      actions: [
        TextButton(
          onPressed: () => Navigator.pop(context),
          child: Text(tr('取消', 'Cancel')),
        ),
        FilledButton(
          onPressed: () => Navigator.pop(context, values),
          child: Text(tr('运行', 'Run')),
        ),
      ],
    ),
  );
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
  var values = <String, dynamic>{};
  bool busy = false;

  Map get schema => widget.form['requestedSchema'] as Map? ?? {};
  List<String> get required => (schema['required'] as List? ?? []).cast();

  Future<void> answer(String text) async {
    setState(() => busy = true);
    try {
      await widget.onAnswer(text);
    } finally {
      if (mounted) setState(() => busy = false);
    }
  }

  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    final url = widget.form['mode'] == 'url'
        ? '${widget.form['url'] ?? ''}'
        : null;
    return PendingCard(
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
              padding: const EdgeInsets.only(top: 10),
              child: SelectableText(
                url,
                style: TextStyle(color: c.muted, fontSize: FontSizes.caption),
              ),
            ),
          SchemaForm(
            properties: Map<String, dynamic>.from(
              schema['properties'] as Map? ?? {},
            ),
            required: required,
            enabled: !busy,
            onChanged: (v) => values = v,
          ),
          const SizedBox(height: 12),
          Wrap(
            alignment: WrapAlignment.end,
            spacing: Space.gap,
            runSpacing: Space.gap,
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
                  // A required field must be filled first.
                  onPressed: busy
                      ? null
                      : () {
                          if (required.every(values.containsKey)) {
                            answer(jsonEncode(values));
                          }
                        },
                  child: Text(tr('提交', 'Submit')),
                ),
            ],
          ),
        ],
      ),
    );
  }
}
