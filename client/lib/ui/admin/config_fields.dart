import 'package:flutter/material.dart';

import '../../l10n.dart';
import '../../theme.dart';
import '../common.dart';
import 'fields.dart';

/// A page of the instance configuration.
class ConfigPage {
  final String id;
  final IconData icon;
  final String Function() title;
  final List<String> keywords;
  const ConfigPage(this.id, this.icon, this.title, this.keywords);
}

final configSections = [
  ConfigPage('general', Icons.tune, () => tr('通用', 'General'), [
    'origin',
    '来源',
    'url',
  ]),
  ConfigPage('auth', Icons.login, () => tr('登录', 'Sign-in'), [
    '注册',
    'registration',
    'oidc',
    'github',
  ]),
  ConfigPage('models', Icons.smart_toy_outlined, () => tr('模型', 'Models'), [
    '网关',
    'gateway',
    'llm',
    'key',
    '密钥',
  ]),
  ConfigPage('policy', Icons.policy_outlined, () => tr('策略', 'Policy'), [
    '限制',
    'limit',
    'policy',
  ]),
  ConfigPage(
    'sandbox',
    Icons.inventory_2_outlined,
    () => tr('沙箱', 'Sandboxes'),
    ['sandbox', '镜像', 'image', 'egress'],
  ),
  ConfigPage(
    'work',
    Icons.terminal,
    () => tr('Work 与 Harness', 'Work and harnesses'),
    ['work', 'codex', 'claude', 'acp', 'harness'],
  ),
  ConfigPage('plugins', Icons.extension_outlined, () => tr('插件', 'Plugins'), [
    'plugin',
    'mcp',
  ]),
  ConfigPage(
    'search',
    Icons.manage_search,
    () => tr('搜索与记忆', 'Search and memory'),
    ['search', 'embedding', '向量', '记忆', 'memory'],
  ),
  ConfigPage(
    'push',
    Icons.notifications_outlined,
    () => tr('推送', 'Notifications'),
    ['push', 'fcm', '通知'],
  ),
  ConfigPage('retention', Icons.history, () => tr('数据保留', 'Retention'), [
    '保留',
    'retention',
    'usage',
    'audit',
    'checkpoint',
  ]),
];

/// What a field's control sees: its value, the whole configuration (for the
/// values others hold) and the variables keys may come from. [onChanged] is
/// null when the field is read-only.
class FieldInput {
  final Object? value;
  final Map<String, dynamic> config;
  final Map<String, bool> env;
  final ValueChanged<Object?>? onChanged;
  const FieldInput(this.value, this.config, this.env, this.onChanged);
}

class Field {
  final String path;
  final String Function() label;
  final String Function()? hint;

  /// The control goes under the label rather than beside it.
  final bool wide;
  final Widget Function(BuildContext context, FieldInput input) control;
  const Field(
    this.path,
    this.label,
    this.control, {
    this.hint,
    this.wide = false,
  });
}

/// The value at dotted [path] in [config].
Object? valueAt(Map<String, dynamic> config, String path) {
  Object? value = config;
  for (final key in path.split('.')) {
    value = value is Map ? value[key] : null;
  }
  return value;
}

/// A JSON list as strings.
List<String> strings(Object? value) => [
  for (final v in value as List? ?? []) '$v',
];

Field _toggle(
  String path,
  String Function() label, {
  String Function()? hint,
}) => Field(
  path,
  label,
  hint: hint,
  (_, f) => Switch(
    value: f.value == true,
    onChanged: f.onChanged == null ? null : (v) => f.onChanged!(v),
  ),
);

Field _text(
  String path,
  String Function() label, {
  bool optional = false,
  String Function()? hint,
}) => Field(
  path,
  label,
  hint: hint,
  (_, f) => SizedBox(
    width: 300,
    child: TextValueField(
      value: f.value as String?,
      optional: optional,
      onChanged: f.onChanged,
    ),
  ),
);

Field _number(
  String path,
  String Function() label,
  List<Scale> scales, {
  String Function()? hint,
}) => Field(
  path,
  label,
  hint: hint,
  (_, f) => NumberField(
    value: (f.value as num? ?? 0).toInt(),
    scales: scales,
    onChanged: f.onChanged,
  ),
);

Field _limit(
  String path,
  String Function() label,
  List<Scale> scales,
  int initial,
) => Field(
  path,
  label,
  (_, f) => LimitField(
    value: (f.value as num?)?.toInt(),
    scales: scales,
    initial: initial,
    onChanged: f.onChanged,
  ),
);

Field _tri(String path, String Function() label) => Field(
  path,
  label,
  (_, f) => TriStateField(value: f.value as bool?, onChanged: f.onChanged),
);

Field _choice(String path, String Function() label, List<String> options) =>
    Field(
      path,
      label,
      (_, f) => ChoiceField<String>(
        value: '${f.value}',
        options: [for (final o in options) (o, o)],
        onChanged: f.onChanged,
      ),
    );

Field _tags(
  String path,
  String Function() label, {
  bool Function(String)? valid,
}) => Field(
  path,
  label,
  wide: true,
  (_, f) =>
      TagsField(values: strings(f.value), valid: valid, onChanged: f.onChanged),
);

/// A set the policy may leave unrestricted (null), else a list of values:
/// typed in, or picked from [options] when there are known ones.
Field _restricted(
  String path,
  String Function() label, {
  List<(String, String)> Function(Map<String, dynamic> config)? options,
  bool Function(String)? valid,
}) => Field(path, label, wide: true, (_, f) {
  final open = f.value == null;
  return Column(
    crossAxisAlignment: CrossAxisAlignment.stretch,
    children: [
      Row(
        children: [
          Text(tr('不限', 'No limit')),
          Switch(
            value: open,
            onChanged: f.onChanged == null
                ? null
                : (v) => f.onChanged!(v ? null : <String>[]),
          ),
        ],
      ),
      if (!open)
        options == null
            ? TagsField(
                values: strings(f.value),
                valid: valid,
                onChanged: f.onChanged,
              )
            : MultiField(
                values: strings(f.value),
                options: options(f.config),
                onChanged: f.onChanged,
              ),
    ],
  );
});

Field _env(String path, String Function() label, {bool allowDefault = false}) =>
    Field(
      path,
      label,
      (_, f) => EnvField(
        value: '${f.value ?? ''}',
        env: f.env,
        allowDefault: allowDefault,
        onChanged: f.onChanged,
      ),
    );

Field _keyValue(String path, String Function() label) => Field(
  path,
  label,
  wide: true,
  (_, f) => KeyValueField(
    values: {
      for (final MapEntry(:key, :value) in (f.value as Map? ?? {}).entries)
        '$key': '$value',
    },
    onChanged: f.onChanged,
  ),
);

/// A value only the configuration file sets, summarised.
Field _summary(
  String path,
  String Function() label,
  String Function(Object? value) summary,
) => Field(
  path,
  label,
  (context, f) => Text(
    summary(f.value),
    style: TextStyle(color: HubColors.of(context).muted),
  ),
);

/// Models the instance routes, for picking among them.
List<(String, String)> knownModels(Map<String, dynamic> config) {
  final ids = <String>{
    '${valueAt(config, 'llm.model') ?? ''}',
    for (final m in valueAt(config, 'llm.models') as List? ?? []) '${m['id']}',
  }..remove('');
  return [for (final id in ids) (id, id)];
}

/// Harnesses the instance runs, for picking among them.
List<(String, String)> knownHarnesses(Map<String, dynamic> config) => [
  ('chat', 'Chat'),
  ('codex', 'Codex'),
  ('claude', 'Claude Code'),
  for (final a in valueAt(config, 'harness.acp') as List? ?? [])
    ('${a['name']}', '${a['label'] ?? a['name']}'),
];

const _protocols = ['anthropic', 'responses', 'chat_completions', 'gemini'];

bool _origin(String v) {
  final uri = Uri.tryParse(v);
  return uri != null &&
      (uri.scheme == 'http' || uri.scheme == 'https') &&
      uri.host.isNotEmpty &&
      (uri.path.isEmpty || uri.path == '/') &&
      !uri.hasQuery;
}

bool _http(String v) => v.startsWith('http://') || v.startsWith('https://');

/// An egress rule as nodes take it: a lowercase host name, or `*.` and one.
bool egressRule(String v) {
  final name = v.startsWith('*.') ? v.substring(2) : v;
  return name.length <= 253 &&
      name.contains('.') &&
      name
          .split('.')
          .every(
            (l) => RegExp(r'^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$').hasMatch(l),
          );
}

String Function() _zero(String zh, String en) =>
    () => tr('0：$zh', '0: $en');

List<Field> fieldsOf(String section) => switch (section) {
  'general' => [
    _text('listen', () => tr('监听地址', 'Listen address')),
    _text('public_url', () => tr('公开地址', 'Public URL')),
    _tags(
      'allowed_origins',
      () => tr('允许的来源', 'Allowed origins'),
      valid: _origin,
    ),
    _number(
      'approval_ttl_secs',
      () => tr('审批有效期', 'Approvals expire after'),
      secondScales,
    ),
    _text('attachment_renderer', () => tr('图片解码程序', 'Image decoder')),
  ],
  'auth' => [
    _toggle('auth.registration', () => tr('开放注册', 'Open sign-up')),
    _toggle(
      'auth.link_verified_email',
      () => tr('按已验证的邮箱合并账号', 'Join accounts by verified email'),
    ),
    _tags(
      'auth.return_urls',
      () => tr('登录后可返回的地址', 'Return addresses after sign-in'),
      valid: _http,
    ),
    _summary(
      'auth.oidc',
      () => 'OpenID Connect',
      (v) => (v as List? ?? []).isEmpty
          ? tr('未配置', 'None')
          : [for (final p in v as List) '${p['label'] ?? p['name']}']
                .join(', '),
    ),
    _summary(
      'auth.github',
      () => 'GitHub',
      (v) => v == null ? tr('未配置', 'None') : '${(v as Map)['label']}',
    ),
  ],
  'models' => [
    _toggle('llm.enabled', () => tr('模型网关', 'Model gateway')),
    _text('llm.listen', () => tr('网关监听地址', 'Gateway listen address')),
    _text('llm.gateway_url', () => tr('网关地址', 'Gateway URL')),
    _text(
      'llm.unix_socket',
      () => tr('沙箱用的网关 socket', 'Gateway socket for sandboxes'),
      optional: true,
    ),
    _text('llm.model', () => tr('默认 Chat 模型', 'Default Chat model')),
    Field(
      'llm.models',
      () => tr('模型路由', 'Model routes'),
      wide: true,
      (context, f) => ObjectListField(
        values: [
          for (final m in f.value as List? ?? []) Map<String, dynamic>.from(m),
        ],
        title: (m) => '${m['id']}',
        subtitle: (m) => [
          '${m['protocol']}',
          strings(m['harnesses']).join(', '),
          for (final u in m['upstreams'] as List? ?? []) '${u['url']}',
        ].join(' · '),
        edit: (m) => editModelRoute(context, m, f),
        onChanged: f.onChanged,
      ),
    ),
    _text('llm.upstream_url', () => tr('Anthropic 地址', 'Anthropic URL')),
    _env('llm.platform_key_env', () => tr('Anthropic 密钥', 'Anthropic key')),
    _text(
      'llm.responses_url',
      () => tr('OpenAI Responses 地址', 'OpenAI Responses URL'),
    ),
    _env(
      'llm.responses_key_env',
      () => tr('OpenAI Responses 密钥', 'OpenAI Responses key'),
    ),
    _text(
      'llm.chat_completions_url',
      () => tr('Chat Completions 地址', 'Chat Completions URL'),
    ),
    _env(
      'llm.chat_completions_key_env',
      () => tr('Chat Completions 密钥', 'Chat Completions key'),
    ),
    _text('llm.gemini_url', () => tr('Gemini 地址', 'Gemini URL')),
    _env('llm.gemini_key_env', () => tr('Gemini 密钥', 'Gemini key')),
    _number(
      'llm.requests_per_minute_key',
      () => tr('每个密钥每分钟请求数', 'Requests per minute per key'),
      plainScales,
    ),
    _number(
      'llm.max_tokens',
      () => tr('最大输出 token', 'Max output tokens'),
      plainScales,
    ),
    _number(
      'llm.context_budget_tokens',
      () => tr('Chat 上下文预算 token', 'Chat context budget (tokens)'),
      plainScales,
      hint: _zero('保留全部工具结果', 'keep every tool result'),
    ),
    _tags(
      'llm.compaction_models',
      () => tr('自动压缩上下文的模型（前缀）', 'Models that compact context (prefixes)'),
    ),
    _number(
      'llm.compaction_trigger_tokens',
      () => tr('压缩触发 token', 'Compaction threshold (tokens)'),
      plainScales,
    ),
    _tags(
      'llm.tool_search_models',
      () => tr('按需加载工具的模型（前缀）', 'Models that search tools (prefixes)'),
    ),
    _tags(
      'llm.pro_mode_models',
      () => tr('支持 Pro 模式的模型（前缀）', 'Models with Pro mode (prefixes)'),
    ),
    _text('llm.vault_key_env', () => tr('凭据加密密钥变量', 'Vault key variable')),
    _number(
      'llm.vault_key_id',
      () => tr('凭据加密密钥 ID', 'Vault key id'),
      plainScales,
    ),
    _keyValue(
      'llm.vault_retired_keys',
      () => tr('已退役的加密密钥', 'Retired vault keys'),
    ),
  ],
  'policy' => [
    _restricted(
      'policy.models',
      () => tr('可用模型', 'Models'),
      options: knownModels,
    ),
    _restricted(
      'policy.harnesses',
      () => tr('可用 Harness', 'Harnesses'),
      options: knownHarnesses,
    ),
    _restricted(
      'policy.plugin_sources',
      () => tr('插件来源', 'Plugin sources'),
      options: (_) => [
        for (final s in ['url', 'git', 'upload', 'catalog', 'mcp']) (s, s),
      ],
    ),
    _restricted(
      'policy.egress_hosts',
      () => tr('沙箱可访问的主机', 'Hosts sandboxes reach'),
      valid: egressRule,
    ),
    _limit(
      'policy.max_sandboxes',
      () => tr('每个组织的沙箱数', 'Sandboxes per organization'),
      plainScales,
      4,
    ),
    _limit(
      'policy.sandbox_disk_bytes',
      () => tr('沙箱磁盘', 'Sandbox disk'),
      byteScales,
      10 << 30,
    ),
    _limit(
      'policy.compute_idle_ttl_secs',
      () => tr('空闲挂起不晚于', 'Suspend idle sandboxes within'),
      secondScales,
      1800,
    ),
    _limit(
      'policy.auto_archive_after_secs',
      () => tr('自动归档', 'Archive conversations after'),
      secondScales,
      30 * 86400,
    ),
    _limit(
      'policy.archive_retention_secs',
      () => tr('归档保留', 'Keep archives for'),
      secondScales,
      90 * 86400,
    ),
    _tri(
      'policy.allow_personal_credentials',
      () => tr('个人模型凭据', 'Personal model keys'),
    ),
    _tri(
      'policy.allow_platform_credentials',
      () => tr('平台模型凭据', 'Platform model keys'),
    ),
    _tri('policy.allow_personal_plugins', () => tr('个人插件', 'Personal plugins')),
    _tri(
      'policy.allow_unverified_plugins',
      () => tr('未验证的插件', 'Unverified plugins'),
    ),
    _tri('policy.allow_memory', () => tr('记忆', 'Memory')),
    _tri(
      'policy.allow_memory_auto_extract',
      () => tr('自动提取记忆', 'Automatic memory'),
    ),
    _tri('policy.allow_mcp_apps', () => 'MCP Apps'),
    _tri('policy.allow_full_auto', () => tr('全部自动模式', 'Full-auto mode')),
    _tri('policy.allow_projects', () => tr('项目', 'Projects')),
    _limit(
      'policy.project_file_bytes',
      () => tr('项目文件大小', 'Project file size'),
      byteScales,
      10 << 20,
    ),
    _limit(
      'policy.attachment_bytes',
      () => tr('单个文件大小', 'File size'),
      byteScales,
      100 << 20,
    ),
    _limit(
      'policy.attachment_quota_bytes',
      () => tr('文件库容量', 'File library quota'),
      byteScales,
      1 << 30,
    ),
    Field(
      'policy.max_reasoning',
      () => tr('最高推理强度', 'Most reasoning'),
      (_, f) => f.onChanged == null
          ? Text(
              f.value == null
                  ? tr('不限', 'No limit')
                  : reasoningLabel('${f.value}'),
            )
          : ChoicePill<String>(
              tooltip: tr('最高推理强度', 'Most reasoning'),
              value: '${f.value ?? ''}',
              options: [
                ('', tr('不限', 'No limit')),
                for (final l in [
                  'minimal',
                  'low',
                  'medium',
                  'high',
                  'xhigh',
                  'max',
                ])
                  (l, l == 'minimal' ? tr('最低', 'Minimal') : reasoningLabel(l)),
              ],
              onChanged: (v) => f.onChanged!(v.isEmpty ? null : v),
            ),
    ),
    _restricted(
      'policy.approval_required_tools',
      () => tr('需要审批的插件工具', 'Plugin tools that need approval'),
    ),
    _restricted(
      'policy.memory_forbidden_categories',
      () => tr('不记忆的类别', 'Categories kept out of memory'),
    ),
  ],
  'sandbox' => [
    _toggle('sandbox.enabled', () => tr('沙箱', 'Sandboxes')),
    _text('sandbox.host_socket', () => tr('宿主 socket', 'Host socket')),
    _summary(
      'sandbox.nodes',
      () => tr('执行节点', 'Execution nodes'),
      (v) => (v as List? ?? []).isEmpty
          ? 'default'
          : [for (final n in v as List) '${n['name']}'].join(', '),
    ),
    _text('sandbox.image', () => tr('镜像', 'Image')),
    _text('sandbox.deps', () => tr('依赖层', 'Dependency layer')),
    _choice('sandbox.runtime', () => tr('运行时', 'Runtime'), ['runc', 'runsc']),
    _number('sandbox.memory_bytes', () => tr('内存', 'Memory'), byteScales),
    _number('sandbox.disk_bytes', () => tr('磁盘', 'Disk'), byteScales),
    _number('sandbox.pids_limit', () => tr('进程数', 'Processes'), plainScales),
    _tags(
      'sandbox.egress_allow',
      () => tr('可访问的主机', 'Reachable hosts'),
      valid: egressRule,
    ),
    _number(
      'sandbox.chat_idle_secs',
      () => tr('Chat 沙箱保留', 'Keep Chat sandboxes for'),
      secondScales,
      hint: _zero('运行结束即销毁', 'destroy when the run ends'),
    ),
    _toggle(
      'sandbox.chat_checkpoints',
      () => tr('Chat 工作区检查点', 'Chat workspace checkpoints'),
    ),
    _number(
      'sandbox.reclaim_after_secs',
      () => tr('回收已停止的沙箱', 'Reclaim stopped sandboxes after'),
      secondScales,
    ),
    _number(
      'sandbox.workspace_evict_after_secs',
      () => tr('移出闲置的 Chat 工作区', 'Evict idle Chat workspaces after'),
      secondScales,
      hint: _zero('不移出', 'keep them'),
    ),
    _number(
      'sandbox.image_gc_after_secs',
      () => tr('删除无用镜像', 'Remove unused images after'),
      secondScales,
    ),
  ],
  'work' => [
    _toggle('work.enabled', () => 'Work'),
    _text('work.model', () => tr('默认 Codex 模型', 'Default Codex model')),
    _text(
      'work.claude_model',
      () => tr('默认 Claude Code 模型', 'Default Claude Code model'),
    ),
    _text('work.storage_root', () => tr('检查点存储', 'Checkpoint storage')),
    _text(
      'work.secondary_root',
      () => tr('第二份存储', 'Second copy'),
      optional: true,
    ),
    _number(
      'work.idle_suspend_secs',
      () => tr('空闲挂起', 'Suspend when idle for'),
      secondScales,
      hint: _zero('不挂起', 'keep running'),
    ),
    Field(
      'harness.acp',
      () => tr('ACP Harness', 'ACP harnesses'),
      wide: true,
      (context, f) => ObjectListField(
        values: [
          for (final a in f.value as List? ?? []) Map<String, dynamic>.from(a),
        ],
        title: (a) =>
            '${a['label'] ?? ''}'.isEmpty ? '${a['name']}' : '${a['label']}',
        subtitle: (a) =>
            '${a['name']} · ${a['protocol']} · ${a['model']} · ${a['version']}',
        edit: (a) => editAcp(context, a),
        onChanged: f.onChanged,
      ),
    ),
  ],
  'plugins' => [
    _toggle('plugins.enabled', () => tr('插件', 'Plugins')),
    _keyValue(
      'plugins.trusted_keys',
      () => tr('受信任的发布者密钥', 'Trusted publisher keys'),
    ),
    _text('plugins.catalog_url', () => tr('官方目录地址', 'Catalog URL')),
    _text('plugins.client_url', () => tr('客户端地址', 'Client URL')),
    _text('plugins.content_origin', () => tr('MCP Apps 来源', 'MCP Apps origin')),
    _text(
      'plugins.content_listen',
      () => tr('MCP Apps 监听地址', 'MCP Apps listen address'),
    ),
    _tags(
      'plugins.insecure_hosts',
      () => tr('不安全的主机（仅开发）', 'Insecure hosts (development)'),
    ),
    _number(
      'plugins.defer_tools_above',
      () => tr('工具超过此数时按需加载', 'Load tools on demand above'),
      plainScales,
    ),
  ],
  'search' => [
    _toggle('search.enabled', () => tr('搜索与记忆', 'Search and memory')),
    _choice(
      'search.embedding_provider',
      () => tr('向量提供方', 'Embedding provider'),
      ['openai', 'local'],
    ),
    _text('search.embedding_url', () => tr('向量服务地址', 'Embedding URL')),
    _text('search.embedding_model', () => tr('向量模型', 'Embedding model')),
    _env(
      'search.embedding_key_env',
      () => tr('向量服务密钥', 'Embedding key'),
      allowDefault: true,
    ),
    _text(
      'search.local_embedding_model',
      () => tr('本地向量模型', 'Local embedding model'),
    ),
    _number(
      'search.memory_budget_chars',
      () => tr('注入记忆的字数上限', 'Memory budget (characters)'),
      plainScales,
    ),
    _number(
      'search.max_attempts',
      () => tr('索引重试次数', 'Index attempts'),
      plainScales,
    ),
  ],
  'push' => [
    _toggle('push.enabled', () => tr('Android 推送', 'Android notifications')),
    _text(
      'push.fcm_credentials',
      () => tr('FCM 服务账号文件', 'FCM service account file'),
    ),
    _text('push.fcm_endpoint', () => tr('FCM 地址', 'FCM endpoint')),
  ],
  _ => [
    _number(
      'events_retention_days',
      () => tr('事件保留', 'Keep events for'),
      dayScales,
    ),
    _number(
      'retention.usage_days',
      () => tr('用量记录保留', 'Keep usage records for'),
      dayScales,
      hint: _zero('永久', 'forever'),
    ),
    _number(
      'retention.audit_days',
      () => tr('审计日志保留', 'Keep audit logs for'),
      dayScales,
      hint: _zero('永久', 'forever'),
    ),
    _number(
      'work.checkpoint_retention_days',
      () => tr('检查点保留', 'Keep checkpoints for'),
      dayScales,
      hint: _zero('不按时间删除', 'not by age'),
    ),
    _number(
      'work.checkpoint_keep_last',
      () => tr('每个对话保留的检查点', 'Checkpoints per conversation'),
      plainScales,
      hint: _zero('不限', 'no limit'),
    ),
    _number(
      'work.checkpoint_gc_grace_secs',
      () => tr('检查点删除前的宽限', 'Grace before deleting checkpoints'),
      secondScales,
    ),
  ],
};

/// A model route, edited in a dialog: null when dropped.
Future<Map<String, dynamic>?> editModelRoute(
  BuildContext context,
  Map<String, dynamic>? route,
  FieldInput input,
) => showDialog<Map<String, dynamic>>(
  context: context,
  builder: (_) => _ObjectDialog(
    title: tr('模型路由', 'Model route'),
    initial: {
      'id': '',
      'protocol': 'anthropic',
      'harnesses': ['chat'],
      'inputs': ['image', 'pdf'],
      'upstreams': <dynamic>[],
      ...?route,
    },
    fields: (draft, set) => [
      _Labeled(
        tr('模型 ID', 'Model id'),
        TextValueField(
          value: draft['id'],
          onChanged: (v) => set('id', v ?? ''),
        ),
      ),
      _Labeled(
        tr('协议', 'Protocol'),
        ChoiceField<String>(
          value: '${draft['protocol']}',
          options: [for (final p in _protocols) (p, p)],
          onChanged: (v) => set('protocol', v),
        ),
      ),
      _Labeled(
        'Harness',
        MultiField(
          values: strings(draft['harnesses']),
          options: knownHarnesses(input.config),
          onChanged: (v) => set('harnesses', v),
        ),
      ),
      _Labeled(
        tr('可读的文件', 'Reads files'),
        MultiField(
          values: strings(draft['inputs']),
          options: [('image', tr('图片', 'Images')), ('pdf', 'PDF')],
          onChanged: (v) => set('inputs', v),
        ),
      ),
      _Labeled(
        tr('上游', 'Upstreams'),
        _Upstreams(
          values: [
            for (final u in draft['upstreams'] as List)
              Map<String, dynamic>.from(u),
          ],
          env: input.env,
          onChanged: (v) => set('upstreams', v),
        ),
      ),
    ],
  ),
);

/// An ACP harness, edited in a dialog: null when dropped.
Future<Map<String, dynamic>?> editAcp(
  BuildContext context,
  Map<String, dynamic>? acp,
) => showDialog<Map<String, dynamic>>(
  context: context,
  builder: (_) => _ObjectDialog(
    title: tr('ACP Harness', 'ACP harness'),
    initial: {
      'name': '',
      'label': '',
      'command': '',
      'args': <dynamic>[],
      'env': <String, dynamic>{},
      'model_option': '{model}',
      'protocol': 'anthropic',
      'model': '',
      'version': '',
      ...?acp,
    },
    fields: (draft, set) => [
      for (final (key, label) in [
        ('name', tr('名称', 'Name')),
        ('label', tr('显示名', 'Label')),
        ('command', tr('命令', 'Command')),
        ('model', tr('默认模型', 'Default model')),
        ('model_option', tr('模型选项', 'Model option')),
        ('version', tr('版本', 'Version')),
      ])
        _Labeled(
          label,
          TextValueField(
            value: '${draft[key] ?? ''}',
            onChanged: (v) => set(key, v ?? ''),
          ),
        ),
      _Labeled(
        tr('协议', 'Protocol'),
        ChoiceField<String>(
          value: '${draft['protocol']}',
          options: [for (final p in _protocols) (p, p)],
          onChanged: (v) => set('protocol', v),
        ),
      ),
      _Labeled(
        tr('参数', 'Arguments'),
        TagsField(
          values: strings(draft['args']),
          onChanged: (v) => set('args', v),
        ),
      ),
      _Labeled(
        tr('环境变量', 'Environment'),
        KeyValueField(
          values: {
            for (final MapEntry(:key, :value) in (draft['env'] as Map).entries)
              '$key': '$value',
          },
          onChanged: (v) => set('env', v),
        ),
      ),
    ],
  ),
);

class _Labeled extends StatelessWidget {
  final String label;
  final Widget child;
  const _Labeled(this.label, this.child);
  @override
  Widget build(BuildContext context) => Padding(
    padding: const EdgeInsets.only(bottom: Space.edge),
    child: Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        Text(
          label,
          style: TextStyle(
            fontSize: FontSizes.label,
            color: HubColors.of(context).subtle,
          ),
        ),
        const SizedBox(height: 4),
        child,
      ],
    ),
  );
}

/// An object's fields in a dialog, saved as a whole.
class _ObjectDialog extends StatefulWidget {
  final String title;
  final Map<String, dynamic> initial;
  final List<Widget> Function(
    Map<String, dynamic> draft,
    void Function(String key, Object? value) set,
  )
  fields;
  const _ObjectDialog({
    required this.title,
    required this.initial,
    required this.fields,
  });
  @override
  State<_ObjectDialog> createState() => _ObjectDialogState();
}

class _ObjectDialogState extends State<_ObjectDialog> {
  late final draft = Map<String, dynamic>.from(widget.initial);

  @override
  Widget build(BuildContext context) => AlertDialog(
    title: Text(widget.title),
    content: SizedBox(
      width: 520,
      child: SingleChildScrollView(
        child: Column(
          mainAxisSize: MainAxisSize.min,
          crossAxisAlignment: CrossAxisAlignment.stretch,
          children: widget.fields(
            draft,
            (key, value) => setState(() => draft[key] = value),
          ),
        ),
      ),
    ),
    actions: [
      TextButton(
        onPressed: () => Navigator.pop(context),
        child: Text(tr('取消', 'Cancel')),
      ),
      FilledButton(
        onPressed: () => Navigator.pop(context, draft),
        child: Text(tr('完成', 'Done')),
      ),
    ],
  );
}

/// A route's upstreams: where it is served, tried by priority, spread by
/// weight.
class _Upstreams extends StatefulWidget {
  final List<Map<String, dynamic>> values;
  final Map<String, bool> env;
  final ValueChanged<List<Map<String, dynamic>>> onChanged;
  const _Upstreams({
    required this.values,
    required this.env,
    required this.onChanged,
  });
  @override
  State<_Upstreams> createState() => _UpstreamsState();
}

class _UpstreamsState extends State<_Upstreams> {
  /// Bumped when one is removed, so the fields after it start over.
  int generation = 0;

  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    final values = widget.values;
    void set(int i, String key, Object? value) =>
        widget.onChanged([...values]..[i] = {...values[i], key: value});
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        for (final (i, u) in values.indexed)
          Container(
            key: ValueKey('$generation/$i'),
            margin: const EdgeInsets.only(bottom: Space.gap),
            padding: const EdgeInsets.all(Space.gap),
            decoration: BoxDecoration(
              borderRadius: BorderRadius.circular(Radii.field),
              border: Border.all(color: c.border),
            ),
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.stretch,
              children: [
                Row(
                  children: [
                    Expanded(
                      child: TextValueField(
                        value: '${u['url'] ?? ''}',
                        onChanged: (v) => set(i, 'url', v ?? ''),
                      ),
                    ),
                    IconButton(
                      tooltip: tr('移除', 'Remove'),
                      icon: const Icon(Icons.close, size: 16),
                      onPressed: () {
                        setState(() => generation++);
                        widget.onChanged([...values]..removeAt(i));
                      },
                    ),
                  ],
                ),
                Wrap(
                  spacing: Space.edge,
                  runSpacing: Space.gap,
                  crossAxisAlignment: WrapCrossAlignment.center,
                  children: [
                    EnvField(
                      value: '${u['key_env'] ?? ''}',
                      env: widget.env,
                      allowDefault: true,
                      onChanged: (v) => set(i, 'key_env', v),
                    ),
                    SizedBox(
                      width: 160,
                      child: TextValueField(
                        value: '${u['model'] ?? ''}',
                        onChanged: (v) => set(i, 'model', v ?? ''),
                      ),
                    ),
                    Text(tr('优先级', 'Priority')),
                    NumberField(
                      value: (u['priority'] as num? ?? 0).toInt(),
                      scales: plainScales,
                      onChanged: (v) => set(i, 'priority', v),
                    ),
                    Text(tr('权重', 'Weight')),
                    NumberField(
                      value: (u['weight'] as num? ?? 1).toInt(),
                      scales: plainScales,
                      onChanged: (v) => set(i, 'weight', v),
                    ),
                  ],
                ),
              ],
            ),
          ),
        Align(
          alignment: Alignment.centerLeft,
          child: TextButton.icon(
            icon: const Icon(Icons.add, size: 16),
            label: Text(tr('添加上游', 'Add upstream')),
            onPressed: () => widget.onChanged([
              ...values,
              {
                'url': '',
                'key_env': '',
                'model': '',
                'priority': 0,
                'weight': 1,
              },
            ]),
          ),
        ),
      ],
    );
  }
}
