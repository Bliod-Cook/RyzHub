import 'package:flutter/material.dart';

import '../controller.dart';
import '../l10n.dart';
import '../theme.dart';
import 'attachments.dart' show fileTime;
import 'common.dart';
import 'dialogs.dart';
import 'sectioned.dart';

/// The priorities a memory can take, highest first.
List<(int, String)> _priorities() => [
  (100, tr('最高', 'Highest')),
  (50, tr('高', 'High')),
  (0, tr('普通', 'Normal')),
  (-50, tr('低', 'Low')),
  (-100, tr('最低', 'Lowest')),
];

class MemorySettings extends StatefulWidget {
  final HubController hub;
  const MemorySettings({super.key, required this.hub});
  @override
  State<MemorySettings> createState() => _MemorySettingsState();
}

class _MemorySettingsState extends State<MemorySettings> {
  final loader = GlobalKey<LoaderState<dynamic>>();
  final search = TextEditingController();
  String? project;
  String sort = 'priority', query = '', error = '';
  HubController get hub => widget.hub;

  @override
  void dispose() {
    search.dispose();
    super.dispose();
  }

  Future<void> run(Future<void> Function() action) async {
    setState(() => error = '');
    try {
      await action();
    } catch (e) {
      if (mounted) setState(() => error = errorText(e));
    }
    loader.currentState?.reload();
  }

  /// Show memories of another scope, order or search.
  void show(VoidCallback change) {
    setState(change);
    loader.currentState?.reload();
  }

  Future<void> edit([Map? memory]) async {
    final content = await textDialog(
      context,
      tr('记忆内容', 'Memory'),
      memory?['content'] ?? '',
    );
    if (content == null || content.trim().isEmpty || !mounted) return;
    await run(
      () => hub.saveMemory(
        content: content,
        project: project,
        replace: memory?['id'],
        priority: memory?['priority'] ?? 0,
      ),
    );
  }

  Future<void> delete(Map memory) => run(() async {
    final result = await hub.deleteMemory('${memory['id']}');
    if (!mounted) return;
    ScaffoldMessenger.of(context).showSnackBar(
      SnackBar(
        content: Text(tr('记忆已删除', 'Memory deleted')),
        action: SnackBarAction(
          label: tr('撤销', 'Undo'),
          onPressed: () => run(() => hub.undoMemory('${result['event_id']}')),
        ),
      ),
    );
  });

  @override
  Widget build(BuildContext context) => Loader<dynamic>(
    key: loader,
    load: () => hub.memories(project: project, sort: sort, query: query),
    builder: (context, data, _) => _memories(context, data as Map),
  );

  Widget _memories(BuildContext context, Map data) {
    final c = HubColors.of(context);
    final preferences = Map<String, dynamic>.from(data['preferences'] as Map);
    final policy = data['policy_enabled'] == true;
    final enabled = policy && preferences['enabled'] == true;
    final items = data['items'] as List;
    Future<void> toggle(String key, bool value) =>
        run(() => hub.setMemoryPreferences({...preferences, key: value}));
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        SettingsCard(
          children: [
            SettingRow(
              title: tr('启用记忆', 'Enable memory'),
              subtitle: policy
                  ? null
                  : tr('组织策略已关闭', 'Off by organization policy'),
              trailing: Switch(
                value: preferences['enabled'] == true,
                onChanged: policy ? (v) => toggle('enabled', v) : null,
              ),
            ),
            SettingRow(
              title: tr('引用历史对话', 'Reference chat history'),
              subtitle: tr(
                '关闭后摘要在 30 天内删除',
                'When off, digests are deleted within 30 days',
              ),
              trailing: Switch(
                value: preferences['reference_history'] == true,
                onChanged: (v) => toggle('reference_history', v),
              ),
            ),
          ],
        ),
        SettingsCard(
          children: [
            SettingRow(
              title: tr('记忆范围', 'Memory scope'),
              trailing: ChoicePill<String>(
                tooltip: tr('记忆范围', 'Memory scope'),
                value: project ?? '',
                options: [
                  ('', tr('个人记忆', 'Personal memories')),
                  for (final p in hub.activeProjects)
                    if (p['memory_mode'] == 'project_only')
                      (p['id'] as String, p['name'] as String),
                ],
                onChanged: (v) => show(() => project = v.isEmpty ? null : v),
              ),
            ),
            SettingRow(
              title: tr('排序', 'Sort'),
              trailing: ChoicePill<String>(
                tooltip: tr('排序', 'Sort'),
                value: sort,
                options: [
                  ('priority', tr('按优先级', 'By priority')),
                  ('time', tr('最近更新', 'Recently updated')),
                ],
                onChanged: (v) => show(() => sort = v),
              ),
            ),
          ],
        ),
        Row(
          children: [
            Expanded(
              child: TextField(
                controller: search,
                decoration: InputDecoration(
                  hintText: tr('搜索记忆', 'Search memories'),
                  prefixIcon: const Icon(Icons.search, size: 18),
                ),
                onSubmitted: (v) => show(() => query = v.trim()),
              ),
            ),
            const SizedBox(width: 12),
            FilledButton.icon(
              onPressed: enabled ? edit : null,
              icon: const Icon(Icons.add, size: 18),
              label: Text(tr('添加记忆', 'Add memory')),
            ),
          ],
        ),
        if (error.isNotEmpty)
          Padding(
            padding: const EdgeInsets.only(top: Space.gap),
            child: Text(error, style: TextStyle(color: c.danger)),
          ),
        const SizedBox(height: 16),
        SettingsCard(
          children: [
            if (items.isEmpty) SettingRow(title: tr('暂无记忆', 'No memories yet')),
            for (final item in items.cast<Map>())
              SettingRow(
                title: '${item['content']}',
                subtitle: fileTime(item['updated_at']),
                trailing: Row(
                  mainAxisSize: MainAxisSize.min,
                  children: [
                    ChoicePill<int>(
                      tooltip: tr('优先级', 'Priority'),
                      value: (item['priority'] as num).toInt(),
                      options: _priorities(),
                      onChanged: (priority) => run(
                        () => hub.saveMemory(
                          content: item['content'],
                          replace: item['id'],
                          project: project,
                          priority: priority,
                        ),
                      ),
                    ),
                    IconButton(
                      tooltip: tr('编辑记忆', 'Edit memory'),
                      icon: const Icon(Icons.edit_outlined, size: 18),
                      onPressed: enabled ? () => edit(item) : null,
                    ),
                    IconButton(
                      tooltip: tr('删除记忆', 'Delete memory'),
                      icon: const Icon(Icons.delete_outline, size: 18),
                      onPressed: () => delete(item),
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
