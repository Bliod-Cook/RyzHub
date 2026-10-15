import 'package:idb_shim/idb_browser.dart';

/// Client preferences live apart from the `ryzhub-core` cache.
class PrefsStore {
  final Database _db;
  PrefsStore._(this._db);
  static Future<PrefsStore> open() async => PrefsStore._(
    await idbFactoryBrowser.open(
      'ryzhub-prefs',
      version: 1,
      onUpgradeNeeded: (event) => event.database.createObjectStore('prefs'),
    ),
  );

  Future<String> read() async {
    final tx = _db.transaction('prefs', idbModeReadOnly);
    final value = await tx.objectStore('prefs').getObject('client');
    await tx.completed;
    return value as String? ?? '';
  }

  Future<void> write(String json) async {
    final tx = _db.transaction('prefs', idbModeReadWrite);
    await tx.objectStore('prefs').put(json, 'client');
    await tx.completed;
  }
}
