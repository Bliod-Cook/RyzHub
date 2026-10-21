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

/// What each approval mode leaves to the member.
String approvalDescription(String value) => switch (value) {
  'ask' => tr(
    '执行命令、修改文件前都先询问',
    'Asks before running commands or changing files',
  ),
  'allow_edits' => tr(
    '修改工作区文件不再询问，执行命令仍会询问',
    'Edits files without asking; still asks before commands',
  ),
  'full_auto' => tr(
    '不再询问，Agent 在沙箱里自行执行',
    'Never asks; the agent acts on its own inside the sandbox',
  ),
  _ => '',
};

/// How the platform stands in for a capability the harness lacks
/// (`emulated` in the command catalog); null for the others.
String? emulatedNote(String capability) => switch (capability) {
  'steer' => tr(
    '这个 Agent 不能在回合中途接收指令：补充指令排到下一轮发送。',
    "This agent can't take instructions mid-turn: they're sent as the next turn.",
  ),
  'plan_mode' => tr(
    '由平台在提示中要求 Agent 先给出计划。',
    'The platform asks the agent for a plan in its prompt.',
  ),
  'fork' => tr(
    '由平台复制对话与工作区来分叉，Agent 从检查点重新开始。',
    'The platform copies the conversation and workspace; the agent restarts from the checkpoint.',
  ),
  'compact' => tr(
    '由平台用摘要替换较早的上下文。',
    'The platform replaces older context with a summary.',
  ),
  _ => null,
};

String reasoningLabel(String value) => switch (value) {
  'low' => tr('低', 'Low'),
  'medium' => tr('中', 'Medium'),
  'high' => tr('高', 'High'),
  'xhigh' => tr('超高', 'Extra high'),
  'max' => tr('最高', 'Max'),
  _ => value,
};

/// Names of the Work harnesses the server configures beyond Codex and Claude Code,
/// from its model catalog.
final Map<String, String> harnessLabels = {};

String pathLabel(String value) => switch (value) {
  'codex' => 'Codex',
  'claude' => 'Claude Code',
  _ => harnessLabels[value] ?? value,
};

String errorText(Object error) {
  var code = '$error';
  if (code.startsWith('Exception: ')) code = code.substring(11);
  // A code, then the server's explanation (a refused plugin package says why).
  final split = code.indexOf('\n');
  if (split > 0) {
    final detail = code.substring(split + 1);
    return '${errorText(Exception(code.substring(0, split)))}\n$detail';
  }
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
    'UNSUPPORTED_PROJECT_FILE' => tr(
      '只支持 UTF-8 文本、PDF 和 DOCX 文件。',
      'Only UTF-8 text, PDF and DOCX files can be added.',
    ),
    'DOCUMENT_UNREADABLE' => tr(
      '无法从这个文档中读出文字。',
      'No text could be read from this document.',
    ),
    'WORKSPACE_PURGED' => tr(
      '归档已超过保留期，工作区文件已删除；对话记录仍可查看。',
      'The archive outlived its retention period and the workspace files were deleted. The conversation is still readable.',
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
    'ATTACHMENT_TOO_LARGE' => tr('文件太大。', 'The file is too large.'),
    'ATTACHMENT_QUOTA_EXCEEDED' => tr(
      '文件库空间已满，请先删除一些文件。',
      'Your file library is full. Delete some files first.',
    ),
    'ATTACHMENT_UNAVAILABLE' => tr('文件不可用。', 'The file is not available.'),
    'ATTACHMENT_DELETED' => tr('文件已删除。', 'The file was deleted.'),
    'FILE_NOT_FOUND' => tr(
      '回答完成时沙箱里没有这个文件。',
      'The sandbox had no such file when the reply completed.',
    ),
    'NOT_A_FILE' => tr('链接指向的不是文件。', 'The link does not point to a file.'),
    'FILE_TOO_LARGE' => tr('文件太大，无法提供。', 'The file is too large to hand over.'),
    'INVALID_PATH' => tr('文件路径无效。', 'The file path is not valid.'),
    'FILE_UNAVAILABLE' => tr(
      '暂时无法提供这个文件。',
      'The file could not be handed over.',
    ),
    'TOO_MANY_ATTACHMENTS' => tr(
      '一条消息最多附带 10 个文件。',
      'A message carries at most 10 files.',
    ),
    'UPLOAD_BUSY' => tr(
      '上传的人太多了，请稍后重试。',
      'Too many uploads right now. Try again shortly.',
    ),
    'LOGIN_FAILED' => tr(
      '登录没有完成，请重试。',
      'Signing in did not finish. Try again.',
    ),
    'REGISTRATION_CLOSED' => tr(
      '此服务器不开放注册，请用已有账号登录。',
      'This server takes no new accounts. Log in with an existing one.',
    ),
    'ACCOUNT_EXISTS' => tr(
      '该邮箱已有账号。请用原方式登录后，在设置中连接此账号。',
      'This email has an account. Sign in as before, then connect this account in Settings.',
    ),
    'IDENTITY_TAKEN' => tr(
      '这个外部账号已连接到另一个 RyzHub 账号。',
      'This account is connected to another RyzHub account.',
    ),
    'IDENTITY_EMAIL_REQUIRED' => tr(
      '外部账号没有已验证的邮箱。',
      'The account has no verified email.',
    ),
    'LAST_SIGN_IN_METHOD' => tr(
      '这是你唯一的登录方式，不能移除。',
      'This is your only way to sign in and cannot be removed.',
    ),
    'UNSAFE_DESTINATION' => tr(
      '只能访问 HTTPS 的公网地址。',
      'Only public HTTPS addresses can be reached.',
    ),
    'PLUGIN_INVALID' => tr('插件包无效。', 'The plugin package is invalid.'),
    'PLUGIN_FETCH_FAILED' => tr(
      '无法获取插件，请检查地址。',
      'The plugin could not be fetched. Check the address.',
    ),
    'PLUGIN_TOO_LARGE' => tr('插件包太大。', 'The plugin package is too large.'),
    'PLUGIN_BLOCKED' => tr(
      '该插件已被管理员停用。',
      'An administrator has stopped this plugin.',
    ),
    'PLUGIN_EXISTS' => tr(
      '已安装同名插件，请使用更新。',
      'A plugin of that name is installed. Update it instead.',
    ),
    'PLUGIN_UNVERIFIED_DENIED' => tr(
      '组织策略不允许安装未验证的插件。',
      'Your organization does not allow unverified plugins.',
    ),
    'PLUGIN_SETTING_REQUIRED' => tr(
      '请填写插件的必填设置。',
      'Fill in the plugin\'s required settings.',
    ),
    'PLUGIN_OAUTH_FAILED' => tr(
      '账号连接失败。',
      'The account could not be connected.',
    ),
    'PLUGIN_AUTH_NOT_REQUIRED' => tr(
      '该服务不需要连接账号。',
      'This server does not need an account.',
    ),
    'PLUGIN_INPUT_REQUIRED' => tr(
      '该工具需要先回答它的问题。',
      'The tool needs answers to its questions first.',
    ),
    _ => code,
  };
}
