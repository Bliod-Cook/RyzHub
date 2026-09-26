import 'package:idb_shim/idb_browser.dart';

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

  Future<String> load() async {
    final tx = _db.transaction('cache', idbModeReadOnly);
    final value = await tx.objectStore('cache').getObject(_partition);
    await tx.completed;
    return value as String? ?? '';
  }

  Future<void> save(String state) async {
    final tx = _db.transaction('cache', idbModeReadWrite);
    await tx.objectStore('cache').put(state, _partition);
    await tx.completed;
  }

  Future<void> close() async => _db.close();
}
