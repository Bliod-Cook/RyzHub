import 'dart:math';

import 'package:flutter/material.dart';

import '../controller.dart';
import '../l10n.dart';
import '../prefs.dart';
import '../theme.dart';
import 'attachments.dart';
import 'common.dart';
import 'dialogs.dart';
import 'icons.dart';

/// Files per page, as the server pages them.
const _page = 50;

/// The widest the page grows.
const _width = 1120.0;

/// The library's filters: a label, the kinds shown (every kind when null) and
/// whose files (uploads or models' outputs; both when null).
final _filters = <String, (String Function(), List<String>?, String?)>{
  'all': (() => tr('全部', 'All'), null, null),
  'images': (() => tr('图片', 'Images'), ['image'], null),
  'documents': (() => tr('文档', 'Documents'), ['pdf', 'text'], null),
  'other': (() => tr('其他', 'Other'), ['file'], null),
  'generated': (() => tr('AI 生成', 'AI-made'), null, 'output'),
};

/// The member's file library in place of a conversation: browse, search,
/// upload and delete.
class LibraryView extends StatefulWidget {
  final HubController hub;

  /// Start a new chat with the file on its first message.
  final ValueChanged<Map<String, dynamic>> onUse;
  const LibraryView({super.key, required this.hub, required this.onUse});
  @override
  State<LibraryView> createState() => LibraryViewState();
}

class LibraryViewState extends State<LibraryView> {
  final scroll = ScrollController();
  List<Map<String, dynamic>> files = [];
  bool busy = true, more = false;

  /// The page being loaded follows the files shown, rather than replacing
  /// them.
  bool paging = false;
  String error = '', query = '', filter = 'all';
  int uploading = 0, _loads = 0;

  HubController get hub => widget.hub;

  @override
  void initState() {
    super.initState();
    scroll.addListener(_nearEnd);
    load();
  }

  @override
  void dispose() {
    scroll.dispose();
    super.dispose();
  }

  void _set(VoidCallback change) {
    if (mounted) setState(change);
  }

  void _nearEnd() {
    if (more && !busy && scroll.position.extentAfter < 600) load(next: true);
  }

  /// The first page for the current search and filter, or with [next] the
  /// page after the files shown. A newer load wins over an older one.
  Future<void> load({bool next = false}) async {
    final load = ++_loads;
    _set(() {
      busy = true;
      paging = next;
    });
    try {
      final page = await hub.library(
        before: next && files.isNotEmpty ? files.last['id'] as String : null,
        query: query,
        kinds: _filters[filter]!.$2,
        direction: _filters[filter]!.$3,
      );
      if (load != _loads) return;
      _set(() {
        files = next ? [...files, ...page] : page;
        more = page.length >= _page;
        error = '';
        busy = false;
      });
      // A page that does not fill the screen cannot be scrolled to its end.
      WidgetsBinding.instance.addPostFrameCallback((_) {
        if (mounted && scroll.hasClients) _nearEnd();
      });
    } catch (e) {
      if (load != _loads) return;
      _set(() {
        error = errorText(e);
        busy = false;
      });
    }
  }

  bool _shows(Map<String, dynamic> file) {
    final (_, kinds, direction) = _filters[filter]!;
    final words = query.trim().toLowerCase();
    return (kinds == null || kinds.contains(file['kind'])) &&
        (direction == null || file['direction'] == direction) &&
        '${file['name']}'.toLowerCase().contains(words);
  }

  /// Upload files of this device into the library, one after another.
  Future<void> upload(List<LocalFile> chosen) async {
    final messenger = ScaffoldMessenger.of(context);
    void refuse(String name, Object error) => messenger.showSnackBar(
      SnackBar(content: Text('$name: ${errorText(error)}')),
    );
    final limit = hub.maxFileBytes;
    for (final file in chosen) {
      if (limit != null && file.size > limit) {
        refuse(file.name, 'ATTACHMENT_TOO_LARGE');
        continue;
      }
      _set(() => uploading++);
      try {
        final added = await hub.uploadFile(file.name, file.size, file.open);
        if (_shows(added)) _set(() => files.insert(0, added));
      } catch (e) {
        refuse(file.name, e);
      } finally {
        _set(() => uploading--);
      }
    }
  }

  Future<void> pick() async => upload(await chooseLocalFiles());

  Future<void> remove(Map<String, dynamic> file) async {
    final used = (file['conversations'] as num?) ?? 0;
    final ok = await confirmDialog(
      context,
      title: tr('删除文件？', 'Delete file?'),
      message: used > 0
          ? tr(
              '“${file['name']}” 会从引用它的 $used 个对话中移除，模型和沙箱都不会再看到它。',
              '"${file['name']}" will be removed from the $used conversations that use it; models and sandboxes will no longer see it.',
            )
          : tr(
              '“${file['name']}” 将被永久删除。',
              '"${file['name']}" will be deleted permanently.',
            ),
      confirm: tr('删除', 'Delete'),
      danger: true,
    );
    if (!ok || !mounted) return;
    final messenger = ScaffoldMessenger.of(context);
    try {
      await hub.deleteFile(file['id'] as String);
      _set(() => files.remove(file));
    } catch (e) {
      messenger.showSnackBar(SnackBar(content: Text(errorText(e))));
    }
  }

  /// Any file opens in the preview, which steps through the files shown.
  void open(Map<String, dynamic> file) =>
      hub.openPreview(files, index: files.indexOf(file));

  void act(Map<String, dynamic> file, String action) {
    switch (action) {
      case 'use':
        widget.onUse(file);
      case 'download':
        downloadFile(
          context,
          hub,
          file['id'] as String,
          file['name'] as String,
        );
      case 'delete':
        remove(file);
    }
  }

  void choose(String filter) {
    if (filter == this.filter) return;
    this.filter = filter;
    load();
  }

  Widget uploadButton({bool compact = false}) {
    final label = uploading > 0
        ? tr('上传中（$uploading）', 'Uploading ($uploading)')
        : tr('上传', 'Upload');
    return FilledButton.icon(
      style: FilledButton.styleFrom(
        padding: EdgeInsets.symmetric(horizontal: compact ? 14 : 18),
        // As tall as the search box beside it, at any density.
        visualDensity: VisualDensity.standard,
        minimumSize: const Size(0, 40),
      ),
      onPressed: pick,
      icon: uploading > 0
          ? SizedBox(
              width: 16,
              height: 16,
              child: CircularProgressIndicator(
                strokeWidth: 2,
                color: HubColors.of(context).background,
              ),
            )
          : const Icon(Icons.add, size: 18),
      label: Text(label),
    );
  }

  Widget header(bool narrow) {
    final c = HubColors.of(context);
    final title = Semantics(
      container: true,
      header: true,
      child: Text(
        tr('文件库', 'Library'),
        style: TextStyle(
          fontSize: narrow ? 24 : 28,
          fontWeight: FontWeight.w500,
          color: c.text,
        ),
      ),
    );
    final search = FileSearchField(
      onSearch: (query) {
        if (query.trim() == this.query.trim()) return;
        this.query = query;
        load();
      },
    );
    final toggle = _ViewToggle(
      list: prefs.libraryList,
      onChanged: (list) =>
          setState(() => prefs.update((p) => p.libraryList = list)),
    );
    final filters = SingleChildScrollView(
      scrollDirection: Axis.horizontal,
      child: Row(
        children: [
          for (final MapEntry(key: id, value: (label, _, _))
              in _filters.entries)
            Padding(
              padding: const EdgeInsets.only(right: 4),
              child: _FilterTab(
                label: label(),
                selected: filter == id,
                onTap: () => choose(id),
              ),
            ),
        ],
      ),
    );
    if (narrow) {
      return Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          Row(
            children: [
              Expanded(child: title),
              uploadButton(compact: true),
            ],
          ),
          const SizedBox(height: 12),
          Row(
            children: [
              Expanded(child: search),
              const SizedBox(width: 8),
              toggle,
            ],
          ),
          const SizedBox(height: 12),
          filters,
        ],
      );
    }
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        Row(
          children: [
            Expanded(child: title),
            toggle,
            const SizedBox(width: 12),
            SizedBox(width: 280, child: search),
            const SizedBox(width: 12),
            uploadButton(),
          ],
        ),
        const SizedBox(height: 20),
        filters,
      ],
    );
  }

  Widget empty() {
    final c = HubColors.of(context);
    final narrowed = query.trim().isNotEmpty || filter != 'all';
    return Center(
      child: Padding(
        padding: const EdgeInsets.all(24),
        child: Column(
          mainAxisSize: MainAxisSize.min,
          children: [
            HubIcon(HubIcons.library, size: 40, color: c.subtle),
            const SizedBox(height: 16),
            Text(
              narrowed
                  ? tr('没有匹配的文件', 'No matching files')
                  : tr('还没有文件', 'No files yet'),
              style: TextStyle(
                fontSize: 16,
                fontWeight: FontWeight.w500,
                color: c.text,
              ),
            ),
            if (!narrowed) ...[
              const SizedBox(height: 6),
              ConstrainedBox(
                constraints: const BoxConstraints(maxWidth: 420),
                child: Text(
                  tr(
                    '上传的文件会保存在这里，可以在任意对话中再次使用。删除文件会把它从所有对话中移除；删除对话不会删除文件。',
                    'Files you upload stay here and can be used in any conversation. Deleting a file removes it from every conversation; deleting a conversation keeps its files.',
                  ),
                  textAlign: TextAlign.center,
                  style: TextStyle(fontSize: 13, color: c.subtle),
                ),
              ),
            ],
          ],
        ),
      ),
    );
  }

  Widget body(double side) {
    final c = HubColors.of(context);
    if (files.isEmpty) {
      if (busy) return const Center(child: CircularProgressIndicator());
      if (error.isNotEmpty) {
        return Center(
          child: Column(
            mainAxisSize: MainAxisSize.min,
            children: [
              Text(error, style: TextStyle(color: c.danger)),
              const SizedBox(height: 8),
              TextButton(onPressed: load, child: Text(tr('重试', 'Retry'))),
            ],
          ),
        );
      }
      return empty();
    }
    final list = prefs.libraryList;
    return CustomScrollView(
      controller: scroll,
      slivers: [
        SliverPadding(
          padding: EdgeInsets.fromLTRB(side, list ? 8 : 16, side, 16),
          sliver: list
              ? SliverList.builder(
                  itemCount: files.length,
                  itemBuilder: (context, i) => _FileRow(
                    key: ValueKey(files[i]['id']),
                    hub: hub,
                    file: files[i],
                    onOpen: () => open(files[i]),
                    onAction: (action) => act(files[i], action),
                  ),
                )
              : SliverGrid.builder(
                  gridDelegate: const SliverGridDelegateWithMaxCrossAxisExtent(
                    maxCrossAxisExtent: 260,
                    mainAxisSpacing: 16,
                    crossAxisSpacing: 16,
                  ),
                  itemCount: files.length,
                  itemBuilder: (context, i) => _FileCard(
                    key: ValueKey(files[i]['id']),
                    hub: hub,
                    file: files[i],
                    onOpen: () => open(files[i]),
                    onAction: (action) => act(files[i], action),
                  ),
                ),
        ),
        SliverToBoxAdapter(
          child: Padding(
            padding: const EdgeInsets.only(bottom: 24),
            child: Center(
              child: busy && paging
                  ? SizedBox(
                      width: 18,
                      height: 18,
                      child: CircularProgressIndicator(
                        strokeWidth: 2,
                        color: c.subtle,
                      ),
                    )
                  : error.isNotEmpty
                  ? TextButton(
                      onPressed: () => load(next: true),
                      child: Text(tr('重新加载', 'Retry')),
                    )
                  : const SizedBox.shrink(),
            ),
          ),
        ),
      ],
    );
  }

  @override
  Widget build(BuildContext context) {
    return LayoutBuilder(
      builder: (context, box) {
        final narrow = box.maxWidth < 640;
        final side = max(narrow ? 16.0 : 32.0, (box.maxWidth - _width) / 2);
        return Column(
          crossAxisAlignment: CrossAxisAlignment.stretch,
          children: [
            Padding(
              padding: EdgeInsets.fromLTRB(side, 4, side, 4),
              child: header(narrow),
            ),
            // A refresh keeps the files on screen under a thin bar.
            SizedBox(
              height: 2,
              child: busy && !paging && files.isNotEmpty
                  ? const LinearProgressIndicator(minHeight: 2)
                  : null,
            ),
            Expanded(child: body(side)),
          ],
        );
      },
    );
  }
}

/// Grid or rows.
class _ViewToggle extends StatelessWidget {
  final bool list;
  final ValueChanged<bool> onChanged;
  const _ViewToggle({required this.list, required this.onChanged});
  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    Widget button(bool rows, IconData icon, String label) => Semantics(
      selected: list == rows,
      child: IconButton(
        tooltip: label,
        isSelected: list == rows,
        style: IconButton.styleFrom(
          backgroundColor: list == rows ? c.selected : Colors.transparent,
          minimumSize: const Size(36, 36),
          fixedSize: const Size(36, 36),
          padding: EdgeInsets.zero,
        ),
        icon: Icon(icon, size: 20, color: list == rows ? c.text : c.muted),
        onPressed: () => onChanged(rows),
      ),
    );
    return Row(
      mainAxisSize: MainAxisSize.min,
      children: [
        button(false, Icons.grid_view_outlined, tr('网格视图', 'Grid view')),
        const SizedBox(width: 2),
        button(true, Icons.view_list_outlined, tr('列表视图', 'List view')),
      ],
    );
  }
}

class _FilterTab extends StatelessWidget {
  final String label;
  final bool selected;
  final VoidCallback onTap;
  const _FilterTab({
    required this.label,
    required this.selected,
    required this.onTap,
  });
  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    return Semantics(
      container: true,
      button: true,
      selected: selected,
      child: Material(
        color: selected ? c.selected : Colors.transparent,
        shape: const StadiumBorder(),
        child: InkWell(
          customBorder: const StadiumBorder(),
          hoverColor: c.hover,
          onTap: onTap,
          child: Padding(
            padding: const EdgeInsets.symmetric(horizontal: 16, vertical: 8),
            child: Text(
              label,
              style: TextStyle(
                fontSize: 14,
                color: selected ? c.text : c.muted,
              ),
            ),
          ),
        ),
      ),
    );
  }
}

/// A file's commands: use it in a new chat, download it, delete it.
class _FileMenu extends StatelessWidget {
  final ValueChanged<String> onAction;
  final bool raised;
  const _FileMenu({required this.onAction, this.raised = false});
  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    HubMenuItem<String> item(
      String value,
      IconData icon,
      String label, {
      Color? color,
    }) => HubMenuItem(
      value: value,
      height: 38,
      child: Row(
        children: [
          Icon(icon, size: 17, color: color),
          const SizedBox(width: 12),
          Text(label, style: TextStyle(color: color)),
        ],
      ),
    );
    final button = SizedBox(
      width: 32,
      height: 32,
      child: PopupMenuButton<String>(
        tooltip: tr('文件选项', 'File options'),
        padding: EdgeInsets.zero,
        icon: Icon(Icons.more_horiz, size: 18, color: c.muted),
        position: PopupMenuPosition.under,
        offset: const Offset(0, 6),
        onSelected: onAction,
        itemBuilder: (_) => [
          item(
            'use',
            Icons.add_comment_outlined,
            tr('在新对话中使用', 'Use in new chat'),
          ),
          item('download', Icons.download_outlined, tr('下载', 'Download')),
          item(
            'delete',
            Icons.delete_outline,
            tr('删除', 'Delete'),
            color: c.danger,
          ),
        ],
      ),
    );
    if (!raised) return button;
    // Over a picture: a disc keeps the glyph legible.
    return Material(
      color: c.background.withValues(alpha: .9),
      shape: const CircleBorder(),
      child: button,
    );
  }
}

/// A file in the grid: a picture fills its card; any other file shows its
/// name, the icon of its kind and when it was uploaded.
class _FileCard extends StatelessWidget {
  final HubController hub;
  final Map<String, dynamic> file;
  final VoidCallback onOpen;
  final ValueChanged<String> onAction;
  const _FileCard({
    super.key,
    required this.hub,
    required this.file,
    required this.onOpen,
    required this.onAction,
  });
  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    final name = file['name'] as String;
    final kind = file['kind'] as String?;
    final image = kind == 'image';
    return Hover(
      builder: (context, hovered) => Material(
        color: image ? c.hover : c.background,
        clipBehavior: Clip.antiAlias,
        shape: RoundedRectangleBorder(
          borderRadius: BorderRadius.circular(16),
          side: BorderSide(color: c.border),
        ),
        child: Stack(
          children: [
            Positioned.fill(
              child: Semantics(
                container: true,
                button: true,
                // A picture is named here; a card shows its name.
                label: image ? name : null,
                excludeSemantics: image,
                child: InkWell(
                  onTap: onOpen,
                  child: image
                      ? LayoutBuilder(
                          builder: (context, box) => FileThumbnail(
                            hub: hub,
                            name: name,
                            id: file['id'] as String,
                            kind: kind,
                            size: box.maxWidth,
                          ),
                        )
                      : Padding(
                          padding: const EdgeInsets.fromLTRB(18, 16, 18, 16),
                          child: Column(
                            crossAxisAlignment: CrossAxisAlignment.start,
                            children: [
                              Padding(
                                // Clear of the menu in the corner.
                                padding: const EdgeInsets.only(right: 20),
                                child: Text(
                                  name,
                                  maxLines: 2,
                                  overflow: TextOverflow.ellipsis,
                                  style: TextStyle(
                                    fontSize: 15,
                                    height: 1.35,
                                    color: c.text,
                                  ),
                                ),
                              ),
                              Expanded(
                                child: Center(
                                  child: Icon(
                                    fileIcon(kind, name),
                                    size: 36,
                                    color: c.text,
                                  ),
                                ),
                              ),
                              Text(
                                uploadedOn(
                                  file['created_at'],
                                  generated: file['direction'] == 'output',
                                ),
                                maxLines: 1,
                                overflow: TextOverflow.ellipsis,
                                style: TextStyle(
                                  fontSize: 12.5,
                                  color: c.subtle,
                                ),
                              ),
                            ],
                          ),
                        ),
                ),
              ),
            ),
            // A picture's name shows on hover, over its lower edge. It stays
            // in the tree so the menu after it keeps its state, and with it
            // an open menu's selection, when the pointer leaves for the menu.
            if (image)
              Positioned(
                left: 0,
                right: 0,
                bottom: 0,
                child: IgnorePointer(
                  child: AnimatedOpacity(
                    opacity: hovered || touchPlatform ? 1 : 0,
                    duration: const Duration(milliseconds: 120),
                    child: Container(
                      padding: const EdgeInsets.fromLTRB(12, 24, 12, 10),
                      decoration: BoxDecoration(
                        gradient: LinearGradient(
                          begin: Alignment.topCenter,
                          end: Alignment.bottomCenter,
                          colors: [
                            Colors.black.withValues(alpha: 0),
                            Colors.black.withValues(alpha: .55),
                          ],
                        ),
                      ),
                      child: Text(
                        name,
                        maxLines: 1,
                        overflow: TextOverflow.ellipsis,
                        style: const TextStyle(
                          fontSize: 13,
                          color: Colors.white,
                        ),
                      ),
                    ),
                  ),
                ),
              ),
            Positioned(
              top: 8,
              right: 8,
              child: Reveal(
                visible: hovered,
                child: _FileMenu(onAction: onAction, raised: image),
              ),
            ),
          ],
        ),
      ),
    );
  }
}

/// A file as a row: its picture or icon, name, size, upload date and use.
class _FileRow extends StatelessWidget {
  final HubController hub;
  final Map<String, dynamic> file;
  final VoidCallback onOpen;
  final ValueChanged<String> onAction;
  const _FileRow({
    super.key,
    required this.hub,
    required this.file,
    required this.onOpen,
    required this.onAction,
  });
  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    final name = file['name'] as String;
    final used = (file['conversations'] as num?) ?? 0;
    final details = [
      fileSize(file['size'] as num),
      uploadedOn(file['created_at'], generated: file['direction'] == 'output'),
      if (used > 0) tr('用于 $used 个对话', 'In $used conversations'),
    ].where((part) => part.isNotEmpty).join(' · ');
    return Hover(
      builder: (context, hovered) => Material(
        color: hovered ? c.hover : Colors.transparent,
        borderRadius: BorderRadius.circular(12),
        child: Row(
          children: [
            Expanded(
              child: Semantics(
                container: true,
                button: true,
                child: InkWell(
                  borderRadius: BorderRadius.circular(12),
                  onTap: onOpen,
                  child: Padding(
                    padding: const EdgeInsets.symmetric(
                      horizontal: 10,
                      vertical: 9,
                    ),
                    child: Row(
                      children: [
                        FileThumbnail(
                          hub: hub,
                          name: name,
                          id: file['id'] as String,
                          kind: file['kind'] as String?,
                          size: 40,
                        ),
                        const SizedBox(width: 12),
                        Expanded(
                          child: Column(
                            crossAxisAlignment: CrossAxisAlignment.start,
                            children: [
                              Text(
                                name,
                                maxLines: 1,
                                overflow: TextOverflow.ellipsis,
                                style: TextStyle(fontSize: 14, color: c.text),
                              ),
                              const SizedBox(height: 2),
                              Text(
                                details,
                                maxLines: 1,
                                overflow: TextOverflow.ellipsis,
                                style: TextStyle(
                                  fontSize: 12.5,
                                  color: c.subtle,
                                ),
                              ),
                            ],
                          ),
                        ),
                      ],
                    ),
                  ),
                ),
              ),
            ),
            Reveal(
              visible: hovered,
              child: _FileMenu(onAction: onAction),
            ),
            const SizedBox(width: 8),
          ],
        ),
      ),
    );
  }
}
