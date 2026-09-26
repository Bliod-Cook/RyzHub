import 'dart:io';

import 'package:path_provider/path_provider.dart';
import 'package:sqflite/sqflite.dart' as mobile;
import 'package:sqflite_common_ffi/sqflite_ffi.dart';

class LocalCache {
  final Database _db;
  final String _partition;
  LocalCache._(this._db, this._partition);
  static Future<LocalCache> open(String partition) async {
    final folder = await getApplicationSupportDirectory();
    final factory = Platform.isAndroid || Platform.isIOS
        ? mobile.databaseFactory
        : databaseFactoryFfi;
    if (!Platform.isAndroid && !Platform.isIOS) sqfliteFfiInit();
    final db = await factory.openDatabase(
      '${folder.path}/ryzhub.db',
      options: OpenDatabaseOptions(
        version: 1,
        onCreate: (db, _) => db.execute(
          'CREATE TABLE cache(partition TEXT PRIMARY KEY, state TEXT NOT NULL)',
        ),
      ),
    );
    return LocalCache._(db, partition);
  }

  Future<String> load() async {
    final rows = await _db.query(
      'cache',
      where: 'partition = ?',
      whereArgs: [_partition],
    );
    return rows.isEmpty ? '' : rows.first['state'] as String;
  }

  Future<void> save(String state) async {
    await _db.insert('cache', {
      'partition': _partition,
      'state': state,
    }, conflictAlgorithm: ConflictAlgorithm.replace);
  }

  Future<void> close() => _db.close();
}
