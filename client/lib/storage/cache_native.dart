import 'dart:io';

import 'package:path_provider/path_provider.dart';
import 'package:sqflite/sqflite.dart' as mobile;
import 'package:sqflite_common_ffi/sqflite_ffi.dart';

/// The client cache: records of one partition (server, user and organization), changed
/// together in one transaction. The record with the empty key is a cache from before records.
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
    const records =
        'CREATE TABLE cache_records(partition TEXT NOT NULL, key TEXT NOT NULL, '
        'value TEXT NOT NULL, PRIMARY KEY(partition, key))';
    final db = await factory.openDatabase(
      '${folder.path}/ryzhub.db',
      options: OpenDatabaseOptions(
        version: 2,
        onCreate: (db, _) async {
          await db.execute(
            'CREATE TABLE cache(partition TEXT PRIMARY KEY, state TEXT NOT NULL)',
          );
          await db.execute(records);
        },
        onUpgrade: (db, from, _) async {
          if (from < 2) await db.execute(records);
        },
      ),
    );
    return LocalCache._(db, partition);
  }

  Future<Map<String, String>> load() async {
    final records = <String, String>{};
    final legacy = await _db.query(
      'cache',
      where: 'partition = ?',
      whereArgs: [_partition],
    );
    if (legacy.isNotEmpty) records[''] = legacy.first['state'] as String;
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
          if (key.isEmpty) {
            // The cache from before records is only ever removed.
            await txn.delete(
              'cache',
              where: 'partition = ?',
              whereArgs: [_partition],
            );
          } else if (value == null) {
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
