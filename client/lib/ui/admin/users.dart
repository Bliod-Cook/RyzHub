import 'dart:async';

import 'package:flutter/material.dart';

import '../../controller.dart';
import '../../l10n.dart';
import '../../theme.dart';
import '../common.dart';
import '../sectioned.dart';
import 'admin.dart';

/// What a user's status means to the admin.
String userStatus(Map user) {
  switch (user['status']) {
    case 'active':
      return tr('正常', 'Active');
    case 'disabled':
      return tr('已禁用', 'Disabled');
    case 'deletion_pending':
      final at = (user['delete_after_ms'] as num?)?.toInt() ?? 0;
      final left = DateTime.fromMillisecondsSinceEpoch(at)
          .difference(DateTime.now())
          .inHours;
      final days = (left / 24).ceil().clamp(0, 999);
      return tr('待删除（$days 天后清除）', 'Pending deletion (purged in $days days)');
    default:
      return user['purging'] == true
          ? tr('清除中', 'Purging')
          : tr('已删除', 'Deleted');
  }
}

/// The instance's users: find one, see what they have, disable, enable,
/// delete or restore them.
class Users extends StatefulWidget {
  final HubController hub;
  const Users({super.key, required this.hub});
  @override
  State<Users> createState() => _UsersState();
}

class _UsersState extends State<Users> {
  final users = <Map<String, dynamic>>[];
  String query = '', status = '', error = '';
  String? next, selected;
  bool loading = false;
  Timer? typing;

  @override
  void initState() {
    super.initState();
    load();
  }

  @override
  void dispose() {
    typing?.cancel();
    super.dispose();
  }

  /// The first page, or the one after [next] when [more].
  Future<void> load({bool more = false}) async {
    setState(() {
      loading = true;
      error = '';
    });
    final params = {
      if (query.isNotEmpty) 'q': query,
      if (status.isNotEmpty) 'status': status,
      if (more && next != null) 'before': next!,
    };
    try {
      final page = await widget.hub.admin(
        Uri(path: '/admin/users', queryParameters: params).toString(),
      );
      if (!mounted) return;
      setState(() {
        if (!more) users.clear();
        users.addAll(
          (page['users'] as List).map((u) => Map<String, dynamic>.from(u)),
        );
        next = page['next'] as String?;
      });
    } catch (e) {
      if (mounted) setState(() => error = errorText(e));
    }
    if (mounted) setState(() => loading = false);
  }

  /// [user] as the server now has it, in the list.
  void changed(Map<String, dynamic> user) => setState(() {
    final i = users.indexWhere((u) => u['id'] == user['id']);
    if (i >= 0) users[i] = {...users[i], ...user};
  });

  Widget list(BuildContext context) {
    final c = HubColors.of(context);
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        Row(
          children: [
            Expanded(
              child: TextField(
                decoration: InputDecoration(
                  hintText: tr('搜索邮箱或名称', 'Search email or name'),
                  prefixIcon: const Icon(Icons.search, size: 18),
                  isDense: true,
                ),
                onChanged: (v) {
                  typing?.cancel();
                  typing = Timer(const Duration(milliseconds: 300), () {
                    query = v.trim();
                    load();
                  });
                },
              ),
            ),
            const SizedBox(width: Space.gap),
            ChoicePill<String>(
              tooltip: tr('状态', 'Status'),
              value: status,
              options: [
                ('', tr('全部', 'All')),
                ('active', tr('正常', 'Active')),
                ('disabled', tr('已禁用', 'Disabled')),
                ('deletion_pending', tr('待删除', 'Pending deletion')),
                ('deleted', tr('已删除', 'Deleted')),
              ],
              onChanged: (v) {
                setState(() => status = v);
                load();
              },
            ),
          ],
        ),
        const SizedBox(height: Space.gap),
        for (final u in users)
          NavRow(
            icon: u['is_instance_admin'] == true
                ? Icons.admin_panel_settings_outlined
                : Icons.person_outline,
            label: '${u['email']}',
            selected: selected == u['id'],
            trailing: u['status'] == 'active'
                ? null
                : Text(
                    userStatus(u),
                    style: TextStyle(
                      fontSize: FontSizes.caption,
                      color: c.subtle,
                    ),
                  ),
            onTap: () => setState(() => selected = '${u['id']}'),
          ),
        if (users.isEmpty && !loading && error.isEmpty)
          EmptyHint(tr('没有匹配的用户', 'No matching users')),
        if (error.isNotEmpty)
          Padding(
            padding: const EdgeInsets.all(Space.gap),
            child: Text(error, style: TextStyle(color: c.danger)),
          ),
        if (next != null)
          TextButton(
            onPressed: loading ? null : () => load(more: true),
            child: Text(tr('加载更多', 'Load more')),
          ),
      ],
    );
  }

  @override
  Widget build(BuildContext context) => LayoutBuilder(
    builder: (context, constraints) {
      final detail = selected == null
          ? null
          : _Detail(
              key: ValueKey(selected),
              hub: widget.hub,
              id: selected!,
              onChanged: changed,
              onClose: () => setState(() => selected = null),
            );
      if (constraints.maxWidth < 760) {
        return detail ?? list(context);
      }
      return Row(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          SizedBox(width: 380, child: list(context)),
          const SizedBox(width: 24),
          Expanded(
            child:
                detail ??
                EmptyHint(tr('选择一个用户查看详情', 'Select a user to see details')),
          ),
        ],
      );
    },
  );
}

class _Detail extends StatefulWidget {
  final HubController hub;
  final String id;
  final ValueChanged<Map<String, dynamic>> onChanged;
  final VoidCallback onClose;
  const _Detail({
    super.key,
    required this.hub,
    required this.id,
    required this.onChanged,
    required this.onClose,
  });
  @override
  State<_Detail> createState() => _DetailState();
}

class _DetailState extends State<_Detail> {
  Map<String, dynamic>? user;
  String error = '';
  bool busy = false;

  @override
  void initState() {
    super.initState();
    widget.hub
        .admin('/admin/users/${widget.id}')
        .then(
          (u) {
            if (mounted) setState(() => user = Map<String, dynamic>.from(u));
          },
          onError: (Object e) {
            if (mounted) setState(() => error = errorText(e));
          },
        );
  }

  Future<void> act(
    String path, {
    String? method,
    Map<String, dynamic>? body,
  }) async {
    setState(() {
      busy = true;
      error = '';
    });
    try {
      final changed = Map<String, dynamic>.from(
        await widget.hub.admin(path, method: method, body: body ?? {}),
      );
      if (!mounted) return;
      setState(() => user = {...?user, ...changed});
      widget.onChanged(changed);
    } catch (e) {
      if (mounted) setState(() => error = errorText(e));
    }
    if (mounted) setState(() => busy = false);
  }

  Future<void> disable() async {
    if (await confirmTyped(
      context,
      title: tr('禁用这个用户？', 'Disable this user?'),
      message: tr(
        '会立即退出所有设备，并停止进行中的任务。重新启用后需要重新登录。',
        'Signs them out everywhere and stops their runs at once. They sign in again once enabled.',
      ),
      confirm: tr('禁用', 'Disable'),
    )) {
      await act(
        '/admin/users/${widget.id}',
        method: 'PATCH',
        body: {'status': 'disabled'},
      );
    }
  }

  Future<void> delete() async {
    final email = '${user!['email']}';
    if (await confirmTyped(
      context,
      title: tr('删除这个用户？', 'Delete this user?'),
      message: tr(
        '账号立即停用，30 天内可撤销；之后清除其个人空间的全部数据。输入邮箱以确认。',
        'The account stops at once and can be restored for 30 days; then everything in their personal space is purged. Type the email to confirm.',
      ),
      confirm: tr('删除', 'Delete'),
      typed: email,
    )) {
      await act('/admin/users/${widget.id}', method: 'DELETE');
    }
  }

  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    final u = user;
    if (u == null) {
      return error.isEmpty
          ? const Center(child: CircularProgressIndicator(strokeWidth: 2))
          : Text(error, style: TextStyle(color: c.danger));
    }
    final status = '${u['status']}';
    final tokens = u['tokens_30d'] as Map? ?? {};
    final identities = (u['identities'] as List? ?? []).cast<Map>();
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        Row(
          children: [
            IconButton(
              tooltip: tr('关闭', 'Close'),
              icon: const Icon(Icons.close, size: 18),
              onPressed: widget.onClose,
            ),
            Expanded(
              child: SelectableText(
                '${u['email']}',
                style: TextStyle(fontSize: FontSizes.title, color: c.text),
              ),
            ),
          ],
        ),
        const SizedBox(height: Space.gap),
        SettingsCard(
          children: [
            SettingRow(title: tr('名称', 'Name'), trailing: Text('${u['name']}')),
            SettingRow(
              title: tr('状态', 'Status'),
              trailing: Text(userStatus(u)),
            ),
            if (u['is_instance_admin'] == true)
              SettingRow(
                title: tr('实例管理员', 'Instance admin'),
                trailing: Icon(Icons.verified_user_outlined, color: c.accent),
              ),
            SettingRow(
              title: tr('注册时间', 'Joined'),
              trailing: Text(formatTime(u['created_at_ms'] as num?)),
            ),
            SettingRow(
              title: tr('最近登录', 'Last sign-in'),
              trailing: Text(formatTime(u['last_sign_in_ms'] as num?)),
            ),
            SettingRow(
              title: tr('登录方式', 'Signs in with'),
              trailing: Text(
                [
                  if (u['password'] == true) tr('密码', 'Password'),
                  for (final p in u['providers'] as List? ?? []) '$p',
                ].join(', '),
              ),
            ),
            for (final i in identities)
              SettingRow(
                title: '${i['provider']}',
                subtitle: '${i['email'] ?? i['subject']}',
                trailing: Text(formatTime(i['created_at_ms'] as num?)),
              ),
            SettingRow(
              title: tr('已登录设备', 'Signed-in devices'),
              trailing: Text('${u['devices'] ?? 0}'),
            ),
            SettingRow(
              title: tr('对话', 'Conversations'),
              trailing: Text('${u['conversations'] ?? 0}'),
            ),
            SettingRow(
              title: tr('近 30 天 token', 'Tokens, last 30 days'),
              trailing: Text(
                '${formatCount(tokens['input'] as num? ?? 0)} / ${formatCount(tokens['output'] as num? ?? 0)}',
              ),
            ),
          ],
        ),
        if (status != 'deleted')
          Wrap(
            spacing: Space.gap,
            runSpacing: Space.gap,
            children: [
              if (status == 'active')
                OutlinedButton(
                  onPressed: busy ? null : disable,
                  child: Text(tr('禁用', 'Disable')),
                ),
              if (status == 'disabled')
                OutlinedButton(
                  onPressed: busy
                      ? null
                      : () => act(
                          '/admin/users/${widget.id}',
                          method: 'PATCH',
                          body: {'status': 'active'},
                        ),
                  child: Text(tr('启用', 'Enable')),
                ),
              if (status == 'deletion_pending')
                OutlinedButton(
                  onPressed: busy
                      ? null
                      : () => act('/admin/users/${widget.id}/restore'),
                  child: Text(tr('撤销删除', 'Restore')),
                ),
              if (status != 'deletion_pending')
                OutlinedButton(
                  style: OutlinedButton.styleFrom(foregroundColor: c.danger),
                  onPressed: busy ? null : delete,
                  child: Text(tr('删除', 'Delete')),
                ),
            ],
          ),
        if (error.isNotEmpty)
          Padding(
            padding: const EdgeInsets.only(top: Space.gap),
            child: Text(error, style: TextStyle(color: c.danger)),
          ),
      ],
    );
  }
}

/// A confirmation that, given [typed], is only accepted once that text is
/// typed in.
Future<bool> confirmTyped(
  BuildContext context, {
  required String title,
  required String message,
  required String confirm,
  String? typed,
}) async {
  final c = HubColors.of(context);
  final input = TextEditingController();
  final result = await showDialog<bool>(
    context: context,
    builder: (context) => StatefulBuilder(
      builder: (context, setState) => AlertDialog(
        title: Text(title),
        content: SizedBox(
          width: 400,
          child: Column(
            mainAxisSize: MainAxisSize.min,
            crossAxisAlignment: CrossAxisAlignment.stretch,
            children: [
              Text(message),
              if (typed != null) ...[
                const SizedBox(height: Space.edge),
                TextField(
                  controller: input,
                  autofocus: true,
                  decoration: InputDecoration(hintText: typed),
                  onChanged: (_) => setState(() {}),
                ),
              ],
            ],
          ),
        ),
        actions: [
          TextButton(
            onPressed: () => Navigator.pop(context, false),
            child: Text(tr('取消', 'Cancel')),
          ),
          FilledButton(
            style: FilledButton.styleFrom(
              backgroundColor: c.danger,
              foregroundColor: Colors.white,
            ),
            onPressed: typed == null || input.text.trim() == typed
                ? () => Navigator.pop(context, true)
                : null,
            child: Text(confirm),
          ),
        ],
      ),
    ),
  );
  // The dialog's route may still be animating its text field out.
  return result ?? false;
}
