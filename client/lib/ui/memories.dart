import 'package:flutter/material.dart';

import '../controller.dart';
import '../l10n.dart';
import 'dialogs.dart';

class MemorySettings extends StatefulWidget {
  final HubController hub;
  const MemorySettings({super.key, required this.hub});
  @override
  State<MemorySettings> createState() => _MemorySettingsState();
}

class _MemorySettingsState extends State<MemorySettings> {
  List<dynamic> items = [];
  Map<String, dynamic> preferences = {
    'enabled': true,
    'reference_history': true,
  };
  String? project;
  String sort = 'priority';
  String query = '', error = '';
  bool busy = true, policy = true;
  int generation = 0;
  final search = TextEditingController();
  HubController get hub => widget.hub;
  @override
  void initState() {
    super.initState();
    load();
  }

  @override
  void dispose() {
    search.dispose();
    super.dispose();
  }

  Future<void> load() async {
    final turn = ++generation;
    try {
      final uri = Uri(
        path: '/orgs/${hub.org}/memories',
        queryParameters: {
          'project': ?project,
          'sort': sort,
          if (query.isNotEmpty) 'q': query,
        },
      );
      final data = await hub.request(uri.toString());
      if (!mounted || turn != generation) return;
      setState(() {
        items = data['items'];
        preferences = Map<String, dynamic>.from(data['preferences']);
        policy = data['policy_enabled'] == true;
        busy = false;
        error = '';
      });
    } catch (e) {
      if (mounted && turn == generation) {
        setState(() {
          error = errorText(e);
          busy = false;
        });
      }
    }
  }

  Future<void> run(Future<void> Function() action) async {
    if (mounted) {
      setState(() {
        busy = true;
        error = '';
      });
    }
    try {
      await action();
      if (mounted) await load();
    } catch (e) {
      if (mounted) {
        setState(() {
          busy = false;
          error = errorText(e);
        });
      }
    }
  }

  Future<void> edit([Map<String, dynamic>? memory]) async {
    final content = await textDialog(
      context,
      tr('记忆内容', 'Memory'),
      memory?['content'] ?? '',
    );
    if (content == null || content.trim().isEmpty || !mounted) return;
    await run(() async {
      await hub.request(
        '/orgs/${hub.org}/memories',
        body: {
          'content': content,
          'project_id': project,
          if (memory != null) 'replace': memory['id'],
          'priority': memory?['priority'] ?? 0,
        },
      );
    });
  }

  Future<void> toggle(String key, bool value) => run(() async {
    await hub.request(
      '/orgs/${hub.org}/memories/preferences',
      method: 'PATCH',
      body: {...preferences, key: value},
    );
  });
  @override
  Widget build(BuildContext context) => Column(
    crossAxisAlignment: CrossAxisAlignment.stretch,
    children: [
      SwitchListTile(
        title: Text(tr('启用记忆', 'Enable memory')),
        value: preferences['enabled'] == true,
        onChanged: busy || !policy ? null : (v) => toggle('enabled', v),
      ),
      SwitchListTile(
        title: Text(tr('引用历史对话', 'Reference chat history')),
        subtitle: Text(
          tr(
            '关闭后不再引用历史，摘要会在 30 天内删除。',
            'When disabled, history is not referenced and digests are deleted within 30 days.',
          ),
        ),
        value: preferences['reference_history'] == true,
        onChanged: busy ? null : (v) => toggle('reference_history', v),
      ),
      if (!policy)
        Text(tr('组织策略已关闭记忆。', 'Memory is disabled by organization policy.')),
      const SizedBox(height: 12),
      DropdownButtonFormField<String>(
        initialValue: project ?? '',
        decoration: InputDecoration(labelText: tr('记忆范围', 'Memory scope')),
        items: [
          DropdownMenuItem(
            value: '',
            child: Text(tr('个人记忆', 'Personal memories')),
          ),
          for (final p in hub.activeProjects.where(
            (p) => p['memory_mode'] == 'project_only',
          ))
            DropdownMenuItem(value: p['id'], child: Text(p['name'])),
        ],
        onChanged: busy
            ? null
            : (v) {
                setState(() {
                  project = v == '' ? null : v;
                  busy = true;
                });
                load();
              },
      ),
      const SizedBox(height: 12),
      DropdownButtonFormField<String>(
        initialValue: sort,
        decoration: InputDecoration(labelText: tr('排序', 'Sort')),
        items: [
          DropdownMenuItem(
            value: 'priority',
            child: Text(tr('按优先级', 'By priority')),
          ),
          DropdownMenuItem(
            value: 'time',
            child: Text(tr('最近更新', 'Recently updated')),
          ),
        ],
        onChanged: busy
            ? null
            : (v) {
                setState(() {
                  sort = v ?? 'priority';
                  busy = true;
                });
                load();
              },
      ),
      const SizedBox(height: 12),
      TextField(
        controller: search,
        decoration: InputDecoration(
          labelText: tr('搜索记忆', 'Search memories'),
          suffixIcon: IconButton(
            icon: const Icon(Icons.search),
            onPressed: () {
              query = search.text.trim();
              load();
            },
          ),
        ),
        onSubmitted: (v) {
          query = v.trim();
          load();
        },
      ),
      const SizedBox(height: 12),
      Align(
        alignment: Alignment.centerLeft,
        child: FilledButton.icon(
          onPressed: busy || !policy || preferences['enabled'] != true
              ? null
              : () => edit(),
          icon: const Icon(Icons.add),
          label: Text(tr('添加记忆', 'Add memory')),
        ),
      ),
      if (busy) const LinearProgressIndicator(),
      if (error.isNotEmpty)
        Text(
          error,
          style: TextStyle(color: Theme.of(context).colorScheme.error),
        ),
      if (!busy && items.isEmpty)
        Padding(
          padding: const EdgeInsets.all(16),
          child: Text(tr('暂无记忆', 'No memories yet')),
        ),
      for (final item in items)
        ListTile(
          title: Text(item['content']),
          subtitle: Text(
            '${tr('优先级', 'Priority')}: ${item['priority']} · ${item['updated_at']}',
          ),
          onTap: busy || !policy || preferences['enabled'] != true
              ? null
              : () => edit(Map<String, dynamic>.from(item)),
          trailing: Row(
            mainAxisSize: MainAxisSize.min,
            children: [
              IconButton(
                tooltip: tr('调整优先级', 'Adjust priority'),
                icon: const Icon(Icons.low_priority),
                onPressed: busy
                    ? null
                    : () async {
                        final value = await textDialog(
                          context,
                          tr('优先级（-100 到 100）', 'Priority (-100 to 100)'),
                          '${item['priority']}',
                        );
                        final priority = int.tryParse(value ?? '');
                        if (priority == null ||
                            priority < -100 ||
                            priority > 100) {
                          return;
                        }
                        await run(() async {
                          await hub.request(
                            '/orgs/${hub.org}/memories',
                            body: {
                              'content': item['content'],
                              'replace': item['id'],
                              'project_id': project,
                              'priority': priority,
                            },
                          );
                        });
                      },
              ),
              IconButton(
                tooltip: tr('删除记忆', 'Delete memory'),
                icon: const Icon(Icons.delete_outline),
                onPressed: busy
                    ? null
                    : () => run(() async {
                        final result = await hub.request(
                          '/orgs/${hub.org}/memories/${item['id']}',
                          method: 'DELETE',
                        );
                        if (context.mounted) {
                          ScaffoldMessenger.of(context).showSnackBar(
                            SnackBar(
                              content: Text(tr('记忆已删除', 'Memory deleted')),
                              action: SnackBarAction(
                                label: tr('撤销', 'Undo'),
                                onPressed: () => run(() async {
                                  await hub.request(
                                    '/orgs/${hub.org}/memory-events/${result['event_id']}/undo',
                                    body: {},
                                  );
                                }),
                              ),
                            ),
                          );
                        }
                      }),
              ),
            ],
          ),
        ),
    ],
  );
}
