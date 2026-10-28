import 'package:flutter/material.dart';

import '../l10n.dart';
import '../theme.dart';
import 'common.dart';
import 'icons.dart';

/// A page of a [SectionedPage]: a row in its navigation, and the content it
/// opens. A section with [open] does that instead of opening content.
class PageSection {
  final String id, group;
  final IconData icon;
  final String Function() title;
  final List<String> keywords;

  /// How wide the content grows.
  final double width;
  final VoidCallback? open;
  const PageSection(
    this.id,
    this.group,
    this.icon,
    this.title,
    this.keywords, {
    this.width = 600,
    this.open,
  });
}

/// Sections listed by group beside the open one, with a search over their
/// titles and keywords. Narrow windows show the list and the section in turn.
class SectionedPage extends StatefulWidget {
  final List<PageSection> sections;

  /// Group ids in order, with their titles; a group titled '' goes unlabelled.
  final List<(String, String Function())> groups;

  /// What leaving the page says, e.g. "Back to app".
  final String back;

  /// The section open at first; otherwise [home] on wide windows.
  final String? initial;
  final String home;
  final Widget Function(BuildContext context, PageSection section) content;
  const SectionedPage({
    super.key,
    required this.sections,
    required this.groups,
    required this.back,
    required this.content,
    required this.home,
    this.initial,
  });
  @override
  State<SectionedPage> createState() => _SectionedPageState();
}

class _SectionedPageState extends State<SectionedPage> {
  late String? section = widget.initial ?? (_narrow ? null : widget.home);
  String query = '';
  bool navCollapsed = false;

  bool get _narrow {
    final view = WidgetsBinding.instance.platformDispatcher.views.first;
    return view.physicalSize.width / view.devicePixelRatio < wideWidth;
  }

  List<PageSection> get matches {
    final q = query.trim().toLowerCase();
    if (q.isEmpty) return widget.sections;
    return widget.sections
        .where(
          (s) =>
              s.title().toLowerCase().contains(q) ||
              s.keywords.any((k) => k.toLowerCase().contains(q)),
        )
        .toList();
  }

  Widget nav(BuildContext context, {required bool narrow}) {
    final c = HubColors.of(context);
    final list = matches;
    return Material(
      color: c.sidebar,
      child: SafeArea(
        right: false,
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.stretch,
          children: [
            Padding(
              padding: const EdgeInsets.fromLTRB(
                Space.gap,
                Space.gap,
                Space.gap,
                4,
              ),
              child: Row(
                children: [
                  if (!narrow)
                    IconButton(
                      tooltip: tr('收起侧边栏', 'Collapse sidebar'),
                      icon: const HubIcon(HubIcons.sidebarExpanded),
                      onPressed: () => setState(() => navCollapsed = true),
                    ),
                ],
              ),
            ),
            NavRow(
              icon: Icons.arrow_back,
              label: widget.back,
              onTap: () => Navigator.pop(context),
            ),
            Padding(
              padding: const EdgeInsets.fromLTRB(
                Space.gap,
                Space.gap,
                Space.gap,
                4,
              ),
              child: SizedBox(
                height: 38,
                child: TextField(
                  onChanged: (v) => setState(() => query = v),
                  style: const TextStyle(fontSize: FontSizes.body),
                  decoration: InputDecoration(
                    hintText: tr('搜索', 'Search'),
                    prefixIcon: const Icon(Icons.search, size: 18),
                    prefixIconConstraints: const BoxConstraints(minWidth: 36),
                    filled: true,
                    fillColor: c.hover,
                    contentPadding: const EdgeInsets.symmetric(
                      vertical: Space.gap,
                    ),
                    border: OutlineInputBorder(
                      borderRadius: BorderRadius.circular(Radii.control),
                      borderSide: BorderSide.none,
                    ),
                    enabledBorder: OutlineInputBorder(
                      borderRadius: BorderRadius.circular(Radii.control),
                      borderSide: BorderSide.none,
                    ),
                    focusedBorder: OutlineInputBorder(
                      borderRadius: BorderRadius.circular(Radii.control),
                      borderSide: BorderSide(color: c.border),
                    ),
                  ),
                ),
              ),
            ),
            Expanded(
              child: ListView(
                padding: const EdgeInsets.only(bottom: Space.edge),
                children: [
                  for (final (group, title) in widget.groups)
                    if (list.any((s) => s.group == group)) ...[
                      if (title().isNotEmpty) SectionLabel(title()),
                      for (final s in list.where((s) => s.group == group))
                        NavRow(
                          icon: s.icon,
                          label: s.title(),
                          selected: !narrow && section == s.id,
                          onTap: s.open ?? () => setState(() => section = s.id),
                        ),
                    ],
                  if (list.isEmpty)
                    EmptyHint(tr('没有匹配的设置', 'No matching settings')),
                ],
              ),
            ),
          ],
        ),
      ),
    );
  }

  Widget detail(BuildContext context, {required bool narrow}) {
    final c = HubColors.of(context);
    final s = widget.sections.firstWhere(
      (s) => s.id == section,
      orElse: () => widget.sections.first,
    );
    return SafeArea(
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          SizedBox(
            height: 52,
            child: Row(
              children: [
                const SizedBox(width: Space.gap),
                if (narrow)
                  IconButton(
                    tooltip: tr('返回', 'Back'),
                    icon: const Icon(Icons.arrow_back),
                    onPressed: () => setState(() => section = null),
                  )
                else if (navCollapsed)
                  IconButton(
                    tooltip: tr('打开侧边栏', 'Open sidebar'),
                    icon: const HubIcon(HubIcons.sidebarCollapsed),
                    onPressed: () => setState(() => navCollapsed = false),
                  ),
              ],
            ),
          ),
          Expanded(
            child: SingleChildScrollView(
              padding: const EdgeInsets.fromLTRB(20, 0, 20, 40),
              child: Center(
                child: ConstrainedBox(
                  constraints: BoxConstraints(maxWidth: s.width),
                  child: Column(
                    crossAxisAlignment: CrossAxisAlignment.stretch,
                    children: [
                      Semantics(
                        container: true,
                        header: true,
                        child: Text(
                          s.title(),
                          style: TextStyle(
                            fontSize: 24,
                            fontWeight: FontWeight.w500,
                            color: c.text,
                          ),
                        ),
                      ),
                      const SizedBox(height: 20),
                      KeyedSubtree(
                        key: ValueKey(s.id),
                        child: widget.content(context, s),
                      ),
                    ],
                  ),
                ),
              ),
            ),
          ),
        ],
      ),
    );
  }

  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    final narrow = MediaQuery.sizeOf(context).width < wideWidth;
    return Scaffold(
      body: narrow
          ? section == null
                ? nav(context, narrow: true)
                : detail(context, narrow: true)
          : Row(
              children: [
                if (!navCollapsed)
                  Container(
                    width: 260,
                    decoration: BoxDecoration(
                      border: Border(right: BorderSide(color: c.border)),
                    ),
                    child: nav(context, narrow: false),
                  ),
                Expanded(child: detail(context, narrow: false)),
              ],
            ),
    );
  }
}

class SettingsCard extends StatelessWidget {
  final List<Widget> children;
  final String? title;
  const SettingsCard({super.key, required this.children, this.title});
  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    return Padding(
      padding: const EdgeInsets.only(bottom: 20),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          if (title != null)
            Padding(
              padding: const EdgeInsets.fromLTRB(4, 0, 4, Space.gap),
              child: Text(
                title!,
                style: TextStyle(fontSize: FontSizes.label, color: c.subtle),
              ),
            ),
          Container(
            padding: const EdgeInsets.symmetric(horizontal: Space.edge),
            decoration: BoxDecoration(
              borderRadius: BorderRadius.circular(Radii.card),
              border: Border.all(color: c.border),
            ),
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.stretch,
              children: [
                for (final (i, child) in children.indexed) ...[
                  if (i > 0) Divider(color: c.border),
                  child,
                ],
              ],
            ),
          ),
        ],
      ),
    );
  }
}

class SettingRow extends StatelessWidget {
  final String title;
  final String? subtitle;
  final Widget? trailing;
  const SettingRow({
    super.key,
    required this.title,
    this.subtitle,
    this.trailing,
  });
  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    return Padding(
      padding: const EdgeInsets.symmetric(vertical: 12),
      child: Row(
        children: [
          Expanded(
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                Text(
                  title,
                  style: TextStyle(fontSize: FontSizes.body, color: c.text),
                ),
                if (subtitle != null) ...[
                  const SizedBox(height: 2),
                  Text(
                    subtitle!,
                    style: TextStyle(
                      fontSize: FontSizes.caption,
                      color: c.subtle,
                    ),
                  ),
                ],
              ],
            ),
          ),
          if (trailing != null) ...[const SizedBox(width: 16), trailing!],
        ],
      ),
    );
  }
}
