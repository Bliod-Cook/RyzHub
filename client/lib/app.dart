import 'package:flutter/material.dart';
import 'package:flutter_localizations/flutter_localizations.dart';

import 'link.dart';
import 'prefs.dart';
import 'theme.dart';
import 'ui/home.dart';

class RyzHubApp extends StatefulWidget {
  const RyzHubApp({super.key});
  @override
  State<RyzHubApp> createState() => _RyzHubAppState();
}

class _RyzHubAppState extends State<RyzHubApp> with WidgetsBindingObserver {
  String lang = prefs.lang;
  final link = ConversationLink((link) => HubPage(link: link));

  @override
  void initState() {
    super.initState();
    WidgetsBinding.instance.addObserver(this);
    prefs.addListener(_changed);
  }

  @override
  void dispose() {
    WidgetsBinding.instance.removeObserver(this);
    prefs.removeListener(_changed);
    link.dispose();
    super.dispose();
  }

  @override
  void didChangeLocales(List<Locale>? locales) =>
      prefs.localesChanged(locales ?? const []);

  void _changed() {
    setState(() {});
    if (lang == prefs.lang) return;
    lang = prefs.lang;
    // `tr()` is read during build, so every element (including const widgets
    // and routes below this one) rebuilds; keying MaterialApp would instead
    // drop open routes and the controller's state.
    void mark(Element e) {
      e.markNeedsBuild();
      e.visitChildren(mark);
    }

    WidgetsBinding.instance.rootElement?.visitChildren(mark);
  }

  @override
  Widget build(BuildContext context) {
    final accent = accentColor(prefs.accent);
    return MaterialApp.router(
      title: 'RyzHub',
      debugShowCheckedModeBanner: false,
      theme: buildTheme(Brightness.light, accent),
      darkTheme: buildTheme(Brightness.dark, accent),
      themeMode: prefs.themeMode,
      locale: prefs.lang == 'zh'
          ? const Locale('zh', 'CN')
          : const Locale('en'),
      supportedLocales: const [Locale('zh', 'CN'), Locale('en')],
      localizationsDelegates: GlobalMaterialLocalizations.delegates,
      routerDelegate: link,
      routeInformationParser: const ConversationLinkParser(),
    );
  }
}
