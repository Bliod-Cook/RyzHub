import 'dart:convert';
import 'dart:math';

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';

import '../controller.dart';
import '../l10n.dart';
import '../prefs.dart';
import '../theme.dart';
import 'common.dart';
import 'composer.dart';
import 'dialogs.dart';
import 'icons.dart';
import 'login.dart';
import 'messages.dart';
import 'settings.dart';
import 'sidebar.dart';

class HubPage extends StatefulWidget {
  const HubPage({super.key});
  @override
  State<HubPage> createState() => _HubPageState();
}

class _HubPageState extends State<HubPage>
    with WidgetsBindingObserver
    implements HubActions {
  HubController hub = HubController();
  final composerKey = GlobalKey();
  final scaffoldKey = GlobalKey<ScaffoldState>();

  @override
  void initState() {
    super.initState();
    WidgetsBinding.instance.addObserver(this);
    hub.addListener(_changed);
    prefs.addListener(_changed);
    HardwareKeyboard.instance.addHandler(_shortcut);
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
    HardwareKeyboard.instance.removeHandler(_shortcut);
    prefs.removeListener(_changed);
    hub.dispose();
    super.dispose();
  }

  Future<void> action(
    Future<void> Function() task, {
    bool rethrowError = false,
  }) async {
    try {
      await task();
    } catch (e) {
      if (mounted) {
        ScaffoldMessenger.of(context).showSnackBar(
          SnackBar(
            content: Text(
              tr('操作失败：${errorText(e)}', 'Action failed: ${errorText(e)}'),
            ),
          ),
        );
      }
      if (rethrowError) rethrow;
    }
  }

  bool _shortcut(KeyEvent event) {
    if (event is! KeyDownEvent || !hub.signedIn || !mounted) return false;
    if (!(ModalRoute.of(context)?.isCurrent ?? true)) return false;
    final keys = HardwareKeyboard.instance;
    if (!keys.isControlPressed && !keys.isMetaPressed) return false;
    final key = event.logicalKey;
    if (keys.isShiftPressed && key == LogicalKeyboardKey.keyO) {
      newChat();
    } else if (keys.isShiftPressed && key == LogicalKeyboardKey.keyS) {
      toggleSidebar();
    } else if (!keys.isShiftPressed && key == LogicalKeyboardKey.keyK) {
      openSearch();
    } else if (key == LogicalKeyboardKey.comma) {
      openSettings();
    } else {
      return false;
    }
    return true;
  }

  bool get wide => MediaQuery.sizeOf(context).width >= 840;

  void closeDrawer() {
    final scaffold = scaffoldKey.currentState;
    if (scaffold?.isDrawerOpen ?? false) scaffold!.closeDrawer();
  }

  @override
  RunAction get run => action;

  @override
  void newChat() {
    closeDrawer();
    // The controller falls back to this device's default model for the mode.
    hub.startDraft(mode: prefs.defaultMode, harness: prefs.defaultHarness);
  }

  @override
  Future<void> openSearch() async {
    closeDrawer();
    final result = await showDialog<SearchResult>(
      context: context,
      builder: (_) => SearchDialog(hub: hub),
    );
    switch (result) {
      case SearchNewChat():
        newChat();
      case SearchConversation(:final id, :final itemId, :final seq):
        action(() => hub.selectSearch(id, itemId, seq));
      case SearchProject(:final id):
        openProject(id);
      case null:
    }
  }

  @override
  void toggleSidebar() {
    if (wide) {
      prefs.update((p) => p.sidebarCollapsed = !p.sidebarCollapsed);
    } else if (scaffoldKey.currentState?.isDrawerOpen ?? false) {
      closeDrawer();
    } else {
      scaffoldKey.currentState?.openDrawer();
    }
  }

  @override
  void openSettings([String? section]) {
    closeDrawer();
    Navigator.of(context).push(
      MaterialPageRoute<void>(
        builder: (_) => SettingsPage(hub: hub, app: this, initial: section),
      ),
    );
  }

  @override
  void openConversation(String id) {
    closeDrawer();
    action(() => hub.select(id));
  }

  @override
  void openProject(String id) {
    closeDrawer();
    hub.startDraft(project: id);
  }

  @override
  void manageProjects([Map<String, dynamic>? project]) {
    closeDrawer();
    showDialog<void>(
      context: context,
      builder: (_) => ProjectsDialog(hub: hub, initial: project),
    );
  }

  @override
  Future<void> logout() async {
    if (!await confirmDialog(
      context,
      title: tr('退出登录？', 'Log out?'),
      message: tr(
        '将退出 ${hub.email}。本机缓存会保留，下次登录可继续使用。',
        'You will be logged out of ${hub.email}. This device keeps its cache for your next sign-in.',
      ),
      confirm: tr('退出登录', 'Log out'),
    )) {
      return;
    }
    if (!mounted) return;
    Navigator.of(context).popUntil((route) => route.isFirst);
    final old = hub;
    old.removeListener(_changed);
    setState(() => hub = HubController()..addListener(_changed));
    try {
      await old.logout();
    } finally {
      old.dispose();
    }
  }

  Future<void> login(
    String server,
    String email,
    String password,
    bool register,
  ) {
    hub
      ..mode = prefs.defaultMode
      ..harness = prefs.defaultHarness;
    return hub.login(server, email, password, register);
  }

  void setMode(String mode) {
    if (hub.isDraft) {
      hub.updateDraft(() {
        hub.mode = mode;
        hub.draftModel = null;
      });
    } else if (hub.selectedConversation['mode'] != mode) {
      hub.startDraft(mode: mode, project: hub.contextProject);
    }
  }

  Widget header() {
    final c = HubColors.of(context);
    final conv = hub.selectedConversation;
    final title = hub.isDraft ? null : (conv['title'] ?? '') as String;
    final showPill = wide || hub.isDraft;
    return SizedBox(
      height: 56,
      width: double.infinity,
      child: LayoutBuilder(
        builder: (context, box) {
          final titleMax = showPill
              ? box.maxWidth / 2 - 120
              : box.maxWidth - 160;
          return Stack(
            alignment: Alignment.center,
            children: [
              Positioned.fill(
                child: Row(
                  children: [
                    const SizedBox(width: 8),
                    if (!wide)
                      IconButton(
                        tooltip: tr('打开侧边栏', 'Open sidebar'),
                        icon: const HubIcon(HubIcons.sidebarCollapsed),
                        onPressed: () => scaffoldKey.currentState?.openDrawer(),
                      ),
                    // The status lives in the sidebar; surface it here only
                    // when the drawer hides it and a reconnect is needed.
                    if (!wide && hub.connection != 'online')
                      ConnectionStatus(
                        connection: hub.connection,
                        onReconnect: hub.reconnect,
                      ),
                    if (title != null)
                      ConstrainedBox(
                        constraints: BoxConstraints(maxWidth: max(0, titleMax)),
                        child: Padding(
                          padding: const EdgeInsets.symmetric(horizontal: 8),
                          child: Semantics(
                            container: true,
                            header: true,
                            child: Text(
                              title,
                              maxLines: 1,
                              overflow: TextOverflow.ellipsis,
                              style: TextStyle(
                                fontSize: 15.5,
                                fontWeight: FontWeight.w500,
                                color: c.text,
                              ),
                            ),
                          ),
                        ),
                      ),
                    const Spacer(),
                    // Reserved like pin/delete: enabled once the server
                    // advertises `conversation_temporary`.
                    if (hub.isDraft)
                      IconButton(
                        tooltip: !hub.canTemporary
                            ? tr('临时对话（即将支持）', 'Temporary chat (coming soon)')
                            : hub.temporary
                            ? tr('关闭临时对话', 'Turn off temporary chat')
                            : tr('开启临时对话', 'Turn on temporary chat'),
                        isSelected: hub.temporary,
                        onPressed: hub.canTemporary
                            ? () => hub.updateDraft(
                                () => hub.temporary = !hub.temporary,
                              )
                            : null,
                        icon: const HubIcon(HubIcons.temporaryChat),
                        selectedIcon: HubIcon(
                          HubIcons.temporaryChat,
                          color: c.accent,
                        ),
                      ),
                    if (!hub.isDraft)
                      PinButton(hub: hub, conv: conv, run: action),
                    if (!hub.isDraft)
                      PopupMenuButton<String>(
                        tooltip: tr('对话命令', 'Chat commands'),
                        icon: const Icon(Icons.more_horiz),
                        position: PopupMenuPosition.under,
                        // Clear of the button and its highlight.
                        offset: const Offset(0, 6),
                        onSelected: (v) => action(
                          () => conversationAction(context, hub, conv, v),
                        ),
                        itemBuilder: (_) =>
                            conversationMenu(hub, conv, header: true),
                      ),
                    const SizedBox(width: 8),
                  ],
                ),
              ),
              if (showPill)
                ModePill(
                  mode: hub.isDraft ? hub.mode : (conv['mode'] ?? 'chat'),
                  onChanged: setMode,
                ),
            ],
          );
        },
      ),
    );
  }

  Widget errorBar() {
    final c = HubColors.of(context);
    return Center(
      child: Container(
        constraints: const BoxConstraints(maxWidth: contentWidth),
        margin: const EdgeInsets.fromLTRB(16, 0, 16, 8),
        padding: const EdgeInsets.fromLTRB(14, 6, 6, 6),
        decoration: BoxDecoration(
          color: c.danger.withValues(alpha: .08),
          borderRadius: BorderRadius.circular(12),
        ),
        child: Row(
          children: [
            Icon(Icons.error_outline, size: 18, color: c.danger),
            const SizedBox(width: 10),
            Expanded(
              child: Text(
                errorText(hub.error),
                style: TextStyle(fontSize: 13, color: c.text),
              ),
            ),
            TextButton(
              onPressed: () => setState(() => hub.error = ''),
              child: Text(tr('关闭', 'Dismiss')),
            ),
          ],
        ),
      ),
    );
  }

  Widget composer() {
    final draft = hub.isDraft;
    return Composer(
      key: composerKey,
      onSend: (text) => action(() => hub.send(text), rethrowError: true),
      running: !draft && hub.running,
      onCancel: () => action(hub.cancel),
      // Side questions and the approval mode are off the composer for now;
      // they are meant to come back as commands.
      steer: !draft && (hub.realChat || hub.realWork) && hub.mainlineRunning,
      enterToSend: prefs.enterToSend,
      leading: plusMenu(context, hub, action, onNewProject: manageProjects),
      tools: runTools(hub, action),
      trailing: modelControl(hub, action),
      // Work keeps its controls on their own row below the field.
      stacked: (draft ? hub.mode : hub.selectedConversation['mode']) == 'work',
      footer: composerFooter(hub),
    );
  }

  Widget draftHome() {
    final c = HubColors.of(context);
    final project = hub.projects
        .where((p) => p['id'] == hub.project)
        .firstOrNull;
    if (project == null) {
      return LayoutBuilder(
        builder: (context, box) => SingleChildScrollView(
          padding: const EdgeInsets.symmetric(horizontal: 16),
          child: ConstrainedBox(
            constraints: BoxConstraints(minHeight: box.maxHeight),
            child: Center(
              child: ConstrainedBox(
                constraints: const BoxConstraints(maxWidth: contentWidth),
                child: Padding(
                  padding: EdgeInsets.only(bottom: box.maxHeight * .12),
                  child: Column(
                    mainAxisSize: MainAxisSize.min,
                    crossAxisAlignment: CrossAxisAlignment.stretch,
                    children: [
                      Semantics(
                        container: true,
                        header: true,
                        child: Text(
                          tr('从哪里开始？', 'Where should we begin?'),
                          textAlign: TextAlign.center,
                          style: TextStyle(
                            fontSize: 28,
                            fontWeight: FontWeight.w500,
                            color: c.text,
                          ),
                        ),
                      ),
                      if (hub.temporary) ...[
                        const SizedBox(height: 10),
                        Text(
                          tr(
                            '临时对话不会出现在历史记录中，也不会写入项目记忆。',
                            'Temporary chats stay out of your history and project memory.',
                          ),
                          textAlign: TextAlign.center,
                          style: TextStyle(fontSize: 13, color: c.muted),
                        ),
                      ],
                      const SizedBox(height: 30),
                      composer(),
                    ],
                  ),
                ),
              ),
            ),
          ),
        ),
      );
    }
    final chats = hub.projectConversations(project['id']);
    return SingleChildScrollView(
      padding: const EdgeInsets.fromLTRB(16, 40, 16, 32),
      child: Center(
        child: ConstrainedBox(
          constraints: const BoxConstraints(maxWidth: contentWidth),
          child: Column(
            crossAxisAlignment: CrossAxisAlignment.stretch,
            children: [
              Row(
                children: [
                  Icon(Icons.folder_outlined, size: 26, color: c.text),
                  const SizedBox(width: 12),
                  Expanded(
                    child: Semantics(
                      container: true,
                      header: true,
                      child: Text(
                        project['name'] ?? '',
                        overflow: TextOverflow.ellipsis,
                        style: TextStyle(
                          fontSize: 26,
                          fontWeight: FontWeight.w500,
                          color: c.text,
                        ),
                      ),
                    ),
                  ),
                  OutlinedButton.icon(
                    onPressed: () => manageProjects(project),
                    icon: const Icon(Icons.edit_outlined, size: 16),
                    label: Text(tr('编辑项目', 'Edit project')),
                  ),
                ],
              ),
              if ((project['instructions'] ?? '').toString().isNotEmpty)
                Padding(
                  padding: const EdgeInsets.only(top: 10, left: 38),
                  child: Text(
                    project['instructions'],
                    maxLines: 2,
                    overflow: TextOverflow.ellipsis,
                    style: TextStyle(fontSize: 13, color: c.muted),
                  ),
                ),
              const SizedBox(height: 24),
              composer(),
              const SizedBox(height: 28),
              Text(
                tr('项目中的对话', 'Chats in this project'),
                style: TextStyle(fontSize: 13, color: c.subtle),
              ),
              const SizedBox(height: 8),
              if (chats.isEmpty)
                Container(
                  padding: const EdgeInsets.all(20),
                  decoration: BoxDecoration(
                    borderRadius: BorderRadius.circular(16),
                    border: Border.all(color: c.border),
                  ),
                  child: Text(
                    tr(
                      '还没有对话。发送第一条消息即可在此项目中开始。',
                      'No chats yet. Send a message to start one in this project.',
                    ),
                    style: TextStyle(color: c.muted),
                  ),
                )
              else
                Container(
                  decoration: BoxDecoration(
                    borderRadius: BorderRadius.circular(16),
                    border: Border.all(color: c.border),
                  ),
                  child: Column(
                    children: [
                      for (final (i, x) in chats.indexed) ...[
                        if (i > 0) Divider(color: c.border),
                        ListTile(
                          shape: RoundedRectangleBorder(
                            borderRadius: BorderRadius.circular(16),
                          ),
                          leading: Icon(
                            x['mode'] == 'work'
                                ? Icons.terminal
                                : Icons.chat_bubble_outline,
                            size: 18,
                          ),
                          title: Text(
                            x['title'] ?? '',
                            maxLines: 1,
                            overflow: TextOverflow.ellipsis,
                          ),
                          onTap: () => openConversation(x['id']),
                        ),
                      ],
                    ],
                  ),
                ),
            ],
          ),
        ),
      ),
    );
  }

  Widget workspaceBar() {
    final c = HubColors.of(context);
    final small = TextStyle(fontSize: 12, color: c.muted);
    return Center(
      child: ConstrainedBox(
        constraints: const BoxConstraints(maxWidth: contentWidth),
        child: Padding(
          padding: const EdgeInsets.fromLTRB(16, 4, 16, 6),
          child: Wrap(
            spacing: 8,
            runSpacing: 6,
            crossAxisAlignment: WrapCrossAlignment.center,
            children: [
              Icon(Icons.inventory_2_outlined, size: 16, color: c.subtle),
              if (hub.workspace['recovery_required'] == true)
                Text(
                  tr(
                    '工作区恢复未完成，请重试恢复后继续。',
                    'Workspace recovery is incomplete. Retry the restore to continue.',
                  ),
                  style: small.copyWith(color: c.danger),
                ),
              if (hub.workspace['checkpoint'] != null)
                Text(
                  tr(
                    '已保存检查点 · 水位 ${hub.workspace['checkpoint']['channel_seq']}',
                    'Checkpoint saved · seq ${hub.workspace['checkpoint']['channel_seq']}',
                  ),
                  style: small,
                ),
              if (hub.workspaceBusy.isNotEmpty)
                Text(tr('工作区任务进行中…', 'Workspace job running…'), style: small),
              for (final (kind, label) in [
                ('checkpoint', tr('保存检查点', 'Save checkpoint')),
                ('archive', tr('归档工作区', 'Archive workspace')),
                ('restore', tr('恢复工作区', 'Restore workspace')),
              ])
                OutlinedButton(
                  style: OutlinedButton.styleFrom(
                    textStyle: const TextStyle(
                      fontSize: 13,
                      fontFamily: fontFamily,
                      fontFamilyFallback: fontFallback,
                    ),
                  ),
                  onPressed: hub.workspaceBusy.isNotEmpty
                      ? null
                      : () => action(() => hub.workspaceOperation(kind)),
                  child: Text(label),
                ),
              IconButton(
                tooltip: tr('刷新工作区', 'Refresh workspace'),
                onPressed: () => action(hub.refreshWorkspace),
                icon: const Icon(Icons.refresh, size: 18),
              ),
            ],
          ),
        ),
      ),
    );
  }

  Widget conversationView() {
    final c = HubColors.of(context);
    final items = hub.items
        .where(
          (i) =>
              hub.searchFocusSeq == null ||
              (i['first_seq'] as int) <= hub.searchFocusSeq!,
        )
        .toList();
    Widget centered(Widget child, {Key? key}) => Center(
      key: key,
      child: ConstrainedBox(
        constraints: const BoxConstraints(maxWidth: contentWidth),
        child: child,
      ),
    );
    return Column(
      children: [
        Expanded(
          child: items.isEmpty
              ? Center(
                  child: Text(
                    tr('还没有消息', 'No messages yet'),
                    style: TextStyle(fontSize: 13, color: c.subtle),
                  ),
                )
              // Reversed so the newest message stays anchored above the
              // composer; shrink-wrapped so a short chat starts at the top.
              : Align(
                  alignment: Alignment.topCenter,
                  child: ListView.builder(
                    reverse: true,
                    shrinkWrap: true,
                    padding: const EdgeInsets.fromLTRB(24, 16, 24, 8),
                    itemCount: items.length + 1,
                    itemBuilder: (context, index) {
                      if (index == items.length) {
                        return OlderLoader(
                          key: ValueKey('older:${hub.selected}'),
                          hub: hub,
                        );
                      }
                      final item = items[items.length - 1 - index];
                      return centered(
                        key: ValueKey(item['id']),
                        MessageCard(
                          item: item,
                          agentLabel: hub.agentLabel,
                          onMemoryRead: item['kind'] == 'memory_context'
                              ? () async {
                                  final result = await hub.request(
                                    '/orgs/${hub.org}/conversations/${hub.selected}/memory-context/${item['id']}',
                                  );
                                  return result['items'] as List<dynamic>;
                                }
                              : null,
                          onSource: (href) {
                            final uri = Uri.tryParse(href);
                            final parts = uri?.pathSegments ?? [];
                            if (parts.length >= 4 &&
                                parts[0] == 'orgs' &&
                                parts[1] == hub.org &&
                                parts[2] == 'conversations') {
                              action(
                                () => hub.selectSearch(
                                  parts[3],
                                  uri?.queryParameters['item'],
                                  int.tryParse(
                                    uri?.queryParameters['seq'] ?? '',
                                  ),
                                ),
                              );
                            }
                          },
                          onMemoryUndo: item['kind'] == 'memory_notice'
                              ? () => action(() async {
                                  final notice = jsonDecode(item['text']);
                                  await hub.request(
                                    '/orgs/${hub.org}/memory-events/${notice['event_id']}/undo',
                                    body: {},
                                  );
                                })
                              : null,
                          onFork: hub.canFork(item)
                              ? () => action(() => hub.fork(item['id']))
                              : null,
                        ),
                      );
                    },
                  ),
                ),
        ),
        if (hub.searchFocusSeq != null)
          TextButton.icon(
            onPressed: hub.clearSearchFocus,
            icon: const Icon(Icons.arrow_downward),
            label: Text(tr('回到最新消息', 'Back to latest messages')),
          ),
        for (final approval in hub.actions)
          Padding(
            padding: const EdgeInsets.fromLTRB(16, 4, 16, 4),
            child: centered(
              (approval['options'] as List).contains('__answer__')
                  ? QuestionCard(
                      key: ValueKey(approval['id']),
                      summary: approval['summary'],
                      onAnswer: (answer) =>
                          action(() => hub.answer(approval, answer)),
                    )
                  : ApprovalCard(
                      key: ValueKey(approval['id']),
                      summary: approval['summary'],
                      onDeny: () => action(() => hub.decide(approval, 'deny')),
                      onAllow: () =>
                          action(() => hub.decide(approval, 'allow')),
                    ),
            ),
          ),
        if (hub.realWork) workspaceBar(),
        SafeArea(
          top: false,
          child: Padding(
            padding: const EdgeInsets.fromLTRB(16, 4, 16, 14),
            child: centered(composer()),
          ),
        ),
      ],
    );
  }

  @override
  Widget build(BuildContext context) {
    if (!hub.signedIn) return LoginPage(onLogin: login);
    final c = HubColors.of(context);
    final collapsed = wide && prefs.sidebarCollapsed;
    return Scaffold(
      key: scaffoldKey,
      drawer: wide
          ? null
          : Drawer(
              child: Sidebar(hub: hub, app: this, drawer: true),
            ),
      body: Row(
        children: [
          if (wide)
            Container(
              width: collapsed ? 56 : 260,
              decoration: BoxDecoration(
                border: Border(right: BorderSide(color: c.border)),
              ),
              child: collapsed
                  ? SidebarRail(hub: hub, app: this)
                  : Sidebar(hub: hub, app: this),
            ),
          Expanded(
            child: SafeArea(
              left: !wide,
              child: Column(
                children: [
                  header(),
                  if (hub.error.isNotEmpty) errorBar(),
                  Expanded(
                    child: hub.isDraft ? draftHome() : conversationView(),
                  ),
                ],
              ),
            ),
          ),
        ],
      ),
    );
  }
}

/// The centred Chat / Work switch.
class ModePill extends StatelessWidget {
  final String mode;
  final ValueChanged<String> onChanged;
  const ModePill({super.key, required this.mode, required this.onChanged});
  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    Widget segment(String value, String label, IconData? icon) {
      final selected = mode == value;
      return Semantics(
        container: true,
        button: true,
        selected: selected,
        child: Material(
          color: selected ? c.background : Colors.transparent,
          elevation: selected ? 1 : 0,
          shadowColor: Colors.black.withValues(alpha: .2),
          shape: const StadiumBorder(),
          child: InkWell(
            customBorder: const StadiumBorder(),
            // Stays tappable when selected so tests never see it disabled.
            onTap: () => onChanged(value),
            child: Padding(
              padding: const EdgeInsets.symmetric(horizontal: 18, vertical: 6),
              child: Row(
                mainAxisSize: MainAxisSize.min,
                children: [
                  if (icon != null) ...[
                    Icon(icon, size: 14, color: selected ? c.text : c.subtle),
                    const SizedBox(width: 5),
                  ],
                  Text(
                    label,
                    style: TextStyle(
                      fontSize: 13.5,
                      color: selected ? c.text : c.subtle,
                    ),
                  ),
                ],
              ),
            ),
          ),
        ),
      );
    }

    return Container(
      padding: const EdgeInsets.all(3),
      decoration: BoxDecoration(
        color: c.bubble,
        borderRadius: BorderRadius.circular(999),
      ),
      child: Row(
        mainAxisSize: MainAxisSize.min,
        children: [
          segment('chat', 'Chat', null),
          segment('work', 'Work', Icons.auto_awesome_outlined),
        ],
      ),
    );
  }
}

/// The top of the timeline. While older messages exist it shows a spinner and
/// loads the next page as soon as it is built, which the lazy list does when
/// it scrolls near the top; once the server reports none left it disappears.
class OlderLoader extends StatefulWidget {
  final HubController hub;
  const OlderLoader({super.key, required this.hub});
  @override
  State<OlderLoader> createState() => _OlderLoaderState();
}

class _OlderLoaderState extends State<OlderLoader> {
  // Shown only once the timeline scrolls: a short chat's top is on screen,
  // and the spinner would flash there while the first page check runs.
  bool scrollable = false;

  @override
  void initState() {
    super.initState();
    _load();
  }

  @override
  void didUpdateWidget(covariant OlderLoader oldWidget) {
    super.didUpdateWidget(oldWidget);
    _load();
  }

  void _load() => WidgetsBinding.instance.addPostFrameCallback((_) {
    if (!mounted) return;
    final position = Scrollable.maybeOf(context)?.position;
    final now =
        position != null &&
        position.hasContentDimensions &&
        position.maxScrollExtent > 0;
    if (now != scrollable) setState(() => scrollable = now);
    final hub = widget.hub;
    // A failed page waits for an explicit retry instead of looping.
    if (!hub.hasOlder || hub.historyLoading || hub.historyFailed) return;
    hub.loadOlder().catchError((_) {});
  });

  @override
  Widget build(BuildContext context) {
    final hub = widget.hub;
    final c = HubColors.of(context);
    if (!hub.hasOlder || (!scrollable && !hub.historyFailed)) {
      return const SizedBox(height: 8);
    }
    return Center(
      child: Padding(
        padding: const EdgeInsets.symmetric(vertical: 12),
        child: hub.historyFailed
            ? TextButton.icon(
                onPressed: () => hub.loadOlder().catchError((_) {}),
                icon: const Icon(Icons.refresh, size: 16),
                label: Text(tr('重新加载更早的消息', 'Retry loading earlier messages')),
              )
            : Semantics(
                label: tr('正在加载更早的消息', 'Loading earlier messages'),
                child: SizedBox(
                  width: 16,
                  height: 16,
                  child: CircularProgressIndicator(
                    strokeWidth: 2,
                    color: c.subtle,
                  ),
                ),
              ),
      ),
    );
  }
}
