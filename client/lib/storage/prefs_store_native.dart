import 'dart:io';

import 'package:path_provider/path_provider.dart';

/// A plain file: the sqflite handle is shared per path and closed on logout.
class PrefsStore {
  final File _file;
  PrefsStore._(this._file);
  static Future<PrefsStore> open() async => PrefsStore._(
    File('${(await getApplicationSupportDirectory()).path}/prefs.json'),
  );

  Future<String> read() async =>
      await _file.exists() ? await _file.readAsString() : '';

  Future<void> write(String json) async {
    await _file.parent.create(recursive: true);
    await _file.writeAsString(json, flush: true);
  }
}
