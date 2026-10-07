import 'package:idb_shim/idb_browser.dart';

/// The client cache: records of one partition (server, user and organization), changed
/// together in one transaction. The record with the empty key is a cache from before records.
class LocalCache {
  final Database _db;
  final String _partition;
  LocalCache._(this._db, this._partition);
  static Future<LocalCache> open(String partition) async {
    final db = await idbFactoryBrowser.open(
      'ryzhub-core',
      version: 1,
      onUpgradeNeeded: (event) {
        event.database.createObjectStore('cache');
      },
    );
    return LocalCache._(db, partition);
  }

  /// A record's key in the store. The cache from before records sits under the partition.
  String _key(String key) => key.isEmpty ? _partition : '$_partition\u0000$key';

  Future<Map<String, String>> load() async {
    final tx = _db.transaction('cache', idbModeReadOnly);
    final store = tx.objectStore('cache');
    final records = <String, String>{};
    final legacy = await store.getObject(_partition);
    if (legacy is String) records[''] = legacy;
    final prefix = '$_partition\u0000';
    await store
        .openCursor(
          range: KeyRange.bound(prefix, '$_partition\u0001', false, true),
          autoAdvance: true,
        )
        .forEach((cursor) {
          final key = cursor.key as String;
          records[key.substring(prefix.length)] = cursor.value as String;
        });
    await tx.completed;
    return records;
  }

  /// Store and remove (a null value) records, all or none.
  Future<void> write(Map<String, String?> records) async {
    final tx = _db.transaction('cache', idbModeReadWrite);
    final store = tx.objectStore('cache');
    for (final MapEntry(:key, :value) in records.entries) {
      if (value == null) {
        await store.delete(_key(key));
      } else {
        await store.put(value, _key(key));
      }
    }
    await tx.completed;
  }

  Future<void> close() async => _db.close();
}
