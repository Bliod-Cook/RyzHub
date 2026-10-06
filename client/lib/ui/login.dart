import 'package:flutter/material.dart';

import '../controller.dart';
import '../l10n.dart';
import '../prefs.dart';
import '../theme.dart';
import 'common.dart';

class LanguageMenu extends StatelessWidget {
  const LanguageMenu({super.key});
  @override
  Widget build(BuildContext context) => PopupMenuButton<String>(
    tooltip: tr('语言', 'Language'),
    icon: const Icon(Icons.translate, size: 20),
    initialValue: prefs.language,
    onSelected: (v) => prefs.update((p) => p.language = v),
    itemBuilder: (_) => [
      for (final (v, label) in languageOptions())
        HubMenuItem(
          value: v,
          height: 38,
          child: Row(
            children: [
              Expanded(child: Text(label)),
              if (prefs.language == v) const Icon(Icons.check, size: 16),
            ],
          ),
        ),
    ],
  );
}

List<(String, String)> languageOptions() => [
  ('auto', tr('自动检测', 'Auto detect')),
  ('zh', '中文'),
  ('en', 'English'),
];

class LoginPage extends StatefulWidget {
  final Future<void> Function(String, String, String, bool) onLogin;
  const LoginPage({super.key, required this.onLogin});
  @override
  State<LoginPage> createState() => _LoginPageState();
}

class _LoginPageState extends State<LoginPage> {
  final server = TextEditingController(
    text: prefs.server.isEmpty ? defaultServerUrl : prefs.server,
  );
  final email = TextEditingController(), password = TextEditingController();
  bool register = true, busy = false;
  String error = '';
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
                      const SizedBox(height: 10),
                      Text(
                        tr(
                          '一个空间，连接你的想法与行动。',
                          'One space for your ideas and actions.',
                        ),
                        textAlign: TextAlign.center,
                        style: TextStyle(color: c.muted),
                      ),
                      const SizedBox(height: 34),
                      TextField(
                        controller: server,
                        decoration: InputDecoration(
                          labelText: tr('服务器地址', 'Server URL'),
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
                        style: obscuredStyle,
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
            const Positioned(top: 8, right: 8, child: LanguageMenu()),
          ],
        ),
      ),
    );
  }
}
