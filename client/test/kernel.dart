import 'dart:convert';
import 'dart:io';

import 'package:flutter/material.dart';
import 'package:flutter_rust_bridge/flutter_rust_bridge_for_generated.dart'
    show ExternalLibrary;
import 'package:flutter_test/flutter_test.dart';
import 'package:ryzhub/rust/api.dart';
import 'package:ryzhub/rust/frb_generated.dart';
import 'package:ryzhub/theme.dart';
import 'package:ryzhub/ui/messages.dart';

/// Loads the kernel built for this machine, so messages render as the app renders them:
/// `cargo build -p ryzhub-bridge --release` first (`scripts/test-client.sh` does).
Future<void> loadKernel() async {
  final name = Platform.isWindows
      ? 'ryzhub_bridge.dll'
      : Platform.isMacOS
      ? 'libryzhub_bridge.dylib'
      : 'libryzhub_bridge.so';
  for (final profile in ['release', 'debug']) {
    final file = File('${Directory.current.path}/../target/$profile/$name');
    if (file.existsSync()) {
      await RustLib.init(externalLibrary: ExternalLibrary.open(file.path));
      return;
    }
  }
  throw StateError('Build the kernel: cargo build -p ryzhub-bridge --release');
}

/// A theme with the app's colours, which widgets read from it.
final hubTheme = ThemeData(extensions: const [HubColors.light]);

/// The rows the kernel makes of [items], in sequence order: an item row is the item, a steps
/// row `{id, kind: steps, run_id, steps, latest}` holds its step items.
List<Map<String, dynamic>> timeline(Iterable<Map<String, dynamic>> items) {
  final numbered = [
    for (final (i, item) in items.indexed)
      {'first_seq': i + 1, 'revision': 1, ...item},
  ];
  final byId = {for (final item in numbered) item['id']: item};
  final snapshot = jsonDecode(
    timelineSnapshot(itemsJson: jsonEncode(numbered)),
  );
  return [
    for (final row in snapshot['rows'] as List)
      if (row['item'] != null)
        byId[row['item']]!
      else
        {
          'id': row['id'],
          'kind': 'steps',
          'run_id': row['run_id'],
          'steps': [for (final id in row['steps'] as List) byId[id]!],
          'latest': row['latest'] == true,
        },
  ];
}

/// The text of each of [items] as the kernel's timeline renders it, by item id.
Rendered? Function(String id) renderedIn(Iterable<Map<String, dynamic>> items) {
  final snapshot = jsonDecode(
    timelineSnapshot(
      itemsJson: jsonEncode([
        for (final item in items) {'revision': 1, ...item},
      ]),
    ),
  );
  final byId = {
    for (final change in snapshot['items'] as List)
      change['id'] as String: Rendered(
        change['blocks']?['blocks'] ?? const [],
        change['chips'] ?? const [],
      ),
  };
  return (id) => byId[id];
}

/// [item]'s text as the kernel's timeline renders it.
Rendered rendered(Map<String, dynamic> item) =>
    renderedIn([item])('${item['id'] ?? ''}') ?? const Rendered([], []);

/// A [MessageCard] for [item], its text rendered as the timeline renders it.
MessageCard messageCard({
  Key? key,
  required Map<String, dynamic> item,
  String agentLabel = 'Chat',
  VoidCallback? onFork,
  VoidCallback? onMemoryUndo,
  ValueChanged<String>? onSource,
  ValueChanged<String>? onFile,
  Widget Function(List<dynamic> files)? files,
}) => MessageCard(
  key: key,
  item: item,
  rendered: rendered(item),
  agentLabel: agentLabel,
  onFork: onFork,
  onMemoryUndo: onMemoryUndo,
  onSource: onSource,
  onFile: onFile,
  files: files,
);

/// A [StepsGroup] of [steps], their text and their subagents' rendered as the timeline
/// renders it.
StepsGroup stepsGroup({
  required List<Map<String, dynamic>> steps,
  bool running = false,
  bool latest = false,
  Duration? worked,
  List<Map<String, dynamic>> Function(String id)? children,
}) => StepsGroup(
  steps: steps,
  rendered: renderedIn([
    ...steps,
    for (final step in steps) ...?children?.call('${step['id']}'),
  ]),
  running: running,
  latest: latest,
  worked: worked,
  children: children,
);

/// The text messages show, a formula or chip as U+FFFC.
String shownText(WidgetTester tester) => tester
    .widgetList<RichText>(find.byType(RichText))
    .map((t) => t.text.toPlainText())
    .join('\n');
