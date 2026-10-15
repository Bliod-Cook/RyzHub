import 'dart:io';

import 'package:path_provider/path_provider.dart';
import 'package:sqflite/sqflite.dart' as mobile;
import 'package:sqflite_common_ffi/sqflite_ffi.dart';

/// The client cache: records of one partition (server, user and organization), changed
/// together in one transaction.
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
        onOpen: (db) => db.execute(
          'CREATE TABLE IF NOT EXISTS cache_records(partition TEXT NOT NULL, '
          'key TEXT NOT NULL, value TEXT NOT NULL, PRIMARY KEY(partition, key))',
        ),
      ),
    );
    return LocalCache._(db, partition);
  }

  Future<Map<String, String>> load() async {
    final records = <String, String>{};
    for (final row in await _db.query(
      'cache_records',
      where: 'partition = ?',
      whereArgs: [_partition],
    )) {
      records[row['key'] as String] = row['value'] as String;
    }
    return records;
  }

  /// Store and remove (a null value) records, all or none.
  Future<void> write(Map<String, String?> records) =>
      _db.transaction((txn) async {
        for (final MapEntry(:key, :value) in records.entries) {
          if (value == null) {
            await txn.delete(
              'cache_records',
              where: 'partition = ? AND key = ?',
              whereArgs: [_partition, key],
            );
          } else {
            await txn.insert('cache_records', {
              'partition': _partition,
              'key': key,
              'value': value,
            }, conflictAlgorithm: ConflictAlgorithm.replace);
          }
        }
      });

  Future<void> close() => _db.close();
}
