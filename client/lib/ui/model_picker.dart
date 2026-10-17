import 'package:flutter/material.dart';

import '../l10n.dart';
import '../theme.dart';
import 'common.dart';
import 'icons.dart';

String providerLabel(String provider) => switch (provider) {
  'anthropic' => 'Anthropic',
  'openai' => 'OpenAI',
  _ => provider,
};

/// One picker for the model. Chat lists models by provider; Work lists
/// harnesses, each opening a flyout with its models. A model's reasoning
/// levels, and Pro mode where it has it, open in a flyout to its right, so
/// harness, model and level are all chosen from one control. Models that
/// cannot be switched to in place (another provider or harness) start a new
/// chat instead.
class ModelPicker extends StatefulWidget {
  final String mode;
  final List<Map<String, dynamic>> chat;
  final List<Map<String, dynamic>> work;
  final String? harness, model;
  final bool Function(String? harness, String id)? inPlace;

  /// [level] is null when the model row itself was chosen.
  final void Function(String? harness, String id, String? level) onPick;
  final void Function(String? harness, String id)? onNewChat;

  /// Reasoning levels offered for a model; empty hides its flyout.
  final List<String> Function(String? harness, String id)? levelsFor;

  /// The current level; null is the server default (medium).
  final String? level;

  /// Whether a model offers OpenAI's Pro mode, whether it is on, and the
  /// switch.
  final bool Function(String? harness, String id)? proFor;
  final bool pro;
  final void Function(String? harness, String id, bool on)? onPro;
  const ModelPicker({
    super.key,
    required this.mode,
    required this.chat,
    required this.work,
    required this.model,
    required this.onPick,
    this.harness,
    this.inPlace,
    this.onNewChat,
    this.levelsFor,
    this.level,
    this.proFor,
    this.pro = false,
    this.onPro,
  });
  @override
  State<ModelPicker> createState() => _ModelPickerState();
}

class _ModelPickerState extends State<ModelPicker> {
  final controller = MenuController();

  bool here(String? harness, String id) =>
      widget.onNewChat == null || (widget.inPlace?.call(harness, id) ?? true);

  List<String> levels(String? harness, String? id) =>
      id == null ? const [] : widget.levelsFor?.call(harness, id) ?? const [];

  bool offersPro(String? harness, String? id) =>
      id != null && (widget.proFor?.call(harness, id) ?? false);

  String? shown(List<String> levels) => levels.isEmpty
      ? null
      : levels.contains(widget.level)
      ? widget.level
      : levels.contains('medium')
      ? 'medium'
      : null;

  Widget check(BuildContext context) =>
      Icon(Icons.check, size: 16, color: HubColors.of(context).text);

  Widget header(BuildContext context, String text) => Padding(
    padding: const EdgeInsets.fromLTRB(Space.inset, Space.gap, Space.inset, 4),
    child: Text(
      text,
      style: TextStyle(
        fontSize: FontSizes.caption,
        color: HubColors.of(context).subtle,
      ),
    ),
  );

  Widget modelRow(
    BuildContext context,
    String? harness,
    Map<String, dynamic> m,
    VoidCallback close,
  ) {
    final c = HubColors.of(context);
    final id = m['id'] as String;
    final inPlace = here(harness, id);
    final current = inPlace && id == widget.model && harness == widget.harness;
    final label = ConstrainedBox(
      constraints: const BoxConstraints(minWidth: 160),
      child: Text(id),
    );
    final options = inPlace ? levels(harness, id) : const <String>[];
    if (options.isEmpty) {
      return MenuItemButton(
        onPressed: () => inPlace
            ? widget.onPick(harness, id, null)
            : widget.onNewChat!(harness, id),
        trailingIcon: current
            ? check(context)
            : inPlace
            ? null
            : HubIcon(HubIcons.newChat, size: 16, color: c.subtle),
        child: label,
      );
    }
    final active = current ? shown(options) : null;
    final pro = current && widget.pro;
    return FlyoutButton(
      onPressed: () {
        close();
        widget.onPick(harness, id, null);
      },
      trailingIcon: current ? check(context) : null,
      menuChildren: [
        header(context, tr('思考强度', 'Reasoning')),
        for (final v in options)
          MenuItemButton(
            onPressed: () => widget.onPick(harness, id, v),
            trailingIcon: v == active ? check(context) : null,
            child: ConstrainedBox(
              constraints: const BoxConstraints(minWidth: 120),
              child: Text(reasoningLabel(v)),
            ),
          ),
        if (offersPro(harness, id)) ...[
          const MenuDivider(),
          MenuItemButton(
            onPressed: () => widget.onPro!(harness, id, !pro),
            trailingIcon: pro ? check(context) : null,
            child: const Text('Pro'),
          ),
        ],
      ],
      child: label,
    );
  }

  List<Widget> chatItems(BuildContext context, VoidCallback close) {
    final current = widget.chat.where((m) => here(null, m['id'])).toList();
    final others = widget.chat.where((m) => !here(null, m['id'])).toList();
    final providers = <String>[];
    for (final m in current) {
      if (!providers.contains(m['provider'])) providers.add(m['provider']);
    }
    return [
      for (final p in providers)
        MenuGroup(
          id: 'models:$p',
          title: providerLabel(p),
          children: [
            for (final m in current.where((m) => m['provider'] == p))
              modelRow(context, null, m, close),
          ],
        ),
      if (others.isNotEmpty) ...[
        const MenuDivider(),
        MenuGroup(
          id: 'models:new-chat',
          title: tr('新对话', 'New chat'),
          children: [for (final m in others) modelRow(context, null, m, close)],
        ),
      ],
    ];
  }

  List<Widget> workItems(BuildContext context, VoidCallback close) => [
    header(context, tr('执行器', 'Harness')),
    for (final g in widget.work) harnessRow(context, g, close),
  ];

  Widget harnessRow(
    BuildContext context,
    Map<String, dynamic> group,
    VoidCallback close,
  ) {
    final h = group['harness'] as String;
    final models = [
      for (final m in group['models'] as List) Map<String, dynamic>.from(m),
    ];
    final elsewhere = models.every((m) => !here(h, m['id']));
    return FlyoutButton(
      leadingIcon: Icon(
        h == 'claude' ? Icons.auto_awesome_outlined : Icons.terminal,
        size: 17,
      ),
      trailingIcon: h == widget.harness ? check(context) : null,
      menuChildren: [for (final m in models) modelRow(context, h, m, close)],
      child: ConstrainedBox(
        constraints: const BoxConstraints(minWidth: 140),
        child: Text(
          elsewhere
              ? '${pathLabel(h)} · ${tr('新对话', 'new chat')}'
              : pathLabel(h),
        ),
      ),
    );
  }

  List<Widget> items(BuildContext context, VoidCallback close) =>
      widget.mode == 'work'
      ? workItems(context, close)
      : chatItems(context, close);

  Widget label(BuildContext context, VoidCallback onTap) {
    final c = HubColors.of(context);
    final compact = MediaQuery.sizeOf(context).width < narrowWidth;
    final level = shown(levels(widget.harness, widget.model));
    final pro = widget.pro && offersPro(widget.harness, widget.model);
    // No hover tooltip: it would cover the footer or the open menu. The
    // label still names the control for assistive tech.
    return Semantics(
      container: true,
      button: true,
      label: tr('模型', 'Model'),
      // Borderless: the model and its reasoning level read as one label. Its
      // own Material shows the hover ink above the composer's background.
      child: Material(
        type: MaterialType.transparency,
        child: InkWell(
          borderRadius: BorderRadius.circular(Radii.control),
          hoverColor: HubColors.of(context).hover,
          onTap: onTap,
          child: Container(
            height: 36,
            padding: const EdgeInsets.symmetric(horizontal: 12),
            child: Row(
              mainAxisSize: MainAxisSize.min,
              children: [
                ConstrainedBox(
                  constraints: BoxConstraints(maxWidth: compact ? 110 : 200),
                  child: Text(
                    widget.model ?? '',
                    maxLines: 1,
                    overflow: TextOverflow.ellipsis,
                    style: modelLabelStyle(size: FontSizes.body, color: c.text),
                  ),
                ),
                if (level != null) ...[
                  const SizedBox(width: 6),
                  Text(
                    pro
                        ? '${reasoningLabel(level)} · Pro'
                        : reasoningLabel(level),
                    style: modelLabelStyle(
                      size: FontSizes.caption,
                      color: c.subtle,
                    ),
                  ),
                ],
                const SizedBox(width: 2),
                Icon(Icons.expand_more, size: 16, color: c.subtle),
              ],
            ),
          ),
        ),
      ),
    );
  }

  @override
  Widget build(BuildContext context) {
    // Inside the composer the menu hangs from the composer box itself.
    final composer = ComposerMenus.maybeOf(context);
    if (composer != null) {
      return label(
        context,
        () => composer.toggle(
          'model',
          (context) => items(context, composer.close),
        ),
      );
    }
    return MenuAnchor(
      controller: controller,
      alignmentOffset: const Offset(0, 4),
      menuChildren: items(context, controller.close),
      builder: (context, controller, _) => label(
        context,
        () => controller.isOpen ? controller.close() : controller.open(),
      ),
    );
  }
}
