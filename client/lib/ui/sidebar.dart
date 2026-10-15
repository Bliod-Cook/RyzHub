import 'package:flutter/material.dart';

import '../controller.dart';
import '../l10n.dart';
import '../theme.dart';
import 'common.dart';
import 'dialogs.dart';
import 'icons.dart';

class Sidebar extends StatelessWidget {
  final HubController hub;
  final HubActions app;
  final bool drawer;
  const Sidebar({
    super.key,
    required this.hub,
    required this.app,
    this.drawer = false,
  });

  Widget conversationRow(Map<String, dynamic> c, {bool indent = false}) =>
      ConversationRow(
        key: ValueKey('conv-row:${c['id']}'),
        hub: hub,
        conversation: c,
        indent: indent,
        onTap: () => app.openConversation(c['id']),
        run: app.run,
      );

  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    final pinned = hub.pinned, recents = hub.recents;
    return Material(
      color: c.sidebar,
      child: SafeArea(
        right: false,
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.stretch,
          children: [
            Padding(
              padding: const EdgeInsets.fromLTRB(
                Space.edge,
                Space.gap,
                Space.gap,
                6,
              ),
              child: SizedBox(
                height: 40,
                child: Row(
                  children: [
                    Icon(Icons.hub_outlined, size: 20, color: c.text),
                    const SizedBox(width: Space.gap),
                    Text(
                      'RyzHub',
                      style: TextStyle(
                        fontSize: FontSizes.title,
                        fontWeight: FontWeight.w600,
                        color: c.text,
                      ),
                    ),
                    const SizedBox(width: 2),
                    ConnectionStatus(
                      connection: hub.connection,
                      onReconnect: hub.reconnect,
                    ),
                    const Spacer(),
                    IconButton(
                      tooltip: tr('搜索对话', 'Search chats'),
                      icon: const Icon(Icons.search, size: 20),
                      onPressed: app.openSearch,
                    ),
                    IconButton(
                      tooltip: drawer
                          ? tr('关闭侧边栏', 'Close sidebar')
                          : tr('收起侧边栏', 'Collapse sidebar'),
                      icon: const HubIcon(HubIcons.sidebarExpanded),
                      onPressed: drawer
                          ? () => Navigator.pop(context)
                          : app.toggleSidebar,
                    ),
                  ],
                ),
              ),
            ),
            NavRow(
              leading: HubIcon(HubIcons.newChat, size: 18, color: c.text),
              label: tr('新建对话', 'New chat'),
              selected: hub.isDraft && hub.project == null && !hub.libraryOpen,
              onTap: app.newChat,
            ),
            NavRow(
              leading: HubIcon(HubIcons.library, size: 18, color: c.text),
              label: tr('文件库', 'Library'),
              selected: hub.libraryOpen,
              onTap: app.openLibrary,
            ),
            NavRow(
              icon: Icons.archive_outlined,
              label: tr('已归档', 'Archived'),
              onTap: () => app.openSettings('data'),
              trailing: hub.archived.isEmpty
                  ? null
                  : Text(
                      '${hub.archived.length}',
                      style: TextStyle(
                        fontSize: FontSizes.caption,
                        color: c.subtle,
                      ),
                    ),
            ),
            Expanded(
              child: ListView(
                padding: const EdgeInsets.only(bottom: 12),
                children: [
                  if (pinned.isNotEmpty) ...[
                    _section('pinned', tr('已置顶', 'Pinned')),
                    if (!FoldHeader.folded('pinned'))
                      for (final x in pinned) conversationRow(x),
                  ],
                  _section(
                    'projects',
                    tr('项目', 'Projects'),
                    trailing: SizedBox(
                      width: 30,
                      height: 26,
                      child: IconButton(
                        padding: EdgeInsets.zero,
                        // Like the header beside it: pointer feedback only.
                        style: const ButtonStyle(
                          overlayColor: WidgetStatePropertyAll(
                            Colors.transparent,
                          ),
                        ),
                        tooltip: tr('新建项目', 'New project'),
                        icon: const Icon(
                          Icons.create_new_folder_outlined,
                          size: 17,
                        ),
                        onPressed: () => app.manageProjects(),
                      ),
                    ),
                  ),
                  if (!FoldHeader.folded('projects')) ...[
                    if (hub.activeProjects.isEmpty)
                      EmptyHint(tr('暂无项目', 'No projects')),
                    for (final p in hub.activeProjects) ...[
                      ProjectRow(
                        key: ValueKey('project-row:${p['id']}'),
                        project: p,
                        open: hub.contextProject == p['id'],
                        selected: hub.isDraft && hub.project == p['id'],
                        onTap: () => app.openProject(p['id']),
                        onEdit: () => app.manageProjects(p),
                        onDelete: () async {
                          if (await confirmDialog(
                            context,
                            title: tr('删除项目？', 'Delete project?'),
                            message: tr(
                              '“${p['name']}”及其对话会被删除，30 天内可以恢复。',
                              '"${p['name']}" and its chats will be deleted. You can restore it within 30 days.',
                            ),
                            confirm: tr('删除', 'Delete'),
                            danger: true,
                          )) {
                            await app.run(() => hub.deleteProject(p['id']));
                          }
                        },
                      ),
                      if (hub.contextProject == p['id'])
                        for (final x in hub.projectConversations(p['id']))
                          conversationRow(x, indent: true),
                    ],
                  ],
                  _section('recents', tr('最近', 'Recents')),
                  if (!FoldHeader.folded('recents')) ...[
                    if (recents.isEmpty) EmptyHint(tr('暂无对话', 'No chats yet')),
                    for (final x in recents) conversationRow(x),
                  ],
                ],
              ),
            ),
            Divider(color: c.border),
            AccountButton(hub: hub, app: app),
          ],
        ),
      ),
    );
  }
}

/// The collapsed sidebar on wide screens.
class SidebarRail extends StatelessWidget {
  final HubController hub;
  final HubActions app;
  const SidebarRail({super.key, required this.hub, required this.app});
  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    return Material(
      color: c.sidebar,
      child: SafeArea(
        right: false,
        child: Column(
          children: [
            const SizedBox(height: 10),
            // The app mark until hovered, then the control it hides.
            Hover(
              builder: (context, hovered) => IconButton(
                tooltip: tr('打开侧边栏', 'Open sidebar'),
                icon: hovered || touchPlatform
                    ? const HubIcon(HubIcons.sidebarCollapsed)
                    : Icon(Icons.hub_outlined, size: 20, color: c.text),
                onPressed: app.toggleSidebar,
              ),
            ),
            ConnectionStatus(
              connection: hub.connection,
              onReconnect: hub.reconnect,
            ),
            IconButton(
              tooltip: tr('新建对话', 'New chat'),
              icon: HubIcon(HubIcons.newChat, color: c.text),
              onPressed: app.newChat,
            ),
            IconButton(
              tooltip: tr('搜索对话', 'Search chats'),
              icon: const Icon(Icons.search, size: 20),
              onPressed: app.openSearch,
            ),
            IconButton(
              tooltip: tr('文件库', 'Library'),
              isSelected: hub.libraryOpen,
              icon: HubIcon(HubIcons.library, color: c.text),
              onPressed: app.openLibrary,
            ),
            const Spacer(),
            AccountButton(hub: hub, app: app, compact: true),
            const SizedBox(height: Space.gap),
          ],
        ),
      ),
    );
  }
}

class ConversationRow extends StatelessWidget {
  final HubController hub;
  final Map<String, dynamic> conversation;
  final bool indent;
  final VoidCallback onTap;
  final RunAction run;
  const ConversationRow({
    super.key,
    required this.hub,
    required this.conversation,
    required this.onTap,
    required this.run,
    this.indent = false,
  });
  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    final unread = hub.unread(conversation);
    return HoverRow<String>(
      selected: conversation['id'] == hub.selected,
      indent: indent ? 22 : 6,
      onTap: onTap,
      menuTooltip: tr('对话选项', 'Chat options'),
      menu: (_) => conversationMenu(hub, conversation, header: false),
      onSelected: (v) =>
          run(() => conversationAction(context, hub, conversation, v)),
      trailing: SizedBox(
        width: 30,
        height: 32,
        child: PinButton(hub: hub, conv: conversation, run: run, compact: true),
      ),
      child: Row(
        children: [
          if (conversation['mode'] == 'work') ...[
            Icon(Icons.terminal, size: 15, color: c.subtle),
            const SizedBox(width: Space.gap),
          ],
          Expanded(
            child: Text(
              conversation['title'] ?? '',
              maxLines: 1,
              overflow: TextOverflow.ellipsis,
              style: TextStyle(
                fontSize: FontSizes.body,
                color: c.text,
                fontWeight: unread ? FontWeight.w600 : null,
              ),
            ),
          ),
          if (unread)
            Semantics(
              label: tr('有新消息', 'New messages'),
              child: Container(
                width: 7,
                height: 7,
                margin: const EdgeInsets.only(left: 6),
                decoration: BoxDecoration(
                  color: c.accent,
                  shape: BoxShape.circle,
                ),
              ),
            ),
        ],
      ),
    );
  }
}

class ProjectRow extends StatelessWidget {
  final Map<String, dynamic> project;
  final bool open, selected;
  final VoidCallback onTap, onEdit, onDelete;
  const ProjectRow({
    super.key,
    required this.project,
    required this.open,
    required this.selected,
    required this.onTap,
    required this.onEdit,
    required this.onDelete,
  });
  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    return HoverRow<String>(
      selected: selected,
      onTap: onTap,
      menuTooltip: tr('项目选项', 'Project options'),
      menu: (_) => [
        HubMenuItem(
          value: 'edit',
          height: 38,
          child: Row(
            children: [
              const Icon(Icons.edit_outlined, size: 17),
              const SizedBox(width: 12),
              Text(tr('编辑项目', 'Edit project')),
            ],
          ),
        ),
        HubMenuItem(
          value: 'delete',
          height: 38,
          child: Row(
            children: [
              Icon(Icons.delete_outline, size: 17, color: c.danger),
              const SizedBox(width: 12),
              Text(
                tr('删除项目', 'Delete project'),
                style: TextStyle(color: c.danger),
              ),
            ],
          ),
        ),
      ],
      onSelected: (value) => value == 'delete' ? onDelete() : onEdit(),
      child: Row(
        children: [
          Icon(
            open ? Icons.folder_open_outlined : Icons.folder_outlined,
            size: 18,
            color: c.text,
          ),
          const SizedBox(width: 10),
          Expanded(
            child: Text(
              project['name'] ?? '',
              maxLines: 1,
              overflow: TextOverflow.ellipsis,
              style: TextStyle(fontSize: FontSizes.body, color: c.text),
            ),
          ),
        ],
      ),
    );
  }
}

class AccountButton extends StatelessWidget {
  final HubController hub;
  final HubActions app;
  final bool compact;
  const AccountButton({
    super.key,
    required this.hub,
    required this.app,
    this.compact = false,
  });
  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    final name = hub.email.isEmpty
        ? tr('我的账户', 'My account')
        : hub.email.split('@').first;
    return LayoutBuilder(
      builder: (context, box) {
        // Expanded: the sidebar's width, inset like its rows. Collapsed: the
        // menu starts at the rail's outer edge.
        final width = compact ? 240.0 : box.maxWidth - 12;
        // A menu anchor flips above the tile when there is no room below, so
        // the menu never covers the tile it came from.
        return MenuAnchor(
          crossAxisUnconstrained: false,
          // Collapsed: beside the rail, its bottom on the avatar's bottom. A
          // root menu flipped above its anchor moves up by -dy, so -42 (inset,
          // tile padding and avatar) lands it there.
          alignmentOffset: compact ? const Offset(1, -42) : const Offset(6, 6),
          style: MenuStyle(
            alignment: compact
                ? AlignmentDirectional.bottomEnd
                : AlignmentDirectional.bottomStart,
            // Standard density: the compact desktop density would shave 8px
            // off the minimum width.
            visualDensity: VisualDensity.standard,
            minimumSize: WidgetStatePropertyAll(Size(width, 0)),
            maximumSize: WidgetStatePropertyAll(Size(width, 400)),
          ),
          menuChildren: [
            Padding(
              padding: const EdgeInsets.fromLTRB(
                Space.inset,
                Space.gap,
                Space.inset,
                Space.gap,
              ),
              child: Row(
                children: [
                  Icon(
                    Icons.account_circle_outlined,
                    size: 18,
                    color: c.subtle,
                  ),
                  const SizedBox(width: 12),
                  Flexible(
                    child: Text(
                      hub.email,
                      overflow: TextOverflow.ellipsis,
                      style: TextStyle(
                        fontSize: FontSizes.label,
                        color: c.subtle,
                      ),
                    ),
                  ),
                ],
              ),
            ),
            const MenuDivider(),
            MenuItemButton(
              leadingIcon: const Icon(Icons.settings_outlined, size: 18),
              onPressed: app.openSettings,
              child: Text(tr('设置', 'Settings')),
            ),
            MenuItemButton(
              leadingIcon: const Icon(Icons.keyboard_outlined, size: 18),
              onPressed: () => app.openSettings('keyboard'),
              child: Text(tr('键盘快捷键', 'Keyboard shortcuts')),
            ),
            const MenuDivider(),
            MenuItemButton(
              leadingIcon: const Icon(Icons.logout, size: 18),
              onPressed: app.logout,
              child: Text(tr('退出登录', 'Log out')),
            ),
          ],
          builder: (context, controller, _) => Semantics(
            container: true,
            button: true,
            label: tr('账户', 'Account'),
            // Inset so the highlight keeps clear of the sidebar edges.
            child: Padding(
              padding: compact
                  ? const EdgeInsets.all(Space.gap)
                  : const EdgeInsets.all(6),
              child: InkWell(
                borderRadius: BorderRadius.circular(Radii.control),
                onTap: () =>
                    controller.isOpen ? controller.close() : controller.open(),
                // Collapsed, the tile spans the rail so the menu can attach to
                // its outer edge.
                child: compact
                    ? SizedBox(
                        width: double.infinity,
                        child: Padding(
                          padding: const EdgeInsets.all(6),
                          child: Center(child: Avatar(hub.email)),
                        ),
                      )
                    : Padding(
                        padding: const EdgeInsets.fromLTRB(
                          Space.gap,
                          Space.gap,
                          Space.gap,
                          Space.gap,
                        ),
                        child: Row(
                          children: [
                            Avatar(hub.email),
                            const SizedBox(width: 10),
                            Expanded(
                              child: Text(
                                name,
                                maxLines: 1,
                                overflow: TextOverflow.ellipsis,
                                style: TextStyle(
                                  fontSize: FontSizes.body,
                                  color: c.text,
                                ),
                              ),
                            ),
                            Icon(Icons.unfold_more, size: 16, color: c.subtle),
                          ],
                        ),
                      ),
              ),
            ),
          ),
        );
      },
    );
  }
}

/// Connection health beside the app name. Healthy is only a status; any other
/// state turns it into the reconnect control.
class ConnectionStatus extends StatelessWidget {
  final String connection;
  final VoidCallback onReconnect;
  const ConnectionStatus({
    super.key,
    required this.connection,
    required this.onReconnect,
  });
  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    final label = connectionLabel(connection);
    final color = switch (connection) {
      'online' => c.success,
      'background' => c.subtle,
      'offline' => c.danger,
      _ => c.warning,
    };
    final dot = Container(
      width: 8,
      height: 8,
      decoration: BoxDecoration(color: color, shape: BoxShape.circle),
    );
    if (connection == 'online') {
      return Semantics(
        label: label,
        excludeSemantics: true,
        child: Tooltip(
          message: label,
          child: Padding(padding: const EdgeInsets.all(Space.gap), child: dot),
        ),
      );
    }
    return Semantics(
      container: true,
      button: true,
      label: tr('重新连接', 'Reconnect'),
      onTap: onReconnect,
      excludeSemantics: true,
      child: Tooltip(
        message: label,
        // Only the dot: its colour carries the state, the tooltip names it.
        child: InkWell(
          customBorder: const CircleBorder(),
          onTap: onReconnect,
          child: Padding(padding: const EdgeInsets.all(Space.gap), child: dot),
        ),
      ),
    );
  }
}

/// A sidebar section title that folds the section.
Widget _section(String id, String title, {Widget? trailing}) => FoldHeader(
  id: id,
  title: title,
  padding: const EdgeInsets.fromLTRB(Space.edge, Space.inset, Space.gap, 2),
  height: 30,
  trailing: trailing,
);
