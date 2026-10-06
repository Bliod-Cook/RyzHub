import 'dart:math' as math;

import 'package:flutter/material.dart';
import 'package:flutter/rendering.dart';
import 'package:flutter/services.dart';

import '../controller.dart';
import '../l10n.dart';
import '../theme.dart';
import 'common.dart';
import 'model_picker.dart';

class Composer extends StatefulWidget {
  final Future<void> Function(String) onSend;
  final VoidCallback onCancel;
  final bool running;
  final bool steer;
  final Future<void> Function(String)? onSideQuery;
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
  const Composer({
    super.key,
    required this.onSend,
    required this.onCancel,
    required this.running,
    this.steer = false,
    this.onSideQuery,
    this.leading,
    this.tools = const [],
    this.trailing,
    this.stacked = false,
    this.footer = const [],
    this.enterToSend = true,
    this.focusNode,
  });
  @override
  State<Composer> createState() => _ComposerState();
}

class _ComposerState extends State<Composer> {
  final input = TextEditingController();

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
  bool sideQuery = false;
  bool empty = true;

  @override
  void initState() {
    super.initState();
    // Every edit can change whether the text still fits on the single row.
    input.addListener(() => setState(() => empty = input.text.trim().isEmpty));
  }

  @override
  void dispose() {
    input.dispose();
    super.dispose();
  }

  Future<void> send() async {
    if (sending ||
        input.text.trim().isEmpty ||
        !input.value.composing.isCollapsed) {
      return;
    }
    setState(() => sending = true);
    try {
      await (sideQuery && widget.onSideQuery != null
          ? widget.onSideQuery!(input.text)
          : widget.onSend(input.text));
      input.clear();
    } catch (_) {
      // The caller reports the failure; the text stays for a retry.
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
    final canSend =
        !widget.running || widget.steer || widget.onSideQuery != null;
    final stop = round(
      tooltip: tr('停止', 'Stop'),
      icon: Icons.stop_rounded,
      onPressed: widget.onCancel,
      background: canSend ? c.composer : c.text,
      foreground: canSend ? c.text : c.background,
      border: canSend ? c.border : null,
    );
    final fieldStyle = TextStyle(fontSize: 15, height: 1.5, color: c.text);
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
            // Separate semantics nodes: otherwise the hint and footer lines merge
            // into the text field's accessible name.
            if (widget.steer && !sideQuery)
              Padding(
                padding: const EdgeInsets.fromLTRB(18, 0, 18, 6),
                child: Semantics(
                  container: true,
                  child: Text(
                    tr(
                      '补充指令将在下一次模型请求前采用',
                      'Instructions apply before the next model request',
                    ),
                    style: TextStyle(fontSize: 11.5, color: c.subtle),
                  ),
                ),
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
                    padding: const EdgeInsets.fromLTRB(8, 6, 8, 6),
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
                      child: _ComposerRow(
                        textWidth: textWidth,
                        multiline: widget.stacked || input.text.contains('\n'),
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
                                        if (widget.onSideQuery != null)
                                          Padding(
                                            padding: const EdgeInsets.only(
                                              left: 4,
                                            ),
                                            child: FilterChip(
                                              label: Text(
                                                tr('只读旁问', 'Side question'),
                                              ),
                                              avatar: sideQuery
                                                  ? null
                                                  : Icon(
                                                      Icons.visibility_outlined,
                                                      size: 15,
                                                      color: c.muted,
                                                    ),
                                              selected: sideQuery,
                                              visualDensity:
                                                  VisualDensity.compact,
                                              onSelected: (value) => setState(
                                                () => sideQuery = value,
                                              ),
                                            ),
                                          ),
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
                              child: TextField(
                                controller: input,
                                focusNode: widget.focusNode,
                                minLines: 1,
                                maxLines: 8,
                                textInputAction: TextInputAction.newline,
                                style: fieldStyle,
                                decoration: InputDecoration(
                                  hintText: tr('写下你的想法…', 'Ask anything…'),
                                  border: InputBorder.none,
                                  enabledBorder: InputBorder.none,
                                  focusedBorder: InputBorder.none,
                                  filled: false,
                                  isDense: true,
                                  contentPadding: const EdgeInsets.fromLTRB(
                                    _ComposerRow.fieldStart,
                                    10,
                                    _ComposerRow.fieldEnd,
                                    10,
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
                                    onPressed: sending ? null : send,
                                    background: empty || sending
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
                    ),
                  ),
                );
              },
            ),
            if (widget.footer.isNotEmpty)
              Padding(
                padding: const EdgeInsets.only(top: 8),
                child: DefaultTextStyle.merge(
                  style: TextStyle(fontSize: 11.5, color: c.subtle),
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
  if (hub.isDraft ||
      hub.commandCatalog.isEmpty ||
      (!hub.realChat && !hub.realWork)) {
    return const [];
  }
  final settings = hub.settings;
  return [
    if (hub.supports('plan_mode'))
      FilterChip(
        label: Text(tr('计划模式', 'Plan mode')),
        selected: settings['plan'] == true,
        visualDensity: VisualDensity.compact,
        onSelected: (v) => action(() => hub.configure({'plan': v})),
      ),
  ];
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
    );
  }
  if (hub.commandCatalog.isEmpty || (!hub.realChat && !hub.realWork)) {
    return null;
  }
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
    if (!hub.isDraft) {
      // The + stays in a conversation too, so the composer keeps one layout.
      return [
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
      Padding(
        padding: const EdgeInsets.fromLTRB(14, 8, 14, 4),
        child: Text(
          tr('新对话放入项目', 'Start in a project'),
          style: TextStyle(fontSize: 12, color: c.subtle),
        ),
      ),
      row(
        Icons.chat_bubble_outline,
        tr('无项目', 'No project'),
        tr('不关联任何项目', 'Not linked to a project'),
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
        tr('设置指令和知识文件后在其中开始', 'Set instructions and files, then start in it'),
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

List<Widget> composerFooter(HubController hub) => [
  if (!hub.isDraft && hub.realWork && hub.settings['harness'] == 'claude')
    Text(tr('补充指令排入下一轮', 'Instructions queue for the next turn')),
];
