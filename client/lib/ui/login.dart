import 'dart:convert';

import 'package:flutter/material.dart';
import 'package:http/http.dart' as http;

import '../controller.dart';
import '../l10n.dart';
import '../prefs.dart';
import '../theme.dart';
import 'common.dart';

List<(String, String)> languageOptions() => [
  ('auto', tr('自动检测', 'Auto detect')),
  ('zh', '中文'),
  ('en', 'English'),
];

class LoginPage extends StatefulWidget {
  final Future<void> Function(String, String, String, bool) onLogin;

  /// Signs in with another account (OIDC, GitHub) by its provider id.
  final Future<void> Function(String server, String provider)? onProvider;

  /// Why an earlier sign-in with another account did not finish.
  final String error;
  const LoginPage({
    super.key,
    required this.onLogin,
    this.onProvider,
    this.error = '',
  });
  @override
  State<LoginPage> createState() => _LoginPageState();
}

class _LoginPageState extends State<LoginPage> {
  final server = TextEditingController(
    text: prefs.server.isEmpty ? defaultServerUrl : prefs.server,
  );
  final email = TextEditingController(), password = TextEditingController();
  bool register = true, busy = false;
  late String error = widget.error;

  /// The server's other ways to sign in: `{id, label, kind}`.
  List<Map<String, dynamic>> providers = [];
  String providersOf = '';

  @override
  void initState() {
    super.initState();
    loadProviders();
  }

  Future<void> loadProviders() async {
    final address = server.text.trim().replaceAll(RegExp(r'/+$'), '');
    if (address.isEmpty || address == providersOf) return;
    providersOf = address;
    try {
      final response = await http
          .get(Uri.parse('$address/capabilities'))
          .timeout(const Duration(seconds: 8));
      final data = jsonDecode(response.body);
      if (!mounted || providersOf != address) return;
      setState(
        () => providers = [
          for (final p in (data['auth_providers'] as List? ?? []))
            if (p is Map) Map<String, dynamic>.from(p),
        ],
      );
    } catch (_) {
      if (mounted && providersOf == address) setState(() => providers = []);
    }
  }

  Future<void> elsewhere(String provider) async {
    setState(() {
      busy = true;
      error = '';
    });
    try {
      await widget.onProvider!(server.text.trim(), provider);
    } catch (e) {
      if (mounted) setState(() => error = errorText(e));
    } finally {
      if (mounted) setState(() => busy = false);
    }
  }

  @override
  void dispose() {
    server.dispose();
    email.dispose();
    password.dispose();
    super.dispose();
  }

  Future<void> submit() async {
    setState(() {
      busy = true;
      error = '';
    });
    try {
      await widget.onLogin(server.text, email.text, password.text, register);
    } catch (e) {
      if (mounted) setState(() => error = errorText(e));
    } finally {
      if (mounted) setState(() => busy = false);
    }
  }

  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    return Scaffold(
      body: SafeArea(
        child: Stack(
          children: [
            Center(
              child: SingleChildScrollView(
                padding: const EdgeInsets.all(28),
                child: SizedBox(
                  width: 380,
                  child: Column(
                    crossAxisAlignment: CrossAxisAlignment.stretch,
                    children: [
                      Icon(Icons.hub_outlined, size: 40, color: c.text),
                      const SizedBox(height: 22),
                      Text(
                        tr('欢迎来到 RyzHub', 'Welcome to RyzHub'),
                        textAlign: TextAlign.center,
                        style: TextStyle(
                          fontSize: 28,
                          fontWeight: FontWeight.w600,
                          color: c.text,
                        ),
                      ),
                      const SizedBox(height: 34),
                      Focus(
                        onFocusChange: (focused) {
                          if (!focused) loadProviders();
                        },
                        child: TextField(
                          controller: server,
                          onSubmitted: (_) => loadProviders(),
                          decoration: InputDecoration(
                            labelText: tr('服务器地址', 'Server URL'),
                          ),
                        ),
                      ),
                      const SizedBox(height: 14),
                      TextField(
                        controller: email,
                        keyboardType: TextInputType.emailAddress,
                        decoration: InputDecoration(
                          labelText: tr('邮箱', 'Email'),
                        ),
                      ),
                      const SizedBox(height: 14),
                      TextField(
                        controller: password,
                        obscureText: true,
                        style: codeStyle,
                        decoration: InputDecoration(
                          labelText: tr(
                            '密码（至少 8 位）',
                            'Password (at least 8 characters)',
                          ),
                        ),
                        onSubmitted: (_) => busy ? null : submit(),
                      ),
                      const SizedBox(height: 20),
                      if (error.isNotEmpty)
                        Padding(
                          padding: const EdgeInsets.only(bottom: 16),
                          child: Text(error, style: TextStyle(color: c.danger)),
                        ),
                      FilledButton(
                        onPressed: busy ? null : submit,
                        child: Padding(
                          padding: const EdgeInsets.symmetric(vertical: 4),
                          child: Text(
                            busy
                                ? tr('连接中…', 'Connecting…')
                                : register
                                ? tr('创建个人空间', 'Create personal space')
                                : tr('登录', 'Log in'),
                          ),
                        ),
                      ),
                      if (providers.isNotEmpty &&
                          widget.onProvider != null) ...[
                        const SizedBox(height: 16),
                        Row(
                          children: [
                            const Expanded(child: Divider()),
                            Padding(
                              padding: const EdgeInsets.symmetric(
                                horizontal: 10,
                              ),
                              child: Text(
                                tr('或', 'or'),
                                style: TextStyle(color: c.muted),
                              ),
                            ),
                            const Expanded(child: Divider()),
                          ],
                        ),
                        const SizedBox(height: 12),
                        for (final provider in providers)
                          Padding(
                            padding: const EdgeInsets.only(bottom: Space.gap),
                            child: OutlinedButton.icon(
                              icon: Icon(
                                provider['kind'] == 'github'
                                    ? Icons.code
                                    : Icons.verified_user_outlined,
                                size: 18,
                              ),
                              onPressed: busy
                                  ? null
                                  : () => elsewhere('${provider['id']}'),
                              label: Text(
                                tr(
                                  '使用 ${provider['label']} 登录',
                                  'Continue with ${provider['label']}',
                                ),
                              ),
                            ),
                          ),
                      ],
                      const SizedBox(height: 6),
                      TextButton(
                        onPressed: busy
                            ? null
                            : () => setState(() => register = !register),
                        child: Text(
                          register
                              ? tr('已有账号？登录', 'Already have an account? Log in')
                              : tr('创建新账号', 'Create a new account'),
                        ),
                      ),
                    ],
                  ),
                ),
              ),
            ),
            Positioned(
              top: Space.gap,
              right: Space.gap,
              child: ChoicePill<String>(
                tooltip: tr('语言', 'Language'),
                icon: Icons.translate,
                value: prefs.language,
                options: languageOptions(),
                onChanged: (v) => prefs.update((p) => p.language = v),
              ),
            ),
          ],
        ),
      ),
    );
  }
}
