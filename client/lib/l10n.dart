import 'dart:async';
import 'dart:ui';

import 'prefs.dart';

String tr(String zh, String en) => prefs.lang == 'zh' ? zh : en;

/// Any Chinese locale uses Simplified Chinese (the bundled font is GB2312);
/// anything unrecognised falls back to English.
String detectLanguage(Iterable<Locale> locales) {
  for (final locale in locales) {
    if (locale.languageCode == 'zh') return 'zh';
    if (locale.languageCode == 'en') return 'en';
  }
  return 'en';
}

String connectionLabel(String code) => switch (code) {
  'online' => tr('已连接', 'Connected'),
  'connecting' => tr('连接中', 'Connecting'),
  'retrying' => tr('重连中', 'Reconnecting'),
  'background' => tr('后台', 'Paused'),
  _ => tr('未连接', 'Offline'),
};

String approvalLabel(String value) => switch (value) {
  'ask' => tr('每次询问', 'Ask every time'),
  'allow_edits' => tr('自动允许编辑', 'Auto-allow edits'),
  'full_auto' => tr('全部自动', 'Full auto'),
  _ => value,
};

String reasoningLabel(String value) => switch (value) {
  'low' => tr('低', 'Low'),
  'medium' => tr('中', 'Medium'),
  'high' => tr('高', 'High'),
  'xhigh' => tr('超高', 'Extra high'),
  'max' => tr('最高', 'Max'),
  _ => value,
};

String pathLabel(String value) => switch (value) {
  'codex' => 'Codex',
  'claude' => 'Claude Code',
  _ => value,
};

String errorText(Object error) {
  var code = '$error';
  if (code.startsWith('Exception: ')) code = code.substring(11);
  if (error is TimeoutException) {
    return tr(
      '请求超时，请检查网络后重试。',
      'The request timed out. Check your connection.',
    );
  }
  return switch (code) {
    'ACTION_ALREADY_RESOLVED' => tr(
      '该审批已由其他设备处理。',
      'This approval was already handled on another device.',
    ),
    'WORKSPACE_RECOVERY_REQUIRED' => tr(
      '工作区需要恢复，请先点击“恢复工作区”。',
      'The workspace needs recovery. Click "Restore workspace" first.',
    ),
    'WORKSPACE_BUSY' => tr(
      '工作区正忙，请稍后重试。',
      'The workspace is busy. Try again shortly.',
    ),
    'CONVERSATION_BUSY' || 'RUN_BUSY_USE_STEER' => tr(
      '对话正在执行，请稍后或补充指令。',
      'The conversation is running. Wait or add an instruction.',
    ),
    'POLICY_DENIED' => tr(
      '组织策略不允许此操作。',
      'Your organization policy does not allow this.',
    ),
    'MODEL_DENIED' ||
    'MODEL_NOT_ROUTABLE' => tr('该模型不可用。', 'This model is not available.'),
    'HARNESS_PROTOCOL_IMMUTABLE' => tr(
      '对话开始后只能切换同一提供方/执行器的模型，请新建对话。',
      'After a chat starts you can only switch to models from the same provider or harness. Start a new chat.',
    ),
    'INVALID_REASONING' => tr(
      '当前路径不支持该推理强度。',
      'This reasoning level is not supported here.',
    ),
    'INVALID_CREDENTIALS' => tr('邮箱或密码不正确。', 'Incorrect email or password.'),
    'CREDENTIAL_UNAVAILABLE' => tr(
      '模型凭据不可用。',
      'The model credential is unavailable.',
    ),
    'UNAUTHORIZED' => tr(
      '登录已失效，请重新登录。',
      'Your session expired. Sign in again.',
    ),
    'FORBIDDEN' => tr('没有权限执行此操作。', 'You do not have permission to do this.'),
    'RATE_LIMITED' => tr(
      '请求过于频繁，请稍后重试。',
      'Too many requests. Try again later.',
    ),
    'USE_WORKSPACE_ARCHIVE' => tr(
      'Work 对话请通过工作区归档。',
      'Archive Work conversations through the workspace.',
    ),
    'PROJECT_DELETED' => tr('项目已删除。', 'The project was deleted.'),
    'FORK_POINT_UNAVAILABLE' => tr('无法从此处分叉。', 'Cannot fork from here.'),
    _ => code,
  };
}
