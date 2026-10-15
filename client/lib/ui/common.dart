import 'package:flutter/foundation.dart';
import 'package:flutter/gestures.dart';
import 'package:flutter/material.dart';

import '../l10n.dart';
import '../prefs.dart';
import '../theme.dart';

typedef RunAction = Future<void> Function(Future<void> Function() task);

/// Navigation the sidebar, dialogs and settings ask the home page to perform.
abstract class HubActions {
  RunAction get run;
  void newChat();
  void openSearch();
  void openLibrary();
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

/// From this window width the sidebar docks beside the page and settings
/// show both panes.
const wideWidth = 840.0;

/// Below this width boxes take less room: narrower margins and labels.
const narrowWidth = 640.0;

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
  const SectionLabel(this.text, {super.key});
  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    return Padding(
      padding: const EdgeInsets.fromLTRB(Space.edge, 18, Space.gap, 4),
      child: SizedBox(
        height: 26,
        child: Align(
          alignment: Alignment.centerLeft,
          child: Text(
            text,
            style: TextStyle(fontSize: FontSizes.caption, color: c.subtle),
          ),
        ),
      ),
    );
  }
}

class Avatar extends StatelessWidget {
  final String name;
  const Avatar(this.name, {super.key});
  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    final letter = name.isEmpty ? '?' : name.characters.first.toUpperCase();
    return Container(
      width: 28,
      height: 28,
      alignment: Alignment.center,
      decoration: BoxDecoration(color: c.accent, shape: BoxShape.circle),
      child: Text(
        letter,
        style: TextStyle(
          color: c.onAccent,
          fontSize: FontSizes.label,
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
      padding: const EdgeInsets.symmetric(horizontal: Space.gap, vertical: 2),
      child: Semantics(
        container: true,
        button: true,
        selected: selected,
        child: Material(
          color: selected ? c.selected : Colors.transparent,
          borderRadius: BorderRadius.circular(Radii.control),
          child: InkWell(
            borderRadius: BorderRadius.circular(Radii.control),
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
                      style: TextStyle(fontSize: FontSizes.body, color: c.text),
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

/// An info mark beside a control the platform emulates: tap or hover for
/// what it does instead.
class EmulatedNote extends StatelessWidget {
  final String capability;
  const EmulatedNote(this.capability, {super.key});
  @override
  Widget build(BuildContext context) {
    final text = emulatedNote(capability);
    if (text == null) return const SizedBox.shrink();
    return Tooltip(
      message: text,
      triggerMode: TooltipTriggerMode.tap,
      child: Semantics(
        label: text,
        child: Padding(
          padding: const EdgeInsets.all(4),
          child: Icon(
            Icons.info_outline,
            size: 15,
            color: HubColors.of(context).subtle,
          ),
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
    padding: const EdgeInsets.fromLTRB(Space.edge, 4, Space.edge, 6),
    child: Text(
      text,
      style: TextStyle(
        fontSize: FontSizes.label,
        color: HubColors.of(context).subtle,
      ),
    ),
  );
}

/// A menu row with a flyout to its right, e.g. a model's reasoning levels or a
/// harness's models. Hovering shows the flyout, so a mouse click runs
/// [onPressed] instead of toggling it shut, as does an assistive activation
/// (Web delivers clicks that way once semantics are on); a touch tap opens the
/// flyout.
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

  void press() {
    if (widget.onPressed != null) {
      widget.onPressed!();
    } else if (!controller.isOpen) {
      controller.open();
    }
  }

  @override
  Widget build(BuildContext context) => Listener(
    onPointerDown: (event) => mouse = event.kind == PointerDeviceKind.mouse,
    onPointerUp: (_) {
      if (!mouse) return;
      // Runs after the button's own tap handler has toggled the flyout.
      Future.microtask(() {
        if (mounted) press();
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
      // Merged into the row's node, this tap replaces the button's toggle.
      child: Semantics(onTap: press, child: widget.child),
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
    super.enabled,
    super.height,
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
      shape: RoundedRectangleBorder(
        borderRadius: BorderRadius.circular(Radii.field),
      ),
      clipBehavior: Clip.antiAlias,
      child: child,
    ),
  );
}

/// A section title that folds its section: hovering shows a chevron, a folded
/// section keeps it, and the whole row toggles without closing a menu around
/// it. The state lives in this device's preferences; [trailing] stays a
/// separate target.
class FoldHeader extends StatelessWidget {
  final String id, title;
  final EdgeInsetsGeometry padding;

  /// The row's height, when it is a larger target than its text.
  final double? height;
  final Widget? trailing;
  const FoldHeader({
    super.key,
    required this.id,
    required this.title,
    required this.padding,
    this.height,
    this.trailing,
  });

  static bool folded(String id) => prefs.collapsedSections.contains(id);

  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    final folded = FoldHeader.folded(id);
    return Padding(
      padding: padding,
      child: Hover(
        builder: (context, hovered) => Row(
          children: [
            Expanded(
              child: Semantics(
                container: true,
                button: true,
                expanded: !folded,
                child: Clickable(
                  onTap: () => prefs.update((p) {
                    if (!p.collapsedSections.remove(id)) {
                      p.collapsedSections.add(id);
                    }
                  }),
                  child: SizedBox(
                    height: height,
                    child: Row(
                      children: [
                        Text(
                          title,
                          style: TextStyle(
                            fontSize: FontSizes.caption,
                            color: c.subtle,
                          ),
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
            ?trailing,
          ],
        ),
      ),
    );
  }
}

/// A foldable group inside a menu, like the sidebar sections.
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
  Widget build(BuildContext context) => Column(
    mainAxisSize: MainAxisSize.min,
    crossAxisAlignment: CrossAxisAlignment.stretch,
    children: [
      FoldHeader(
        id: id,
        title: title,
        padding: const EdgeInsets.fromLTRB(
          Space.inset,
          Space.gap,
          Space.inset,
          4,
        ),
      ),
      if (!FoldHeader.folded(id)) ...children,
    ],
  );
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

/// A pill that picks one of [options], showing the chosen one's label after
/// [icon]. [detail] adds a line under each option in the menu. The tooltip is
/// its accessible name.
class ChoicePill<T> extends StatelessWidget {
  final String tooltip;
  final T value;
  final List<(T, String)> options;
  final ValueChanged<T> onChanged;
  final IconData? icon;
  final Color? iconColor;
  final String Function(T value)? detail;
  const ChoicePill({
    super.key,
    required this.tooltip,
    required this.value,
    required this.options,
    required this.onChanged,
    this.icon,
    this.iconColor,
    this.detail,
  });
  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    final label =
        options.where((o) => o.$1 == value).firstOrNull?.$2 ?? '$value';
    return PopupMenuButton<T>(
      tooltip: tooltip,
      position: PopupMenuPosition.under,
      // Clear of the button and its highlight.
      offset: const Offset(0, 6),
      onSelected: onChanged,
      itemBuilder: (_) => [
        for (final (v, text) in options)
          HubMenuItem(
            value: v,
            height: detail == null ? 38 : 52,
            child: Row(
              children: [
                Expanded(
                  child: detail == null
                      ? Text(text)
                      : Column(
                          crossAxisAlignment: CrossAxisAlignment.start,
                          mainAxisSize: MainAxisSize.min,
                          children: [
                            Text(text),
                            Text(
                              detail!(v),
                              style: TextStyle(
                                fontSize: FontSizes.caption,
                                color: c.muted,
                              ),
                            ),
                          ],
                        ),
                ),
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
        padding: EdgeInsets.only(
          left: icon == null ? Space.inset : 10,
          right: Space.gap,
        ),
        decoration: BoxDecoration(
          borderRadius: BorderRadius.circular(17),
          border: Border.all(color: c.border),
        ),
        child: Row(
          mainAxisSize: MainAxisSize.min,
          children: [
            if (icon != null) ...[
              Icon(icon, size: 15, color: iconColor ?? c.muted),
              const SizedBox(width: 6),
            ],
            Text(
              label,
              style: TextStyle(fontSize: FontSizes.label, color: c.text),
            ),
            const SizedBox(width: 4),
            Icon(Icons.expand_more, size: 16, color: c.subtle),
          ],
        ),
      ),
    );
  }
}

/// A sidebar row: shaded on hover and when [selected], with its [menu]
/// revealed at the end, which a long press also opens.
class HoverRow<T> extends StatefulWidget {
  final bool selected;

  /// Space before the row, deeper for a row under another.
  final double indent;
  final VoidCallback onTap;
  final Widget child;
  final String menuTooltip;
  final PopupMenuItemBuilder<T> menu;
  final PopupMenuItemSelected<T> onSelected;

  /// Revealed after the menu.
  final Widget? trailing;
  const HoverRow({
    super.key,
    required this.selected,
    required this.onTap,
    required this.child,
    required this.menuTooltip,
    required this.menu,
    required this.onSelected,
    this.indent = 6,
    this.trailing,
  });
  @override
  State<HoverRow<T>> createState() => _HoverRowState<T>();
}

class _HoverRowState<T> extends State<HoverRow<T>> {
  final menuKey = GlobalKey<PopupMenuButtonState<T>>();
  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    final selected = widget.selected;
    return Hover(
      builder: (context, hovered) => Padding(
        padding: EdgeInsets.fromLTRB(widget.indent, 1, 6, 1),
        child: Container(
          height: 36,
          decoration: BoxDecoration(
            color: selected
                ? c.selected
                : hovered
                ? c.hover
                : Colors.transparent,
            borderRadius: BorderRadius.circular(Radii.control),
          ),
          child: Row(
            children: [
              Expanded(
                child: Semantics(
                  container: true,
                  button: true,
                  selected: selected,
                  child: InkWell(
                    borderRadius: BorderRadius.circular(Radii.control),
                    onTap: widget.onTap,
                    onLongPress: () => menuKey.currentState?.showButtonMenu(),
                    child: Padding(
                      padding: const EdgeInsets.symmetric(horizontal: 10),
                      child: widget.child,
                    ),
                  ),
                ),
              ),
              Reveal(
                visible: hovered || selected,
                child: SizedBox(
                  width: 32,
                  height: 32,
                  child: PopupMenuButton<T>(
                    key: menuKey,
                    tooltip: widget.menuTooltip,
                    padding: EdgeInsets.zero,
                    // The row already highlights; no second one on the glyph.
                    style: const ButtonStyle(
                      overlayColor: WidgetStatePropertyAll(Colors.transparent),
                    ),
                    icon: Icon(Icons.more_horiz, size: 18, color: c.muted),
                    position: PopupMenuPosition.under,
                    // Clear of the button and its highlight.
                    offset: const Offset(0, 6),
                    onSelected: widget.onSelected,
                    itemBuilder: widget.menu,
                  ),
                ),
              ),
              if (widget.trailing case final trailing?)
                Reveal(visible: hovered || selected, child: trailing),
              const SizedBox(width: 2),
            ],
          ),
        ),
      ),
    );
  }
}

/// The box around what waits on the member: an approval, a question, a
/// plugin's form.
class PendingCard extends StatelessWidget {
  final Widget child;
  const PendingCard({super.key, required this.child});
  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    return Container(
      padding: const EdgeInsets.all(Space.edge),
      decoration: BoxDecoration(
        color: c.composer,
        borderRadius: BorderRadius.circular(Radii.card),
        border: Border.all(color: c.border),
      ),
      child: child,
    );
  }
}

/// What [load] returns, built once it arrives, with a spinner meanwhile. A
/// reload keeps the last value on screen until the next arrives.
class Loader<T> extends StatefulWidget {
  final Future<T> Function() load;
  final Widget Function(BuildContext context, T value, VoidCallback reload)
  builder;

  /// What a failure shows; by default why, and a retry.
  final Widget Function(BuildContext context, Object error)? failed;
  const Loader({
    super.key,
    required this.load,
    required this.builder,
    this.failed,
  });
  @override
  State<Loader<T>> createState() => LoaderState<T>();
}

class LoaderState<T> extends State<Loader<T>> {
  late Future<T> future = widget.load();
  void reload() => setState(() {
    future = widget.load();
  });
  @override
  Widget build(BuildContext context) => FutureBuilder<T>(
    future: future,
    builder: (context, snap) {
      final c = HubColors.of(context);
      final done = snap.connectionState == ConnectionState.done;
      if (done && snap.hasError) {
        return widget.failed?.call(context, snap.error!) ??
            Padding(
              padding: const EdgeInsets.all(Space.edge),
              child: Column(
                mainAxisSize: MainAxisSize.min,
                children: [
                  SelectableText(
                    errorText(snap.error!),
                    textAlign: TextAlign.center,
                    style: TextStyle(color: c.danger),
                  ),
                  TextButton(onPressed: reload, child: Text(tr('重试', 'Retry'))),
                ],
              ),
            );
      }
      if (done || snap.hasData) {
        return widget.builder(context, snap.data as T, reload);
      }
      return Padding(
        padding: const EdgeInsets.all(24),
        child: Center(
          child: SizedBox(
            width: 18,
            height: 18,
            child: CircularProgressIndicator(strokeWidth: 2, color: c.subtle),
          ),
        ),
      );
    },
  );
}
