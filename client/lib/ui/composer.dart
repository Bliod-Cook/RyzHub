import 'dart:math' as math;

import 'package:flutter/material.dart';
import 'package:flutter/rendering.dart';
import 'package:flutter/services.dart';

import '../controller.dart';
import '../l10n.dart';
import '../theme.dart';
import 'attachments.dart';
import 'plugins.dart';
import 'common.dart';
import 'dialogs.dart';
import 'model_picker.dart';

/// A command the composer offers as `/` is typed.
class SlashCommand {
  final String name, description;

  /// The group it is listed under, in the order groups first appear.
  final String group;

  /// Runs it; without one, choosing it puts `/name ` in the field for its
  /// arguments, and it is sent as text.
  final VoidCallback? run;
  const SlashCommand(this.name, this.description, this.group, {this.run});
}

class Composer extends StatefulWidget {
  final Future<void> Function(String) onSend;
  final VoidCallback onCancel;
  final bool running;
  final bool steer;
  final Widget? leading;
  final List<Widget> tools;

  /// Sits beside the send button, e.g. the model and its reasoning level.
  final Widget? trailing;

  /// Always put the field above the controls (Work), instead of one row.
  final bool stacked;
  final List<Widget> footer;

  /// Enter sends and Shift+Enter breaks the line; Ctrl+Enter always sends.
  final bool enterToSend;

  /// The text field's focus, for callers that move the cursor to it.
  final FocusNode? focusNode;

  /// Files picked for the message, shown above the text.
  final Widget? attachments;

  /// The message may be sent without text: it carries files.
  final bool canSendEmpty;

  /// Sending waits, e.g. for files still uploading.
  final bool blocked;

  /// Replaces the field's usual prompt.
  final String? hint;

  /// The field's text, when the page needs to put a message back into it.
  final TextEditingController? controller;

  /// Offered as the field starts with `/`.
  final List<SlashCommand> commands;
  const Composer({
    super.key,
    required this.onSend,
    required this.onCancel,
    required this.running,
    this.steer = false,
    this.leading,
    this.tools = const [],
    this.trailing,
    this.stacked = false,
    this.footer = const [],
    this.enterToSend = true,
    this.focusNode,
    this.attachments,
    this.canSendEmpty = false,
    this.blocked = false,
    this.hint,
    this.controller,
    this.commands = const [],
  });
  @override
  State<Composer> createState() => _ComposerState();
}

class _ComposerState extends State<Composer> {
  late final TextEditingController input =
      widget.controller ?? TextEditingController();

  // One anchor for the composer's menus, so `+` and the model panel both hang
  // from the box and only one is open at a time.
  final menu = MenuController();
  String? menuId;
  List<Widget> Function(BuildContext context)? menuItems;
  bool menuFull = false;

  void toggleMenu(
    String id,
    List<Widget> Function(BuildContext context) items, {
    bool full = false,
  }) {
    if (menu.isOpen && menuId == id) return menu.close();
    setState(() {
      menuId = id;
      menuItems = items;
      menuFull = full;
    });
    // Open after the rebuild so the anchor carries the new items and size.
    WidgetsBinding.instance.addPostFrameCallback((_) {
      if (mounted) menu.open();
    });
  }

  bool sending = false;
  bool empty = true;

  /// The suggested command the keyboard is on.
  int highlighted = 0;

  /// The suggestions were dismissed for the text as it is.
  String? dismissedFor;

  /// The commands whose name starts with what follows the `/`, while only a
  /// command's name is typed.
  List<SlashCommand> get suggestions {
    final text = input.text;
    if (!text.startsWith('/') ||
        text.contains(RegExp(r'\s')) ||
        text == dismissedFor ||
        !input.value.composing.isCollapsed) {
      return const [];
    }
    final query = text.substring(1).toLowerCase();
    return [
      for (final command in widget.commands)
        if (command.name.toLowerCase().startsWith(query)) command,
    ];
  }

  void choose(SlashCommand command) {
    final run = command.run;
    if (run == null) {
      input.text = '/${command.name} ';
      input.selection = TextSelection.collapsed(offset: input.text.length);
      return;
    }
    input.clear();
    run();
  }

  void move(int by) {
    final count = suggestions.length;
    if (count == 0) return;
    setState(() => highlighted = (highlighted + by) % count);
  }

  @override
  void initState() {
    super.initState();
    // Every edit can change whether the text still fits on the single row.
    var typed = input.text;
    input.addListener(
      () => setState(() {
        empty = input.text.trim().isEmpty;
        // Moving the cursor keeps the highlighted suggestion.
        if (input.text != typed) highlighted = 0;
        typed = input.text;
      }),
    );
  }

  @override
  void dispose() {
    if (widget.controller == null) input.dispose();
    super.dispose();
  }

  Future<void> send() async {
    if (sending ||
        widget.blocked ||
        (input.text.trim().isEmpty && !widget.canSendEmpty) ||
        !input.value.composing.isCollapsed) {
      return;
    }
    final text = input.text;
    setState(() => sending = true);
    // Free for the next message at once: what is typed while this one goes
    // out is the next draft, which nothing clears.
    input.clear();
    try {
      await widget.onSend(text);
    } catch (_) {
      // The caller reports the failure; the text comes back for a retry, ahead
      // of anything typed since.
      if (mounted) {
        final typed = input.text;
        input.text = typed.isEmpty ? text : '$text\n$typed';
        input.selection = TextSelection.collapsed(offset: input.text.length);
      }
    } finally {
      if (mounted) setState(() => sending = false);
    }
  }

  Widget round({
    required String tooltip,
    required IconData icon,
    required VoidCallback? onPressed,
    required Color background,
    required Color foreground,
    Color? border,
  }) => Semantics(
    container: true,
    button: true,
    enabled: onPressed != null,
    child: Tooltip(
      message: tooltip,
      child: Material(
        color: background,
        shape: CircleBorder(
          side: border == null ? BorderSide.none : BorderSide(color: border),
        ),
        child: InkWell(
          customBorder: const CircleBorder(),
          onTap: onPressed,
          child: SizedBox(
            width: 36,
            height: 36,
            child: Icon(icon, size: 20, color: foreground),
          ),
        ),
      ),
    ),
  );

  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    final canSend = !widget.running || widget.steer;
    final stop = round(
      tooltip: tr('停止', 'Stop'),
      icon: Icons.stop_rounded,
      onPressed: widget.onCancel,
      background: canSend ? c.composer : c.text,
      foreground: canSend ? c.text : c.background,
      border: canSend ? c.border : null,
    );
    final fieldStyle = TextStyle(
      fontSize: FontSizes.message,
      height: 1.5,
      color: c.text,
    );
    // Measured with the field's own resolved style, so the single row gives
    // way exactly when the text would reach the controls on the right.
    final painter = TextPainter(
      text: TextSpan(
        text: input.text,
        style: Theme.of(context).textTheme.bodyLarge!.merge(fieldStyle),
      ),
      textDirection: Directionality.of(context),
      textScaler: MediaQuery.textScalerOf(context),
      maxLines: 1,
    )..layout();
    final textWidth = painter.width;
    painter.dispose();
    final suggested = suggestions;
    if (highlighted >= suggested.length) highlighted = 0;
    void pick() => choose(suggested[highlighted]);
    // Bound at the field itself: the menu anchor between it and the
    // composer's shortcuts takes arrow keys for its own traversal.
    final picking = <ShortcutActivator, VoidCallback>{
      if (suggested.isNotEmpty) ...{
        // Picking a command comes before sending.
        const SingleActivator(LogicalKeyboardKey.enter): pick,
        const SingleActivator(LogicalKeyboardKey.tab): pick,
        const SingleActivator(LogicalKeyboardKey.arrowDown): () => move(1),
        const SingleActivator(LogicalKeyboardKey.arrowUp): () => move(-1),
        const SingleActivator(LogicalKeyboardKey.escape): () =>
            setState(() => dismissedFor = input.text),
      },
    };
    return CallbackShortcuts(
      bindings: {
        const SingleActivator(LogicalKeyboardKey.enter, control: true): send,
        const SingleActivator(LogicalKeyboardKey.enter, meta: true): send,
        if (widget.enterToSend)
          const SingleActivator(LogicalKeyboardKey.enter): send,
      },
      child: ComposerMenus(
        toggle: toggleMenu,
        close: menu.close,
        child: Column(
          mainAxisSize: MainAxisSize.min,
          crossAxisAlignment: CrossAxisAlignment.stretch,
          children: [
            if (suggested.isNotEmpty)
              _Suggestions(
                commands: suggested,
                highlighted: highlighted,
                onPick: choose,
              ),
            LayoutBuilder(
              builder: (context, box) {
                final width = menuFull
                    ? box.maxWidth
                    : math.min(280.0, box.maxWidth);
                return MenuAnchor(
                  controller: menu,
                  // Otherwise the panel ignores the width and hugs its content.
                  crossAxisUnconstrained: false,
                  // Top edge on the box's bottom edge (or the reverse when there
                  // is no room below): full width for `+`, flush with the
                  // trailing edge for the model.
                  alignmentOffset: Offset(box.maxWidth - width, 6),
                  style: MenuStyle(
                    // Standard density so the compact desktop density cannot
                    // narrow it below the box it aligns with.
                    visualDensity: VisualDensity.standard,
                    minimumSize: WidgetStatePropertyAll(Size(width, 0)),
                    maximumSize: WidgetStatePropertyAll(Size(width, 460)),
                  ),
                  menuChildren: menuItems?.call(context) ?? const [],
                  child: Container(
                    padding: const EdgeInsets.fromLTRB(
                      Space.gap,
                      6,
                      Space.gap,
                      6,
                    ),
                    decoration: BoxDecoration(
                      color: c.composer,
                      borderRadius: BorderRadius.circular(28),
                      border: Border.all(color: c.border),
                      boxShadow: [
                        BoxShadow(
                          color: Colors.black.withValues(alpha: .05),
                          blurRadius: 16,
                          offset: const Offset(0, 4),
                        ),
                      ],
                    ),
                    // A fixed traversal order: by default the web engine orders
                    // semantics nodes by position and re-inserts the focused textarea
                    // when the field moves rows, which blurs it mid-typing.
                    child: Semantics(
                      container: true,
                      explicitChildNodes: true,
                      child: Column(
                        mainAxisSize: MainAxisSize.min,
                        crossAxisAlignment: CrossAxisAlignment.stretch,
                        children: [
                          if (widget.attachments != null)
                            Padding(
                              padding: const EdgeInsets.fromLTRB(6, 4, 6, 4),
                              child: widget.attachments,
                            ),
                          _ComposerRow(
                            textWidth: textWidth,
                            multiline:
                                widget.stacked || input.text.contains('\n'),
                            children: [
                              for (final (i, part) in [
                                Row(
                                  mainAxisSize: MainAxisSize.min,
                                  children: [
                                    ?widget.leading,
                                    Flexible(
                                      child: SingleChildScrollView(
                                        scrollDirection: Axis.horizontal,
                                        child: Row(
                                          mainAxisSize: MainAxisSize.min,
                                          children: [
                                            for (final tool in widget.tools)
                                              Padding(
                                                padding: const EdgeInsets.only(
                                                  left: 6,
                                                ),
                                                child: tool,
                                              ),
                                          ],
                                        ),
                                      ),
                                    ),
                                  ],
                                ),
                                Semantics(
                                  container: true,
                                  child: CallbackShortcuts(
                                    bindings: picking,
                                    child: TextField(
                                      controller: input,
                                      focusNode: widget.focusNode,
                                      minLines: 1,
                                      maxLines: 8,
                                      textInputAction: TextInputAction.newline,
                                      style: fieldStyle,
                                      decoration: InputDecoration(
                                        hintText:
                                            widget.hint ??
                                            tr('写下你的想法…', 'Ask anything…'),
                                        border: InputBorder.none,
                                        enabledBorder: InputBorder.none,
                                        focusedBorder: InputBorder.none,
                                        filled: false,
                                        isDense: true,
                                        contentPadding:
                                            const EdgeInsets.fromLTRB(
                                              _ComposerRow.fieldStart,
                                              10,
                                              _ComposerRow.fieldEnd,
                                              10,
                                            ),
                                      ),
                                    ),
                                  ),
                                ),
                                Row(
                                  mainAxisSize: MainAxisSize.min,
                                  children: [
                                    ?widget.trailing,
                                    if (widget.trailing != null)
                                      const SizedBox(width: 4),
                                    if (widget.running) stop,
                                    if (widget.running && canSend)
                                      const SizedBox(width: 6),
                                    if (canSend)
                                      round(
                                        tooltip: tr('发送', 'Send'),
                                        icon: Icons.arrow_upward_rounded,
                                        onPressed: sending || widget.blocked
                                            ? null
                                            : send,
                                        background:
                                            (empty && !widget.canSendEmpty) ||
                                                sending ||
                                                widget.blocked
                                            ? c.accent.withValues(alpha: .35)
                                            : c.accent,
                                        foreground: c.onAccent,
                                      ),
                                  ],
                                ),
                              ].indexed)
                                Semantics(
                                  container: true,
                                  sortKey: OrdinalSortKey(i.toDouble()),
                                  child: part,
                                ),
                            ],
                          ),
                        ],
                      ),
                    ),
                  ),
                );
              },
            ),
            if (widget.footer.isNotEmpty)
              Padding(
                padding: const EdgeInsets.only(top: Space.gap),
                child: DefaultTextStyle.merge(
                  style: TextStyle(
                    fontSize: FontSizes.caption,
                    color: c.subtle,
                  ),
                  child: Wrap(
                    alignment: WrapAlignment.center,
                    spacing: 14,
                    runSpacing: 2,
                    children: [
                      for (final line in widget.footer)
                        Semantics(container: true, child: line),
                    ],
                  ),
                ),
              ),
          ],
        ),
      ),
    );
  }
}

/// One row (controls, field, controls) while the text fits between them;
/// otherwise the field takes its own row above the controls. The field stays
/// the same child either way, so focus and IME composition survive the switch.
class _ComposerRow extends MultiChildRenderObjectWidget {
  // The start side is tighter so the text sits close to the `+`.
  static const fieldStart = 8.0, fieldEnd = 12.0;
  final double textWidth;
  final bool multiline;
  const _ComposerRow({
    required this.textWidth,
    required this.multiline,
    required super.children,
  });

  @override
  RenderObject createRenderObject(BuildContext context) =>
      _RenderComposerRow(textWidth, multiline);

  @override
  void updateRenderObject(BuildContext context, _RenderComposerRow render) =>
      render
        ..textWidth = textWidth
        ..multiline = multiline;
}

class _ComposerParentData extends ContainerBoxParentData<RenderBox> {}

class _RenderComposerRow extends RenderBox
    with
        ContainerRenderObjectMixin<RenderBox, _ComposerParentData>,
        RenderBoxContainerDefaultsMixin<RenderBox, _ComposerParentData> {
  _RenderComposerRow(this._textWidth, this._multiline);
  static const gap = 4.0, minField = 140.0;

  double _textWidth;
  set textWidth(double value) {
    if (value == _textWidth) return;
    _textWidth = value;
    markNeedsLayout();
  }

  bool _multiline;
  set multiline(bool value) {
    if (value == _multiline) return;
    _multiline = value;
    markNeedsLayout();
  }

  @override
  void setupParentData(RenderBox child) {
    if (child.parentData is! _ComposerParentData) {
      child.parentData = _ComposerParentData();
    }
  }

  void _place(RenderBox child, double x, double y) =>
      (child.parentData! as _ComposerParentData).offset = Offset(x, y);

  @override
  void performLayout() {
    final lead = firstChild!, field = childAfter(lead)!;
    final trail = childAfter(field)!;
    final width = constraints.maxWidth;
    trail.layout(BoxConstraints(maxWidth: width), parentUsesSize: true);
    lead.layout(
      BoxConstraints(maxWidth: math.max(0, width - trail.size.width - gap)),
      parentUsesSize: true,
    );
    final rowField = width - lead.size.width - trail.size.width - gap;
    // A small allowance keeps the caret clear of the controls.
    final single =
        !_multiline &&
        rowField >= minField &&
        _textWidth + _ComposerRow.fieldStart + _ComposerRow.fieldEnd + 4 <=
            rowField;
    if (single) {
      field.layout(
        BoxConstraints.tightFor(width: rowField),
        parentUsesSize: true,
      );
      final height = [
        lead.size.height,
        field.size.height,
        trail.size.height,
      ].reduce(math.max);
      _place(lead, 0, (height - lead.size.height) / 2);
      _place(field, lead.size.width, (height - field.size.height) / 2);
      _place(trail, width - trail.size.width, (height - trail.size.height) / 2);
      size = constraints.constrain(Size(width, height));
      return;
    }
    field.layout(BoxConstraints.tightFor(width: width), parentUsesSize: true);
    final row = math.max(lead.size.height, trail.size.height);
    final top = field.size.height;
    _place(field, 0, 0);
    _place(lead, 0, top + (row - lead.size.height) / 2);
    _place(
      trail,
      width - trail.size.width,
      top + (row - trail.size.height) / 2,
    );
    size = constraints.constrain(Size(width, top + row));
  }

  @override
  void paint(PaintingContext context, Offset offset) =>
      defaultPaint(context, offset);

  @override
  bool hitTestChildren(BoxHitTestResult result, {required Offset position}) =>
      defaultHitTestChildren(result, position: position);
}

/// Run settings shown as pills inside the composer.
List<Widget> runTools(HubController hub, RunAction action) {
  if (hub.isDraft || hub.commandCatalog.isEmpty) return const [];
  final settings = hub.settings;
  final modes = (hub.commandCatalog['approval_modes'] as List? ?? const [])
      .cast<String>();
  final approval = settings['approval'] as String? ?? 'ask';
  return [
    // Work only: Chat has no approvals.
    if (hub.realWork && modes.isNotEmpty)
      ApprovalMenu(
        value: approval,
        modes: modes,
        onSelected: (mode) {
          if (mode != approval) action(() => hub.configure({'approval': mode}));
        },
      ),
    if (hub.supports('plan_mode'))
      Row(
        mainAxisSize: MainAxisSize.min,
        children: [
          FilterChip(
            label: Text(tr('计划模式', 'Plan mode')),
            selected: settings['plan'] == true,
            visualDensity: VisualDensity.compact,
            onSelected: (v) => action(() => hub.configure({'plan': v})),
          ),
          if (hub.emulated('plan_mode')) const EmulatedNote('plan_mode'),
        ],
      ),
  ];
}

/// The Work conversation's approval mode, as a pill; it applies from the next
/// turn.
class ApprovalMenu extends StatelessWidget {
  final String value;
  final List<String> modes;
  final ValueChanged<String> onSelected;
  const ApprovalMenu({
    super.key,
    required this.value,
    required this.modes,
    required this.onSelected,
  });

  @override
  Widget build(BuildContext context) => ChoicePill<String>(
    tooltip: tr('审批模式', 'Approval mode'),
    value: value,
    options: [for (final mode in modes) (mode, approvalLabel(mode))],
    detail: approvalDescription,
    icon: value == 'full_auto'
        ? Icons.bolt_outlined
        : Icons.verified_user_outlined,
    iconColor: value == 'full_auto' ? HubColors.of(context).warning : null,
    onChanged: onSelected,
  );
}

/// The model and its reasoning level, placed beside the send button.
Widget? modelControl(HubController hub, RunAction action) {
  if (hub.isDraft) {
    if (hub.draftModel == null) return null;
    final work = hub.mode == 'work';
    return ModelPicker(
      mode: hub.mode,
      chat: hub.chatModels,
      work: hub.workGroups,
      harness: work ? hub.harness : null,
      model: hub.draftModel,
      onPick: (harness, id, level) => hub.updateDraft(() {
        if (harness != null) hub.harness = harness;
        hub.draftModel = id;
        if (level != null) hub.draftReasoning = level;
      }),
      levelsFor: (harness, id) =>
          (hub.modelEntry(hub.mode, harness, id)?['reasoning_levels']
                      as List? ??
                  const [])
              .cast<String>(),
      level: hub.draftReasoning,
      proFor: (harness, id) => hub.offersPro(hub.mode, harness, id),
      pro: hub.draftPro,
      onPro: (_, id, on) => hub.updateDraft(() {
        hub.draftModel = id;
        hub.draftPro = on;
      }),
    );
  }
  if (hub.commandCatalog.isEmpty) return null;
  return conversationModelPicker(hub, action);
}

/// Switches in place within the conversation's provider (and harness); any
/// other model starts a new chat, because provider-native history does not
/// carry across wire formats.
Widget conversationModelPicker(HubController hub, RunAction action) {
  final settings = hub.settings;
  final work = hub.selectedConversation['mode'] == 'work';
  final harness = settings['harness'] as String?;
  final current = (hub.commandCatalog['models'] as List? ?? []).cast<String>();
  final provider = hub.commandCatalog['provider'] as String? ?? '';
  List<Map<String, dynamic>> withCurrent(List<Map<String, dynamic>> list) => [
    ...list,
    for (final id in current)
      if (!list.any((m) => m['id'] == id)) {'id': id, 'provider': provider},
  ];
  final groups = hub.workGroups;
  final List<Map<String, dynamic>> grouped = [
    for (final g in groups)
      g['harness'] == harness
          ? {
              ...g,
              'models': withCurrent([
                for (final m in g['models'] as List)
                  Map<String, dynamic>.from(m),
              ]),
            }
          : g,
    if (work && !groups.any((g) => g['harness'] == harness))
      {'harness': harness, 'models': withCurrent([])},
  ];
  final levels = hub.supports('thought_level')
      ? (hub.commandCatalog['reasoning_levels'] as List? ?? const [])
            .cast<String>()
      : const <String>[];
  return ModelPicker(
    // The conversation's catalog levels apply to every in-place model.
    levelsFor: (_, _) => levels,
    level: settings['reasoning'] as String?,
    mode: work ? 'work' : 'chat',
    chat: work ? hub.chatModels : withCurrent(hub.chatModels),
    work: grouped,
    harness: work ? harness : null,
    model: settings['model'],
    inPlace: (h, id) => (!work || h == harness) && current.contains(id),
    onPick: (h, id, level) {
      final patch = {
        if (id != settings['model']) 'model': id,
        if (level != null && level != settings['reasoning']) 'reasoning': level,
      };
      if (patch.isNotEmpty) action(() => hub.configure(patch));
    },
    proFor: (h, id) => hub.offersPro(work ? 'work' : 'chat', h, id),
    pro: settings['pro'] == true,
    onPro: (h, id, on) => action(
      () =>
          hub.configure({if (id != settings['model']) 'model': id, 'pro': on}),
    ),
    onNewChat: (h, id) => hub.startDraft(
      mode: work ? 'work' : 'chat',
      harness: h,
      model: id,
      project: hub.contextProject,
    ),
  );
}

/// The composer's `+`: a full-width panel under the box with the draft's
/// project, or the conversation's harness commands.
Widget? plusMenu(
  BuildContext context,
  HubController hub,
  RunAction action, {
  required VoidCallback onNewProject,
}) {
  // The menu's own context goes away when it closes; dialogs outlive it.
  final page = context;
  final commands = hub.isDraft
      ? const []
      : (hub.commandCatalog['harness_commands'] as List? ?? [])
            .where((c) => c is Map && c['name'] is String)
            .toList();
  List<Widget> items(BuildContext context) {
    final c = HubColors.of(context);
    Widget row(
      IconData icon,
      String title,
      String? subtitle,
      VoidCallback? onPressed, {
      bool checked = false,
    }) => MenuItemButton(
      onPressed: onPressed,
      leadingIcon: Icon(icon, size: 20),
      trailingIcon: checked ? Icon(Icons.check, size: 16, color: c.text) : null,
      child: Row(
        children: [
          Text(title),
          if (subtitle != null && subtitle.isNotEmpty) ...[
            const SizedBox(width: 10),
            Flexible(
              child: Text(
                subtitle,
                maxLines: 1,
                overflow: TextOverflow.ellipsis,
                style: TextStyle(color: c.subtle),
              ),
            ),
          ],
        ],
      ),
    );
    final files = [
      row(
        Icons.attach_file,
        tr('上传文件', 'Upload files'),
        null,
        () => pickFiles(page, hub),
      ),
      row(
        Icons.folder_copy_outlined,
        tr('从文件库添加', 'Add from library'),
        null,
        () => chooseFromLibrary(page, hub),
      ),
      const MenuDivider(),
    ];
    if (!hub.isDraft) {
      // The + stays in a conversation too, so the composer keeps one layout.
      return [
        ...files,
        if (hub.capabilities['plugins'] == true) ...[
          row(
            Icons.extension_outlined,
            tr('插件', 'Plugins'),
            null,
            () => showConversationPlugins(page, hub),
          ),
          const MenuDivider(),
        ],
        if (commands.isEmpty)
          row(
            Icons.terminal,
            tr('暂无可用命令', 'No commands available'),
            null,
            null,
          ),
        for (final command in commands)
          row(
            Icons.terminal,
            '/${command['name']}',
            command['description'] as String?,
            hub.running
                ? null
                : () => action(() => hub.send('/${command['name']}')),
          ),
      ];
    }
    return [
      ...files,
      Padding(
        padding: const EdgeInsets.fromLTRB(
          Space.inset,
          Space.gap,
          Space.inset,
          4,
        ),
        child: Text(
          tr('新对话放入项目', 'Start in a project'),
          style: TextStyle(fontSize: FontSizes.caption, color: c.subtle),
        ),
      ),
      row(
        Icons.chat_bubble_outline,
        tr('无项目', 'No project'),
        null,
        () => hub.startDraft(mode: hub.mode),
        checked: hub.project == null,
      ),
      for (final p in hub.activeProjects)
        row(
          Icons.folder_outlined,
          p['name'] as String,
          (p['instructions'] as String? ?? '').split('\n').first,
          () => hub.startDraft(project: p['id'], mode: hub.mode),
          checked: hub.project == p['id'],
        ),
      const MenuDivider(),
      row(
        Icons.create_new_folder_outlined,
        tr('新建项目…', 'New project…'),
        null,
        onNewProject,
      ),
    ];
  }

  return Builder(
    builder: (context) => IconButton(
      tooltip: tr('添加与命令', 'Add and commands'),
      // A round highlight for the round control, unlike the theme's squircle.
      style: IconButton.styleFrom(shape: const CircleBorder()),
      icon: Icon(Icons.add, size: 22, color: HubColors.of(context).text),
      onPressed: () =>
          ComposerMenus.maybeOf(context)?.toggle('plus', items, full: true),
    ),
  );
}

/// Commands matching what follows the typed `/`, grouped, the highlighted one
/// shaded; a click picks one.
class _Suggestions extends StatelessWidget {
  final List<SlashCommand> commands;
  final int highlighted;
  final ValueChanged<SlashCommand> onPick;
  const _Suggestions({
    required this.commands,
    required this.highlighted,
    required this.onPick,
  });

  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    final rows = <Widget>[];
    String? group;
    for (final (i, command) in commands.indexed) {
      if (command.group != group) {
        group = command.group;
        rows.add(
          Padding(
            padding: const EdgeInsets.fromLTRB(
              Space.inset,
              Space.gap,
              Space.inset,
              2,
            ),
            child: Text(
              command.group,
              style: TextStyle(fontSize: FontSizes.caption, color: c.subtle),
            ),
          ),
        );
      }
      rows.add(
        Semantics(
          selected: i == highlighted,
          button: true,
          child: InkWell(
            onTap: () => onPick(command),
            child: Container(
              color: i == highlighted ? c.hover : null,
              padding: const EdgeInsets.symmetric(
                horizontal: Space.inset,
                vertical: 7,
              ),
              child: Row(
                children: [
                  Text(
                    '/${command.name}',
                    style: TextStyle(fontSize: FontSizes.body, color: c.text),
                  ),
                  const SizedBox(width: 12),
                  Expanded(
                    child: Text(
                      command.description,
                      maxLines: 1,
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
          ),
        ),
      );
    }
    return Container(
      margin: const EdgeInsets.only(bottom: 6),
      constraints: const BoxConstraints(maxHeight: 260),
      decoration: BoxDecoration(
        color: c.menu,
        borderRadius: BorderRadius.circular(Radii.block),
        border: Border.all(color: c.border),
      ),
      clipBehavior: Clip.antiAlias,
      child: SingleChildScrollView(
        padding: const EdgeInsets.only(bottom: 4),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.stretch,
          children: rows,
        ),
      ),
    );
  }
}

/// The commands `/` offers in a conversation: the ones every harness has,
/// then the platform's, then the project's and plugins', then the harness's
/// own, which are sent as text.
List<SlashCommand> slashCommands(
  BuildContext context,
  HubController hub,
  RunAction action, {
  required VoidCallback onNewChat,
}) {
  if (hub.isDraft) return const [];
  final conversation = hub.selectedConversation;
  final unified = tr('通用命令', 'Commands');
  final platform = tr('对话', 'Conversation');
  final agent = tr('Agent 命令', 'Agent commands');
  return [
    if (hub.supports('compact') && !hub.running)
      SlashCommand(
        'compact',
        hub.emulated('compact')
            ? emulatedNote('compact')!
            : tr('压缩上下文，释放空间', 'Compact the context to free room'),
        unified,
        run: () => action(hub.compact),
      ),
    if (hub.running)
      SlashCommand(
        'stop',
        tr('停止当前任务', 'Stop the current run'),
        unified,
        run: () => action(hub.cancel),
      ),
    SlashCommand(
      'new',
      tr('新建对话', 'Start a new chat'),
      platform,
      run: onNewChat,
    ),
    for (final (name, label, value) in [
      ('rename', tr('重命名对话', 'Rename the chat'), 'rename'),
      ('archive', tr('归档对话', 'Archive the chat'), 'archive'),
      ('export', tr('导出对话', 'Export the chat'), 'export'),
    ])
      SlashCommand(
        name,
        label,
        platform,
        run: () =>
            action(() => conversationAction(context, hub, conversation, value)),
      ),
    // Plugin commands run as InvokeMenuCommand, after their parameters.
    for (final command
        in (hub.commandCatalog['plugin_commands'] as List? ?? []))
      if (command is Map && command['name'] is String)
        SlashCommand(
          '${command['plugin']}:${command['name']}',
          '${command['title'] ?? ''} ${command['description'] ?? ''}'.trim(),
          tr('插件命令', 'Plugin commands'),
          run: () => action(() async {
            final params = await askCommandParams(context, command);
            if (params == null) return;
            await hub.invokeMenuCommand(
              '${command['installation']}',
              '${command['name']}',
              params,
            );
          }),
        ),
    for (final command
        in (hub.commandCatalog['harness_commands'] as List? ?? []))
      if (command is Map && command['name'] is String)
        SlashCommand(
          command['name'] as String,
          '${command['description'] ?? ''}',
          agent,
        ),
  ];
}

List<Widget> composerFooter(HubController hub) => [
  // A Work harness that cannot steer the turn in progress gets the
  // instructions as its next turn.
  if (!hub.isDraft && hub.realWork && hub.emulated('steer'))
    const EmulatedNote('steer'),
];
