import 'dart:convert';
import 'dart:io';

import 'package:flutter/widgets.dart';
import 'package:flutter_rust_bridge/flutter_rust_bridge_for_generated.dart'
    show ExternalLibrary;
import 'package:flutter_test/flutter_test.dart';
import 'package:ryzhub/rust/api.dart';
import 'package:ryzhub/rust/frb_generated.dart';

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

/// The text messages show, a formula or chip as U+FFFC.
String shownText(WidgetTester tester) => tester
    .widgetList<RichText>(find.byType(RichText))
    .map((t) => t.text.toPlainText())
    .join('\n');
