import 'dart:async';

import 'kernel.dart';

/// Every test renders with the kernel the app runs.
Future<void> testExecutable(FutureOr<void> Function() testMain) async {
  await loadKernel();
  await testMain();
}
