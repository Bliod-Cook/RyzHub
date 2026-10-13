import '../l10n.dart';
import 'attachments.dart' show fileTime;

/// When the workspace's last checkpoint was saved, e.g. "Checkpoint saved ·
/// 5 min ago"; a day or more back, its date and time.
String checkpointSaved(Object? createdAt, {DateTime? now}) {
  final at = DateTime.tryParse('$createdAt');
  if (at == null) return tr('已保存检查点', 'Checkpoint saved');
  final age = (now ?? DateTime.now()).difference(at);
  final when = age.inMinutes < 1
      ? tr('刚刚', 'just now')
      : age.inMinutes < 60
      ? tr('${age.inMinutes} 分钟前', '${age.inMinutes} min ago')
      : age.inHours < 24
      ? tr('${age.inHours} 小时前', '${age.inHours} h ago')
      : fileTime(createdAt, now: now);
  return tr('已保存检查点 · $when', 'Checkpoint saved · $when');
}

/// Where the workspace's files are, when that is news: archived (only in the
/// checkpoint store) or deleted after the archive retention.
String? storageNote(Object? state) => switch (state) {
  'archiving' => tr('正在归档工作区…', 'Archiving the workspace…'),
  'cold' => tr(
    '工作区已归档，只保存在检查点存储中；恢复后继续。',
    'The workspace is archived in checkpoint storage; restore it to continue.',
  ),
  'restoring' => tr('正在恢复工作区…', 'Restoring the workspace…'),
  'purging' || 'deleted' => tr(
    '归档超过保留期，工作区文件已删除；对话记录仍可查看。',
    'The archive outlived its retention; the workspace files were deleted. The conversation stays readable.',
  ),
  _ => null,
};
