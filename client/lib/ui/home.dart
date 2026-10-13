import 'dart:async';
import 'dart:convert';

import 'package:desktop_drop/desktop_drop.dart';
import 'package:flutter/foundation.dart';
import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:url_launcher/url_launcher.dart';

import '../browser.dart';
import '../controller.dart';
import '../l10n.dart';
import '../link.dart';
import '../prefs.dart';
import '../push.dart';
import '../theme.dart';
import 'attachments.dart';
import 'common.dart';
import 'composer.dart';
import 'dialogs.dart';
import 'icons.dart';
import 'library.dart';
import 'login.dart';
import 'messages.dart';
import 'plugins.dart';
import 'plugin_ui.dart';
import 'preview.dart';
import 'settings.dart';
import 'sidebar.dart';
import 'terminal.dart';
import 'unfold.dart';
import 'workspace_status.dart';

class HubPage extends StatefulWidget {
  /// The address that names the open conversation.
  final ConversationLink? link;
  const HubPage({super.key, this.link});
  @override
  State<HubPage> createState() => _HubPageState();
}

class _HubPageState extends State<HubPage>
    with WidgetsBindingObserver
    implements HubActions {
  HubController hub = HubController();
  final composerKey = GlobalKey();
  final composerFocus = FocusNode();

  /// The message field's text, so a refused message can be put back.
  final composerText = TextEditingController();

  /// The Work conversation whose terminal panel is open.
  String? terminalFor;
  final scaffoldKey = GlobalKey<ScaffoldState>();
  final libraryKey = GlobalKey<LibraryViewState>();

  /// Folds in the timeline open downward instead of pushing what is above.
  final unfold = UnfoldAnchor();

  /// Another conversation, a search result or a message sent from here
  /// shows the timeline from its newest row.
  final timelineScroll = ScrollController();
  Object? _timelineShows;

  /// Signing back in with the device's refresh cookie; the login page waits.
  bool restoring = keepsSession;

  /// The conversation whose draft the composer holds; empty for a new chat.
  String _draftFor = '';

  /// What was typed for a new chat, kept while a conversation is open.
  String _newChatDraft = '';

  /// The composer's text is being replaced, not typed.
  bool _loadingDraft = false;
  Timer? _draftSave;

  /// Files are being dragged over the window.
  bool dragging = false;
  late final void Function() _stopPaste;

  /// Each conversation keeps its own draft, synced to the user's other
  /// devices; another device's edit shows while this field is not in use.
  void _syncDraft() {
    if (hub.selected != _draftFor) {
      _draftSave?.cancel();
      final previous = _draftFor, text = composerText.text;
      if (previous.isEmpty) {
        _newChatDraft = text;
      } else {
        unawaited(hub.saveDraft(previous, text));
      }
      _draftFor = hub.selected;
      _setComposer(
        _draftFor.isEmpty
            ? _newChatDraft
            : '${hub.drafts[_draftFor]?['text'] ?? ''}',
      );
      return;
    }
    final remote = hub.drafts[_draftFor];
    if (remote != null &&
        remote['device_id'] != hub.device &&
        !composerFocus.hasFocus &&
        remote['text'] != composerText.text) {
      _setComposer('${remote['text'] ?? ''}');
    }
  }

  void _setComposer(String text) {
    if (composerText.text == text) return;
    _loadingDraft = true;
    composerText.text = text;
    _loadingDraft = false;
  }

  /// Typing saves the conversation's draft once it pauses.
  void _typed() {
    if (_loadingDraft || _draftFor.isEmpty) return;
    _draftSave?.cancel();
    final conv = _draftFor, text = composerText.text;
    _draftSave = Timer(
      const Duration(milliseconds: 1500),
      () => hub.saveDraft(conv, text),
    );
  }

  @override
  void initState() {
    super.initState();
    composerText.addListener(_typed);
    timelineScroll.addListener(() {
      if (timelineScroll.offset < 48) hub.sawLatest();
    });
    WidgetsBinding.instance.addObserver(this);
    // Windows tells of waking from sleep; the socket the sleep cut may look
    // alive, so reconnect straight away.
    if (!kIsWeb && defaultTargetPlatform == TargetPlatform.windows) {
      const MethodChannel('ryzhub/power').setMethodCallHandler((call) async {
        if (call.method == 'resumed' && hub.signedIn) await hub.reconnect();
      });
    }
    hub.addListener(_changed);
    prefs.addListener(_changed);
    widget.link?.requested.addListener(_follow);
    HardwareKeyboard.instance.addHandler(_shortcut);
    _stopPaste = listenForPastedFiles((name, bytes) {
      // Nothing takes files while a preview covers the composer.
      if (!hub.signedIn || !hub.canAttach || hub.previewFiles != null) return;
      if (hub.libraryOpen) {
        libraryKey.currentState?.upload([
          (name: name, size: bytes.length, open: () => Stream.value(bytes)),
        ]);
      } else {
        attachBytes(context, hub, name, bytes);
      }
    });
    if (restoring) _restore();
  }

  void _changed() {
    if (!mounted) return;
    // Not while the old controller is still notifying: it is disposed.
    if (hub.sessionEnded) return scheduleMicrotask(_sessionEnded);
    setState(() {});
    _syncDraft();
    if (!timelineScroll.hasClients || timelineScroll.offset < 48) {
      hub.sawLatest();
    }
    final shows = (hub.selected, hub.searchFocusSeq, hub.sends);
    if (shows != _timelineShows) {
      _timelineShows = shows;
      if (timelineScroll.hasClients) timelineScroll.jumpTo(0);
    }
    _report();
  }

  /// The session ran out and could not be renewed: back to signing in.
  void _sessionEnded() {
    if (!mounted || !hub.sessionEnded) return;
    Navigator.of(context).popUntil((route) => route.isFirst);
    final old = hub;
    old.removeListener(_changed);
    setState(() => hub = HubController()..addListener(_changed));
    old.dispose();
    _report();
    ScaffoldMessenger.of(context).showSnackBar(
      SnackBar(
        content: Text(
          tr('登录已过期，请重新登录。', 'Your session has ended. Sign in again.'),
        ),
      ),
    );
  }

  /// Put a refused message back into the field, with its files, in its
  /// conversation.
  void restoreFailed() {
    final failed = hub.takeFailedSend();
    if (failed == null) return;
    final conv = failed.channel.startsWith('conv:')
        ? failed.channel.substring(5)
        : '';
    if (conv.isNotEmpty && conv != hub.selected && hub.has(conv)) {
      openConversation(conv);
    }
    final typed = composerText.text;
    composerText.text = typed.isEmpty ? failed.text : '${failed.text}\n$typed';
    hub.attachFromLibrary(failed.files);
    focusComposer();
  }

  String get _lastKey => '${hub.base} ${hub.org}';

  /// The address shows the open conversation or the library; a native build
  /// remembers the conversation.
  void _report() {
    widget.link?.show(
      !hub.signedIn
          ? ''
          : hub.libraryOpen
          ? libraryLink
          : hub.selected,
    );
    if (!kIsWeb &&
        hub.signedIn &&
        prefs.lastConversations[_lastKey] != hub.selected) {
      prefs.update((p) => p.lastConversations[_lastKey] = hub.selected);
    }
  }

  Future<void> _restore() async {
    hub
      ..mode = prefs.defaultMode
      ..harness = prefs.defaultHarness;
    final server = prefs.server.isEmpty ? defaultServerUrl : prefs.server;
    // Back from signing in with another account: the address carries the result.
    final returned = takeSignInResult();
    if (returned.containsKey('login_code') ||
        returned.containsKey('login_error')) {
      try {
        await _finishSignIn(server, returned);
        if (!mounted) return;
        setState(() => restoring = false);
        _opened();
        return;
      } catch (e) {
        signInError = errorText(e);
      }
    }
    final restored = await hub.restore(server);
    if (!mounted) return;
    setState(() => restoring = false);
    if (restored) _opened();
    if (restored && returned.containsKey('login_linked')) {
      openSettings('account');
    }
  }

  /// Why signing in with another account did not finish, for the login page.
  String signInError = '';

  /// Sign in with another account at [server]; native builds wait here for the browser.
  Future<void> providerLogin(String server, String provider) async {
    server = server.replaceAll(RegExp(r'/+$'), '');
    prefs.update((p) => p.server = server);
    final returned = await signInElsewhere(
      (returnTo) =>
          '$server/auth/oauth/$provider/start?return_to=${Uri.encodeQueryComponent(returnTo)}',
    );
    await _finishSignIn(server, returned);
  }

  Future<void> _finishSignIn(
    String server,
    Map<String, String> returned,
  ) async {
    final error = returned['login_error'];
    if (error != null) throw Exception(error);
    final code = returned['login_code'];
    if (code == null) return;
    hub
      ..mode = prefs.defaultMode
      ..harness = prefs.defaultHarness;
    await hub.loginWithCode(server, code);
    prefs.update((p) => p.server = server);
    if (mounted) _opened();
  }

  /// After signing in, open the conversation the address names; a native
  /// start reopens the last one.
  void _opened() {
    Push.start(hub, (conversation) {
      if (mounted && hub.has(conversation)) openConversation(conversation);
    });
    final asked = widget.link?.requested.value ?? '';
    widget.link?.requested.value = null;
    final id = asked.isNotEmpty || kIsWeb
        ? asked
        : prefs.lastConversations[_lastKey] ?? '';
    if (id.isEmpty) return;
    if (id == libraryLink) {
      openLibrary();
    } else if (id == pluginsLink) {
      _report();
      openSettings('plugins');
    } else if (hub.has(id)) {
      openConversation(id);
    } else {
      widget.link?.refuse();
    }
  }

  /// Back, forward or an entered address opens what it names.
  void _follow() {
    final id = widget.link?.requested.value;
    if (id == null || !hub.signedIn) return;
    widget.link!.requested.value = null;
    if (id == libraryLink) {
      if (!hub.libraryOpen) openLibrary();
    } else if (id == pluginsLink) {
      _report();
      openSettings('plugins');
    } else if (id == hub.selected && !hub.libraryOpen) {
      return;
    } else if (id.isEmpty) {
      newChat();
    } else if (hub.has(id)) {
      openConversation(id);
    } else {
      widget.link!.refuse();
    }
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
    _stopPaste();
    prefs.removeListener(_changed);
    widget.link?.requested.removeListener(_follow);
    composerFocus.dispose();
    composerText.dispose();
    timelineScroll.dispose();
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
    focusComposer();
  }

  /// A new chat starts at the message field. Touch screens wait for a tap
  /// rather than raise the keyboard.
  void focusComposer() {
    if (touchPlatform) return;
    WidgetsBinding.instance.addPostFrameCallback((_) {
      if (mounted) composerFocus.requestFocus();
    });
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
  void openLibrary() {
    closeDrawer();
    hub.openLibrary();
  }

  /// A previewed file as a message carries it.
  Map<String, dynamic> carried(Map<String, dynamic> file) => {
    'id': file['id'],
    'name': file['name'],
    'size': file['size'],
    'kind': file['kind'],
  };

  /// A question about a file of the library starts a new chat with it.
  Future<void> askAbout(Map<String, dynamic> file, String question) async {
    newChat();
    await action(() => hub.send(question, only: [carried(file)]));
  }

  /// A question about a file previewed over the conversation goes on in it,
  /// with the file; the conversation shows again once it is sent.
  Future<void> followUp(Map<String, dynamic> file, String question) async {
    await action(
      () => hub.send(question, only: [carried(file)]),
      rethrowError: true,
    );
    hub.closePreview();
  }

  /// A file of the library goes with the first message of a new chat.
  void useFile(Map<String, dynamic> file) {
    newChat();
    if (hub.pendingFiles.length >= hub.maxFiles &&
        !hub.pendingFiles.any((f) => f.id == file['id'])) {
      ScaffoldMessenger.of(context).showSnackBar(
        SnackBar(content: Text(errorText('TOO_MANY_ATTACHMENTS'))),
      );
      return;
    }
    hub.attachFromLibrary([file]);
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
    focusComposer();
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
    _report();
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
  ) async {
    hub
      ..mode = prefs.defaultMode
      ..harness = prefs.defaultHarness;
    await hub.login(server, email, password, register);
    prefs.update((p) => p.server = server);
    _opened();
  }

  void setMode(String mode) => hub.updateDraft(() {
    hub.mode = mode;
    hub.draftModel = null;
  });

  /// Above the library only the drawer's button, where there is one.
  Widget libraryHeader() => SizedBox(
    height: wide ? 28 : 56,
    child: Row(
      children: [
        const SizedBox(width: 8),
        if (!wide)
          IconButton(
            tooltip: tr('打开侧边栏', 'Open sidebar'),
            icon: const HubIcon(HubIcons.sidebarCollapsed),
            onPressed: () => scaffoldKey.currentState?.openDrawer(),
          ),
        if (!wide && hub.connection != 'online')
          ConnectionStatus(
            connection: hub.connection,
            onReconnect: hub.reconnect,
          ),
      ],
    ),
  );

  Widget header() {
    final c = HubColors.of(context);
    final conv = hub.selectedConversation;
    final title = hub.isDraft ? null : (conv['title'] ?? '') as String;
    return SizedBox(
      height: 56,
      width: double.infinity,
      child: Stack(
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
                  Expanded(
                    child: Padding(
                      padding: const EdgeInsets.symmetric(horizontal: Space.s8),
                      // The mode trails the title the way a reasoning level
                      // trails its model; it is set when the chat starts.
                      child: Row(
                        children: [
                          Flexible(
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
                          const SizedBox(width: 6),
                          Text(
                            conv['mode'] == 'work' ? 'Work' : 'Chat',
                            style: modelLabelStyle(size: 12, color: c.subtle),
                          ),
                        ],
                      ),
                    ),
                  )
                else
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
                if (!hub.isDraft) PinButton(hub: hub, conv: conv, run: action),
                if (!hub.isDraft)
                  PopupMenuButton<String>(
                    tooltip: tr('对话命令', 'Chat commands'),
                    icon: const Icon(Icons.more_horiz),
                    position: PopupMenuPosition.under,
                    // Clear of the button and its highlight.
                    offset: const Offset(0, 6),
                    onSelected: (v) =>
                        action(() => conversationAction(context, hub, conv, v)),
                    itemBuilder: (_) =>
                        conversationMenu(hub, conv, header: true),
                  ),
                const SizedBox(width: 8),
              ],
            ),
          ),
          if (hub.isDraft) ModePill(mode: hub.mode, onChanged: setMode),
        ],
      ),
    );
  }

  /// A refused message: why, and a way back to editing it.
  Widget failedBar(SentInput failed) {
    final c = HubColors.of(context);
    return Center(
      child: Container(
        constraints: const BoxConstraints(maxWidth: contentWidth),
        margin: const EdgeInsets.fromLTRB(Space.s16, 0, Space.s16, Space.s8),
        padding: const EdgeInsets.fromLTRB(
          Space.s14,
          Space.s6,
          Space.s6,
          Space.s6,
        ),
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
                tr(
                  '消息未发送：${errorText(failed.code)}',
                  'Message not sent: ${errorText(failed.code)}',
                ),
                style: TextStyle(fontSize: 13, color: c.text),
              ),
            ),
            TextButton(
              onPressed: restoreFailed,
              child: Text(tr('恢复编辑', 'Edit again')),
            ),
            TextButton(
              onPressed: hub.takeFailedSend,
              child: Text(tr('关闭', 'Dismiss')),
            ),
          ],
        ),
      ),
    );
  }

  Widget errorBar() {
    final c = HubColors.of(context);
    return Center(
      child: Container(
        constraints: const BoxConstraints(maxWidth: contentWidth),
        margin: const EdgeInsets.fromLTRB(Space.s16, 0, Space.s16, Space.s8),
        padding: const EdgeInsets.fromLTRB(
          Space.s14,
          Space.s6,
          Space.s6,
          Space.s6,
        ),
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

  /// The conversation's run is going, as its composers show.
  bool get running => !hub.isDraft && hub.running;

  /// What is sent while the run goes instructs it.
  bool get steerable =>
      !hub.isDraft && (hub.realChat || hub.realWork) && hub.mainlineRunning;

  Widget composer() {
    final draft = hub.isDraft;
    return Composer(
      key: composerKey,
      controller: composerText,
      onSend: (text) => action(() => hub.send(text), rethrowError: true),
      running: running,
      onCancel: () => action(hub.cancel),
      // Side questions are off the composer for now; they are meant to come
      // back as a command. Work's approval mode is among the run tools.
      steer: steerable,
      enterToSend: prefs.enterToSend,
      focusNode: composerFocus,
      attachments: hub.pendingFiles.isEmpty ? null : PendingFiles(hub: hub),
      canSendEmpty: hub.readyFiles.isNotEmpty,
      blocked: hub.filesBusy,
      leading: plusMenu(context, hub, action, onNewProject: manageProjects),
      tools: runTools(hub, action),
      trailing: modelControl(hub, action),
      // Work keeps its controls on their own row below the field.
      stacked: (draft ? hub.mode : hub.selectedConversation['mode']) == 'work',
      footer: composerFooter(hub),
      commands: slashCommands(
        context,
        hub,
        action,
        onNewChat: () {
          hub.startDraft(project: hub.contextProject);
          focusComposer();
        },
      ),
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
          padding: const EdgeInsets.symmetric(horizontal: Space.s16),
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
      padding: const EdgeInsets.fromLTRB(
        Space.s16,
        Space.s40,
        Space.s16,
        Space.s32,
      ),
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
                  padding: const EdgeInsets.only(
                    top: Space.s10,
                    left: Space.s38,
                  ),
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
                  padding: const EdgeInsets.all(Space.s20),
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
          padding: const EdgeInsets.fromLTRB(
            Space.s16,
            Space.s4,
            Space.s16,
            Space.s6,
          ),
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
                Tooltip(
                  message: fileTime(hub.workspace['checkpoint']['created_at']),
                  child: Text(
                    checkpointSaved(hub.workspace['checkpoint']['created_at']),
                    style: small,
                  ),
                ),
              // In use, yet its last checkpoint is older than the recovery
              // point objective.
              if (hub.workspace['stale'] == true)
                Text(
                  tr(
                    '超过 20 分钟没有新的检查点：执行节点故障时，之后的改动可能丢失。',
                    'No checkpoint for over 20 minutes: a node failure could lose the changes since.',
                  ),
                  style: small.copyWith(color: c.warning),
                ),
              if (storageNote(hub.workspace['storage_state']) case final note?)
                Text(note, style: small),
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
              if (hub.supports('terminal') &&
                  hub.workspace['sandbox']?['state'] == 'running')
                OutlinedButton.icon(
                  style: OutlinedButton.styleFrom(
                    textStyle: const TextStyle(
                      fontSize: 13,
                      fontFamily: fontFamily,
                      fontFamilyFallback: fontFallback,
                    ),
                  ),
                  onPressed: () => setState(
                    () => terminalFor = terminalFor == hub.selected
                        ? null
                        : hub.selected,
                  ),
                  icon: const Icon(Icons.terminal, size: 16),
                  label: Text(tr('终端', 'Terminal')),
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
    final rows = hub.rows;
    Rendered? rendered(String id) {
      final entry = hub.entry(id);
      return entry == null ? null : Rendered(entry.blocks, entry.chips);
    }

    // Found by key, a row keeps its state (an opened steps group) as rows
    // arrive below it.
    final positions = {for (final (i, row) in rows.indexed) row['id']: i};
    void openSource(String href) {
      final uri = Uri.tryParse(href);
      final parts = uri?.pathSegments ?? [];
      if (parts.length >= 4 &&
          parts[0] == 'orgs' &&
          parts[2] == 'conversations') {
        if (parts[1] != hub.org) return;
        action(
          () => hub.selectSearch(
            parts[3],
            uri?.queryParameters['item'],
            int.tryParse(uri?.queryParameters['seq'] ?? ''),
          ),
        );
      } else if (uri != null &&
          (uri.isScheme('https') || uri.isScheme('http'))) {
        // Other links, such as cited web pages, open outside the app.
        launchUrl(uri, mode: LaunchMode.externalApplication);
      }
    }

    /// A file a reply links to opens in the preview once handed over; the
    /// reply holding the link is looked in first, then newer replies.
    void openFile(String href, Iterable<dynamic> scope) {
      final found = linkedFile(href, [...scope, ...hub.items.reversed]);
      if (found != null && found.error == null) {
        return hub.openPreview(
          found.files,
          index: found.index,
          inConversation: true,
        );
      }
      final waiting =
          found == null &&
          (scope.any((item) => item is Map && item['state'] == 'streaming') ||
              hub.running);
      ScaffoldMessenger.of(context).showSnackBar(
        SnackBar(
          content: Text(
            waiting
                ? tr(
                    '回答完成后即可打开这个文件。',
                    'The file opens once the reply completes.',
                  )
                : errorText(found?.error ?? 'FILE_NOT_FOUND'),
          ),
        ),
      );
    }

    void openLink(String href, Iterable<dynamic> scope) =>
        isFileLink(href) ? openFile(href, scope) : openSource(href);

    Widget centered(Widget child, {Key? key}) => Center(
      key: key,
      child: ConstrainedBox(
        constraints: const BoxConstraints(maxWidth: contentWidth),
        child: child,
      ),
    );

    Widget row(BuildContext context, int index) {
      if (index == rows.length) {
        return OlderLoader(key: ValueKey('older:${hub.selected}'), hub: hub);
      }
      final row = rows[rows.length - 1 - index];
      final id = row['item'] as String?;
      if (id == null) {
        final steps = [
          for (final step in row['steps'] as List)
            ?hub.entry(step as String)?.item,
        ];
        return centered(
          key: ValueKey(row['id']),
          StepsGroup(
            steps: steps,
            running: hub.runActive('${row['run_id'] ?? ''}'),
            latest: row['latest'] == true,
            worked: hub.workedFor('${row['run_id'] ?? ''}'),
            onSource: (href) => openLink(href, steps),
            rendered: rendered,
            children: hub.childrenOf,
          ),
        );
      }
      final item = hub.entry(id)?.item ?? {'id': id, 'kind': '', 'text': ''};
      return centered(
        key: ValueKey(row['id']),
        MessageCard(
          item: item,
          rendered: rendered(id),
          agentLabel: hub.agentLabel,
          onSource: (href) => openLink(href, [item]),
          files: (files) => MessageFiles(
            hub: hub,
            files: files,
            alignment: item['kind'] == 'user'
                ? WrapAlignment.end
                : WrapAlignment.start,
          ),
          onMemoryUndo: item['kind'] == 'memory_notice'
              ? () => action(() async {
                  final notice = jsonDecode(item['text']);
                  await hub.undoMemory('${notice['event_id']}');
                  if (mounted) {
                    ScaffoldMessenger.of(this.context).showSnackBar(
                      SnackBar(
                        content: Text(tr('已撤销记忆变更', 'Memory change undone')),
                      ),
                    );
                  }
                })
              : null,
          onFork: hub.canFork(item)
              ? () => action(() => hub.fork(item['id']))
              : null,
          forkNote: hub.emulated('fork') ? emulatedNote('fork') : null,
        ),
      );
    }

    return Column(
      children: [
        Expanded(
          child: rows.isEmpty
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
                  child: UnfoldScope(
                    anchor: unfold,
                    child: CustomScrollView(
                      controller: timelineScroll,
                      reverse: true,
                      shrinkWrap: true,
                      physics: unfold.physics,
                      semanticChildCount: rows.length + 1,
                      slivers: [
                        SliverPadding(
                          padding: const EdgeInsets.fromLTRB(
                            Space.s24,
                            Space.s16,
                            Space.s24,
                            Space.s8,
                          ),
                          sliver: TimelineRows(
                            loader: true,
                            delegate: SliverChildBuilderDelegate(
                              row,
                              childCount: rows.length + 1,
                              findChildIndexCallback: (key) {
                                final i = key is ValueKey
                                    ? positions[key.value]
                                    : null;
                                return i == null ? null : rows.length - 1 - i;
                              },
                            ),
                          ),
                        ),
                      ],
                    ),
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
            padding: const EdgeInsets.fromLTRB(
              Space.s16,
              Space.s4,
              Space.s16,
              Space.s4,
            ),
            child: centered(
              '${approval['form_json'] ?? ''}'.isNotEmpty
                  ? ElicitationCard(
                      key: ValueKey(approval['id']),
                      summary: approval['summary'],
                      form: Map<String, dynamic>.from(
                        jsonDecode(approval['form_json']) as Map,
                      ),
                      onAnswer: (answer) =>
                          action(() => hub.answer(approval, answer)),
                    )
                  : (approval['options'] as List).contains('__answer__')
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
        if (hub.realWork && terminalFor == hub.selected)
          TerminalPanel(
            key: ValueKey(hub.selected),
            hub: hub,
            conversation: hub.selected,
            onClose: () => setState(() => terminalFor = null),
          ),
        SafeArea(
          top: false,
          child: Padding(
            padding: const EdgeInsets.fromLTRB(
              Space.s16,
              Space.s4,
              Space.s16,
              Space.s14,
            ),
            child: centered(composer()),
          ),
        ),
      ],
    );
  }

  @override
  Widget build(BuildContext context) {
    if (restoring) return const Scaffold();
    if (!hub.signedIn) {
      return LoginPage(
        onLogin: login,
        onProvider: providerLogin,
        error: signInError,
      );
    }
    final c = HubColors.of(context);
    final collapsed = wide && prefs.sidebarCollapsed;
    final previewing = hub.previewFiles != null;
    final floating = previewing && hub.previewInConversation;
    return PluginHost(
      hub: hub,
      child: Scaffold(
        key: scaffoldKey,
        drawer: wide
            ? null
            : Drawer(
                child: Sidebar(hub: hub, app: this, drawer: true),
              ),
        body: Stack(
          fit: StackFit.expand,
          children: [
            // Under a preview over the conversation, the whole page, sidebar
            // too, is out of reach.
            ExcludeFocus(
              excluding: floating,
              child: ExcludeSemantics(
                excluding: floating,
                child: IgnorePointer(
                  ignoring: floating,
                  child: Row(
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
                        // Files dropped anywhere on the conversation go with the next message.
                        child: DropTarget(
                          enable: hub.canAttach && !previewing,
                          onDragEntered: (_) => setState(() => dragging = true),
                          onDragExited: (_) => setState(() => dragging = false),
                          onDragDone: (details) async {
                            setState(() => dragging = false);
                            if (hub.libraryOpen) {
                              final files = await droppedFiles(details.files);
                              await libraryKey.currentState?.upload(files);
                            } else {
                              await attachDropped(context, hub, details.files);
                            }
                          },
                          child: Stack(
                            children: [
                              // Under a preview the page keeps its state but is out of
                              // reach: no taps, no keyboard focus, nothing for assistive
                              // tech.
                              ExcludeFocus(
                                excluding: previewing,
                                child: ExcludeSemantics(
                                  excluding: previewing,
                                  child: IgnorePointer(
                                    ignoring: previewing,
                                    child: SafeArea(
                                      left: !wide,
                                      child: Column(
                                        children: [
                                          hub.libraryOpen
                                              ? libraryHeader()
                                              : header(),
                                          if (hub.failedSend case final failed?)
                                            failedBar(failed),
                                          if (hub.error.isNotEmpty) errorBar(),
                                          Expanded(
                                            child: hub.libraryOpen
                                                ? LibraryView(
                                                    key: libraryKey,
                                                    hub: hub,
                                                    onUse: useFile,
                                                  )
                                                : hub.isDraft
                                                ? draftHome()
                                                : conversationView(),
                                          ),
                                        ],
                                      ),
                                    ),
                                  ),
                                ),
                              ),
                              // The library's, over everything right of the sidebar,
                              // even its header.
                              if (previewing && !floating)
                                Positioned.fill(
                                  child: FilePreviewView(
                                    hub: hub,
                                    onAsk: askAbout,
                                  ),
                                ),
                              if (dragging)
                                Positioned.fill(
                                  child: IgnorePointer(
                                    child: Container(
                                      margin: const EdgeInsets.all(Space.s12),
                                      decoration: BoxDecoration(
                                        color: c.background.withValues(
                                          alpha: .85,
                                        ),
                                        borderRadius: BorderRadius.circular(16),
                                        border: Border.all(
                                          color: c.accent,
                                          width: 2,
                                        ),
                                      ),
                                      alignment: Alignment.center,
                                      child: Text(
                                        hub.libraryOpen
                                            ? tr(
                                                '松开以上传到文件库',
                                                'Drop to upload to your library',
                                              )
                                            : tr(
                                                '松开以添加文件',
                                                'Drop to add files',
                                              ),
                                        style: TextStyle(
                                          fontSize: 16,
                                          color: c.text,
                                        ),
                                      ),
                                    ),
                                  ),
                                ),
                            ],
                          ),
                        ),
                      ),
                    ],
                  ),
                ),
              ),
            ),
            if (floating)
              FilePreviewView(
                hub: hub,
                onAsk: followUp,
                floating: true,
                running: running,
                steer: steerable,
                onCancel: () => action(hub.cancel),
              ),
          ],
        ),
      ),
    );
  }
}

/// The centred Chat / Work switch of a new chat.
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
              padding: const EdgeInsets.symmetric(
                horizontal: Space.s18,
                vertical: Space.s6,
              ),
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
      padding: const EdgeInsets.all(Space.s3),
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
        padding: const EdgeInsets.symmetric(vertical: Space.s12),
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
