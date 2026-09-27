import 'package:flutter/foundation.dart';
import 'package:flutter/gestures.dart';
import 'package:flutter/material.dart';

import '../prefs.dart';
import '../theme.dart';

typedef RunAction = Future<void> Function(Future<void> Function() task);

/// Navigation the sidebar, dialogs and settings ask the home page to perform.
abstract class HubActions {
  RunAction get run;
  void newChat();
  void openSearch();
  void toggleSidebar();
  void openSettings([String? section]);
  void openConversation(String id);
  void openProject(String id);
  void manageProjects([Map<String, dynamic>? project]);
  Future<void> logout();
}

/// Touch platforms have no hover, so hover-revealed controls stay visible.
bool get touchPlatform =>
    defaultTargetPlatform == TargetPlatform.android ||
    defaultTargetPlatform == TargetPlatform.iOS;

const contentWidth = 768.0;

class Hover extends StatefulWidget {
  final Widget Function(BuildContext context, bool hovered) builder;
  const Hover({super.key, required this.builder});
  @override
  State<Hover> createState() => _HoverState();
}

class _HoverState extends State<Hover> {
  bool hovered = false;
  @override
  Widget build(BuildContext context) => MouseRegion(
    onEnter: (_) => setState(() => hovered = true),
    onExit: (_) => setState(() => hovered = false),
    child: widget.builder(context, hovered),
  );
}

/// Keeps a hover-only control in the semantics tree for assistive tech and tests.
class Reveal extends StatelessWidget {
  final bool visible;
  final Widget child;
  const Reveal({super.key, required this.visible, required this.child});
  @override
  Widget build(BuildContext context) => AnimatedOpacity(
    opacity: visible || touchPlatform ? 1 : 0,
    duration: const Duration(milliseconds: 120),
    alwaysIncludeSemantics: true,
    child: child,
  );
}

class SectionLabel extends StatelessWidget {
  final String text;
  final Widget? trailing;
  const SectionLabel(this.text, {super.key, this.trailing});
  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    return Padding(
      padding: const EdgeInsets.fromLTRB(16, 18, 8, 4),
      child: SizedBox(
        height: 26,
        child: Row(
          children: [
            Expanded(
              child: Text(
                text,
                style: TextStyle(fontSize: 12.5, color: c.subtle),
              ),
            ),
            ?trailing,
          ],
        ),
      ),
    );
  }
}

class Avatar extends StatelessWidget {
  final String name;
  final double size;
  const Avatar(this.name, {super.key, this.size = 28});
  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    final letter = name.isEmpty ? '?' : name.characters.first.toUpperCase();
    return Container(
      width: size,
      height: size,
      alignment: Alignment.center,
      decoration: BoxDecoration(color: c.accent, shape: BoxShape.circle),
      child: Text(
        letter,
        style: TextStyle(
          color: c.onAccent,
          fontSize: size * .45,
          fontWeight: FontWeight.w600,
        ),
      ),
    );
  }
}

/// A rounded row used by the sidebar and the settings navigation.
class NavRow extends StatelessWidget {
  final IconData? icon;

  /// Replaces [icon], e.g. with an SVG glyph.
  final Widget? leading;
  final String label;
  final bool selected;
  final VoidCallback onTap;
  final Widget? trailing;
  const NavRow({
    super.key,
    this.icon,
    this.leading,
    required this.label,
    required this.onTap,
    this.selected = false,
    this.trailing,
  });
  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    return Padding(
      padding: const EdgeInsets.symmetric(horizontal: 8, vertical: 2),
      child: Semantics(
        container: true,
        button: true,
        selected: selected,
        child: Material(
          color: selected ? c.selected : Colors.transparent,
          borderRadius: BorderRadius.circular(10),
          child: InkWell(
            borderRadius: BorderRadius.circular(10),
            hoverColor: c.hover,
            onTap: onTap,
            child: SizedBox(
              height: 36,
              child: Row(
                children: [
                  const SizedBox(width: 10),
                  leading ?? Icon(icon, size: 18, color: c.text),
                  const SizedBox(width: 10),
                  Expanded(
                    child: Text(
                      label,
                      maxLines: 1,
                      overflow: TextOverflow.ellipsis,
                      style: TextStyle(fontSize: 14, color: c.text),
                    ),
                  ),
                  ?trailing,
                  const SizedBox(width: 6),
                ],
              ),
            ),
          ),
        ),
      ),
    );
  }
}

/// A pill-shaped popup picker. The tooltip is its accessible name.
class PillMenu<T> extends StatelessWidget {
  final String tooltip;
  final IconData icon;
  final String? label;
  final T? value;
  final List<(T, String)> items;
  final ValueChanged<T> onSelected;
  final bool enabled;
  const PillMenu({
    super.key,
    required this.tooltip,
    required this.icon,
    required this.items,
    required this.onSelected,
    this.label,
    this.value,
    this.enabled = true,
  });
  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    final compact = MediaQuery.sizeOf(context).width < 560;
    return PopupMenuButton<T>(
      tooltip: tooltip,
      enabled: enabled,
      position: PopupMenuPosition.under,
      // Clear of the button and its highlight.
      offset: const Offset(0, 6),
      onSelected: onSelected,
      itemBuilder: (_) => [
        for (final (v, text) in items)
          HubMenuItem(
            value: v,
            height: 38,
            child: Row(
              children: [
                Expanded(child: Text(text)),
                if (v == value) ...[
                  const SizedBox(width: 12),
                  Icon(Icons.check, size: 16, color: c.text),
                ],
              ],
            ),
          ),
      ],
      child: Container(
        height: 34,
        padding: EdgeInsets.symmetric(horizontal: compact ? 8 : 10),
        decoration: BoxDecoration(
          borderRadius: BorderRadius.circular(17),
          border: Border.all(color: c.border),
        ),
        child: Row(
          mainAxisSize: MainAxisSize.min,
          children: [
            Icon(icon, size: 16, color: enabled ? c.muted : c.subtle),
            if (label != null && !compact) ...[
              const SizedBox(width: 6),
              ConstrainedBox(
                constraints: const BoxConstraints(maxWidth: 170),
                child: Text(
                  label!,
                  maxLines: 1,
                  overflow: TextOverflow.ellipsis,
                  style: TextStyle(
                    fontSize: 13,
                    color: enabled ? c.text : c.subtle,
                  ),
                ),
              ),
            ],
            const SizedBox(width: 2),
            Icon(Icons.expand_more, size: 16, color: c.subtle),
          ],
        ),
      ),
    );
  }
}

class EmptyHint extends StatelessWidget {
  final String text;
  const EmptyHint(this.text, {super.key});
  @override
  Widget build(BuildContext context) => Padding(
    padding: const EdgeInsets.fromLTRB(16, 4, 16, 6),
    child: Text(
      text,
      style: TextStyle(fontSize: 13, color: HubColors.of(context).subtle),
    ),
  );
}

/// A menu row with a flyout to its right, e.g. a model's reasoning levels or a
/// harness's models. Hovering shows the flyout, so a mouse click runs
/// [onPressed] instead of toggling it shut; a touch tap opens the flyout.
class FlyoutButton extends StatefulWidget {
  final Widget child;
  final Widget? leadingIcon, trailingIcon;
  final List<Widget> menuChildren;
  final VoidCallback? onPressed;
  const FlyoutButton({
    super.key,
    required this.child,
    required this.menuChildren,
    this.leadingIcon,
    this.trailingIcon,
    this.onPressed,
  });
  @override
  State<FlyoutButton> createState() => _FlyoutButtonState();
}

class _FlyoutButtonState extends State<FlyoutButton> {
  final controller = MenuController();
  bool mouse = false;
  @override
  Widget build(BuildContext context) => Listener(
    onPointerDown: (event) => mouse = event.kind == PointerDeviceKind.mouse,
    onPointerUp: (_) {
      if (!mouse) return;
      // Runs after the button's own tap handler has toggled the flyout.
      Future.microtask(() {
        if (!mounted) return;
        if (widget.onPressed != null) {
          widget.onPressed!();
        } else if (!controller.isOpen) {
          controller.open();
        }
      });
    },
    child: SubmenuButton(
      controller: controller,
      // The row sits inside the panel's padding; shift the flyout past the
      // panel edge so the two cards sit side by side instead of overlapping.
      // The offset is mirrored when the flyout flips to the left.
      alignmentOffset: const Offset(12, 0),
      // An outline chevron rather than the default filled arrow.
      submenuIcon: const WidgetStatePropertyAll(
        Icon(Icons.chevron_right, size: 18),
      ),
      leadingIcon: widget.leadingIcon,
      trailingIcon: widget.trailingIcon,
      menuChildren: widget.menuChildren,
      child: widget.child,
    ),
  );
}

/// Lets controls inside the composer open menus anchored to the whole box:
/// full width for `+`, or a panel at its trailing edge for the model.
class ComposerMenus extends InheritedWidget {
  final void Function(
    String id,
    List<Widget> Function(BuildContext context) items, {
    bool full,
  })
  toggle;
  final VoidCallback close;
  const ComposerMenus({
    super.key,
    required this.toggle,
    required this.close,
    required super.child,
  });
  static ComposerMenus? maybeOf(BuildContext context) =>
      context.dependOnInheritedWidgetOfExactType<ComposerMenus>();
  @override
  bool updateShouldNotify(ComposerMenus oldWidget) => false;
}

/// A popup menu item whose highlight is a rounded rectangle inset from the
/// menu edge, like the anchored menus' items.
class HubMenuItem<T> extends PopupMenuItem<T> {
  const HubMenuItem({
    super.key,
    super.value,
    super.onTap,
    super.enabled,
    super.height,
    super.padding,
    required super.child,
  });
  @override
  PopupMenuItemState<T, HubMenuItem<T>> createState() => _HubMenuItemState<T>();
}

class _HubMenuItemState<T> extends PopupMenuItemState<T, HubMenuItem<T>> {
  // Ink paints on the nearest Material; a transparent rounded one clips the
  // highlight to its shape without touching the item's menuitem semantics.
  @override
  Widget buildSemantics({required Widget child}) => super.buildSemantics(
    child: Material(
      type: MaterialType.transparency,
      shape: RoundedRectangleBorder(borderRadius: BorderRadius.circular(12)),
      clipBehavior: Clip.antiAlias,
      child: child,
    ),
  );
}

/// A foldable group inside a menu, like the sidebar sections: hovering shows a
/// chevron and clicking the header row folds or unfolds it without closing
/// the menu. The state lives in this device's preferences.
class MenuGroup extends StatelessWidget {
  final String id, title;
  final List<Widget> children;
  const MenuGroup({
    super.key,
    required this.id,
    required this.title,
    required this.children,
  });
  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    final folded = prefs.collapsedSections.contains(id);
    return Column(
      mainAxisSize: MainAxisSize.min,
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        Hover(
          builder: (context, hovered) => Semantics(
            container: true,
            button: true,
            expanded: !folded,
            child: Clickable(
              onTap: () => prefs.update((p) {
                if (!p.collapsedSections.remove(id)) {
                  p.collapsedSections.add(id);
                }
              }),
              child: Padding(
                padding: const EdgeInsets.fromLTRB(14, 8, 14, 4),
                child: Row(
                  children: [
                    Text(
                      title,
                      style: TextStyle(fontSize: 12, color: c.subtle),
                    ),
                    const SizedBox(width: 2),
                    if (hovered || folded || touchPlatform)
                      Icon(
                        folded ? Icons.chevron_right : Icons.expand_more,
                        size: 14,
                        color: c.subtle,
                      ),
                  ],
                ),
              ),
            ),
          ),
        ),
        if (!folded) ...children,
      ],
    );
  }
}

/// A tap target whose only feedback is the pointer; for headers that fold a
/// section, where a highlight box would read as a separate button.
class Clickable extends StatelessWidget {
  final VoidCallback onTap;
  final Widget child;
  const Clickable({super.key, required this.onTap, required this.child});
  @override
  Widget build(BuildContext context) => MouseRegion(
    cursor: SystemMouseCursors.click,
    child: GestureDetector(
      behavior: HitTestBehavior.opaque,
      onTap: onTap,
      child: child,
    ),
  );
}

/// A divider for anchored menus, spaced so item highlights keep clear of it.
class MenuDivider extends StatelessWidget {
  const MenuDivider({super.key});
  @override
  Widget build(BuildContext context) => const Padding(
    padding: EdgeInsets.symmetric(vertical: 4),
    child: Divider(),
  );
}
