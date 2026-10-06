import 'package:flutter/foundation.dart';
import 'package:flutter/material.dart';

final _uuid = RegExp(r'^[0-9a-fA-F-]{36}$');

/// The conversation an address names: `/c/<id>`, or '' for a new chat. The
/// web build shows it after the `#`.
String conversationIn(Uri uri) {
  final parts = uri.pathSegments;
  return parts.length == 2 && parts[0] == 'c' && _uuid.hasMatch(parts[1])
      ? parts[1]
      : '';
}

class ConversationLinkParser extends RouteInformationParser<String> {
  const ConversationLinkParser();

  @override
  Future<String> parseRouteInformation(RouteInformation routeInformation) =>
      SynchronousFuture(conversationIn(routeInformation.uri));

  @override
  RouteInformation restoreRouteInformation(String configuration) =>
      RouteInformation(
        uri: Uri(path: configuration.isEmpty ? '/' : '/c/$configuration'),
      );
}

/// Keeps the address in step with the open conversation, so a reload, a
/// bookmark or back and forward return to it. The page reports what it shows
/// through [show]; an address to open lands in [requested] until the page
/// takes it.
class ConversationLink extends RouterDelegate<String>
    with ChangeNotifier, PopNavigatorRouterDelegateMixin<String> {
  ConversationLink(this.page);

  /// The page that follows this link.
  final Widget Function(ConversationLink link) page;

  @override
  final navigatorKey = GlobalKey<NavigatorState>();

  /// What the page shows, and so the address.
  String shown = '';

  /// The conversation an address asks for, until the page opens it.
  final requested = ValueNotifier<String?>(null);

  @override
  String get currentConfiguration => shown;

  void show(String conversation) {
    if (conversation == shown) return;
    shown = conversation;
    notifyListeners();
  }

  /// The address named nothing the page could open: show what is open again.
  void refuse() => notifyListeners();

  @override
  Future<void> setNewRoutePath(String configuration) {
    requested.value = configuration;
    return SynchronousFuture(null);
  }

  // Dialogs and settings still push onto this navigator.
  @override
  Widget build(BuildContext context) => Navigator(
    key: navigatorKey,
    pages: [MaterialPage(key: const ValueKey('hub'), child: page(this))],
    onDidRemovePage: (_) {},
  );
}
