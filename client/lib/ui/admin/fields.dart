import 'package:flutter/material.dart';

import '../../l10n.dart';
import '../../theme.dart';
import '../common.dart';

/// A unit a number is shown in: [factor] of the stored unit.
class Scale {
  final String id;
  final int factor;
  final String Function() label;
  Scale(this.id, this.factor, this.label);
}

final byteScales = [
  Scale('MiB', 1 << 20, () => 'MiB'),
  Scale('GiB', 1 << 30, () => 'GiB'),
];
final secondScales = [
  Scale('s', 1, () => tr('秒', 's')),
  Scale('min', 60, () => tr('分', 'min')),
  Scale('h', 3600, () => tr('时', 'h')),
  Scale('d', 86400, () => tr('天', 'd')),
];
final dayScales = [Scale('d', 1, () => tr('天', 'days'))];
final plainScales = [Scale('', 1, () => '')];

/// [value] in the largest of [scales] it is a whole number of; otherwise in
/// the smallest, with a fraction.
(Scale, num) fit(List<Scale> scales, int value) {
  for (final s in scales.reversed) {
    if (value != 0 && value % s.factor == 0) return (s, value ~/ s.factor);
  }
  final s = scales.first;
  return (s, value % s.factor == 0 ? value ~/ s.factor : value / s.factor);
}

/// [number] of [scale], in the stored unit; null unless a number that is not
/// negative.
int? unscale(Scale scale, String number) {
  final n = num.tryParse(number.trim());
  if (n == null || n < 0) return null;
  return (n * scale.factor).round();
}

/// A number typed in one of [scales].
class NumberField extends StatefulWidget {
  final int value;
  final List<Scale> scales;
  final ValueChanged<int>? onChanged;
  const NumberField({
    super.key,
    required this.value,
    required this.scales,
    this.onChanged,
  });
  @override
  State<NumberField> createState() => _NumberFieldState();
}

class _NumberFieldState extends State<NumberField> {
  late Scale scale;
  late final TextEditingController text;
  bool invalid = false;

  @override
  void initState() {
    super.initState();
    final (s, n) = fit(widget.scales, widget.value);
    scale = s;
    text = TextEditingController(text: '$n');
  }

  @override
  void dispose() {
    text.dispose();
    super.dispose();
  }

  void emit() {
    final value = unscale(scale, text.text);
    setState(() => invalid = value == null);
    if (value != null) widget.onChanged?.call(value);
  }

  @override
  Widget build(BuildContext context) => Row(
    mainAxisSize: MainAxisSize.min,
    children: [
      SizedBox(
        width: 96,
        child: TextField(
          controller: text,
          enabled: widget.onChanged != null,
          keyboardType: const TextInputType.numberWithOptions(decimal: true),
          textAlign: TextAlign.end,
          decoration: InputDecoration(
            isDense: true,
            errorText: invalid ? '' : null,
          ),
          onChanged: (_) => emit(),
        ),
      ),
      if (widget.scales.length > 1) ...[
        const SizedBox(width: Space.gap),
        widget.onChanged == null
            ? Text(scale.label())
            : ChoicePill<String>(
                tooltip: tr('单位', 'Unit'),
                value: scale.id,
                options: [for (final s in widget.scales) (s.id, s.label())],
                onChanged: (id) {
                  scale = widget.scales.firstWhere((s) => s.id == id);
                  emit();
                },
              ),
      ] else if (widget.scales.first.id.isNotEmpty) ...[
        const SizedBox(width: Space.gap),
        Text(widget.scales.first.label()),
      ],
    ],
  );
}

/// A limit that may be lifted: null is no limit. [initial] is the limit set
/// when one is added.
class LimitField extends StatelessWidget {
  final int? value;
  final List<Scale> scales;
  final int initial;
  final ValueChanged<int?>? onChanged;
  const LimitField({
    super.key,
    required this.value,
    required this.scales,
    required this.initial,
    this.onChanged,
  });
  @override
  Widget build(BuildContext context) => Row(
    mainAxisSize: MainAxisSize.min,
    children: [
      if (value != null) ...[
        NumberField(value: value!, scales: scales, onChanged: onChanged),
        const SizedBox(width: Space.gap),
      ],
      Text(tr('不限', 'No limit')),
      Switch(
        value: value == null,
        onChanged: onChanged == null
            ? null
            : (unlimited) => onChanged!(unlimited ? null : initial),
      ),
    ],
  );
}

/// The policy's own default, allowed or denied: null, true or false.
class TriStateField extends StatelessWidget {
  final bool? value;
  final ValueChanged<bool?>? onChanged;
  const TriStateField({super.key, required this.value, this.onChanged});
  @override
  Widget build(BuildContext context) => ChoiceField<String>(
    value: switch (value) {
      null => '',
      true => 'allow',
      false => 'deny',
    },
    options: [
      ('', tr('默认', 'Default')),
      ('allow', tr('允许', 'Allow')),
      ('deny', tr('禁止', 'Deny')),
    ],
    onChanged: onChanged == null
        ? null
        : (v) => onChanged!(v.isEmpty ? null : v == 'allow'),
  );
}

/// One of a few values, side by side.
class ChoiceField<T> extends StatelessWidget {
  final T value;
  final List<(T, String)> options;
  final ValueChanged<T>? onChanged;
  const ChoiceField({
    super.key,
    required this.value,
    required this.options,
    this.onChanged,
  });
  @override
  Widget build(BuildContext context) => SegmentedButton<T>(
    showSelectedIcon: false,
    style: const ButtonStyle(visualDensity: VisualDensity.compact),
    segments: [
      for (final (v, label) in options)
        ButtonSegment(value: v, label: Text(label)),
    ],
    selected: {value},
    onSelectionChanged: onChanged == null ? null : (s) => onChanged!(s.first),
  );
}

/// A line of text; with [optional], empty is null.
class TextValueField extends StatefulWidget {
  final String? value;
  final bool optional;
  final ValueChanged<String?>? onChanged;
  const TextValueField({
    super.key,
    required this.value,
    this.optional = false,
    this.onChanged,
  });
  @override
  State<TextValueField> createState() => _TextValueFieldState();
}

class _TextValueFieldState extends State<TextValueField> {
  late final text = TextEditingController(text: widget.value ?? '');

  @override
  void dispose() {
    text.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) => TextField(
    controller: text,
    enabled: widget.onChanged != null,
    style: const TextStyle(fontSize: FontSizes.body),
    decoration: const InputDecoration(isDense: true),
    onChanged: (v) =>
        widget.onChanged?.call(widget.optional && v.trim().isEmpty ? null : v),
  );
}

/// A list of strings, each checked by [valid] as it is added.
class TagsField extends StatefulWidget {
  final List<String> values;
  final bool Function(String value)? valid;
  final ValueChanged<List<String>>? onChanged;
  const TagsField({
    super.key,
    required this.values,
    this.valid,
    this.onChanged,
  });
  @override
  State<TagsField> createState() => _TagsFieldState();
}

class _TagsFieldState extends State<TagsField> {
  final input = TextEditingController();
  bool invalid = false;

  @override
  void dispose() {
    input.dispose();
    super.dispose();
  }

  void add(String raw) {
    final value = raw.trim();
    if (value.isEmpty) return;
    if (widget.valid?.call(value) == false) {
      setState(() => invalid = true);
      return;
    }
    setState(() => invalid = false);
    input.clear();
    if (!widget.values.contains(value)) {
      widget.onChanged!([...widget.values, value]);
    }
  }

  @override
  Widget build(BuildContext context) => Column(
    crossAxisAlignment: CrossAxisAlignment.stretch,
    children: [
      Wrap(
        spacing: 6,
        runSpacing: 6,
        children: [
          for (final v in widget.values)
            InputChip(
              label: Text(v),
              visualDensity: VisualDensity.compact,
              onDeleted: widget.onChanged == null
                  ? null
                  : () => widget.onChanged!([
                      for (final o in widget.values)
                        if (o != v) o,
                    ]),
            ),
        ],
      ),
      if (widget.onChanged != null)
        TextField(
          controller: input,
          style: const TextStyle(fontSize: FontSizes.body),
          decoration: InputDecoration(
            isDense: true,
            hintText: tr('输入后按回车添加', 'Type and press Enter to add'),
            errorText: invalid ? tr('格式不对', 'Not valid') : null,
          ),
          onSubmitted: add,
        ),
    ],
  );
}

/// A choice of any of [options] (value, label); chosen values that are not
/// among them stay listed.
class MultiField extends StatelessWidget {
  final List<String> values;
  final List<(String, String)> options;
  final ValueChanged<List<String>>? onChanged;
  const MultiField({
    super.key,
    required this.values,
    required this.options,
    this.onChanged,
  });
  @override
  Widget build(BuildContext context) {
    final all = [
      ...options,
      for (final v in values)
        if (!options.any((o) => o.$1 == v)) (v, v),
    ];
    return Wrap(
      spacing: 6,
      runSpacing: 6,
      children: [
        for (final (v, label) in all)
          FilterChip(
            label: Text(label),
            visualDensity: VisualDensity.compact,
            selected: values.contains(v),
            onSelected: onChanged == null
                ? null
                : (on) => onChanged!([
                    for (final (o, _) in all)
                      if (o == v ? on : values.contains(o)) o,
                  ]),
          ),
      ],
    );
  }
}

/// Names and their values.
class KeyValueField extends StatefulWidget {
  final Map<String, String> values;
  final ValueChanged<Map<String, String>>? onChanged;
  const KeyValueField({super.key, required this.values, this.onChanged});
  @override
  State<KeyValueField> createState() => _KeyValueFieldState();
}

class _KeyValueFieldState extends State<KeyValueField> {
  final name = TextEditingController(), value = TextEditingController();

  @override
  void dispose() {
    name.dispose();
    value.dispose();
    super.dispose();
  }

  void add() {
    if (name.text.trim().isEmpty) return;
    widget.onChanged!({...widget.values, name.text.trim(): value.text});
    name.clear();
    value.clear();
  }

  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        for (final MapEntry(:key, :value) in widget.values.entries)
          Row(
            children: [
              SizedBox(
                width: 160,
                child: Text(key, overflow: TextOverflow.ellipsis),
              ),
              Expanded(
                child: Text(
                  value,
                  overflow: TextOverflow.ellipsis,
                  style: TextStyle(color: c.muted),
                ),
              ),
              if (widget.onChanged != null)
                IconButton(
                  tooltip: tr('移除', 'Remove'),
                  icon: const Icon(Icons.close, size: 16),
                  onPressed: () => widget.onChanged!({
                    for (final e in widget.values.entries)
                      if (e.key != key) e.key: e.value,
                  }),
                ),
            ],
          ),
        if (widget.onChanged != null)
          Row(
            children: [
              SizedBox(
                width: 160,
                child: TextField(
                  controller: name,
                  decoration: InputDecoration(
                    isDense: true,
                    hintText: tr('名称', 'Name'),
                  ),
                ),
              ),
              const SizedBox(width: Space.gap),
              Expanded(
                child: TextField(
                  controller: value,
                  decoration: InputDecoration(
                    isDense: true,
                    hintText: tr('值', 'Value'),
                  ),
                  onSubmitted: (_) => add(),
                ),
              ),
              IconButton(
                tooltip: tr('添加', 'Add'),
                icon: const Icon(Icons.add, size: 18),
                onPressed: add,
              ),
            ],
          ),
      ],
    );
  }
}

/// An environment variable the server may read a key from: one of [env]'s,
/// which says whether each is set. With [allowDefault], '' is the default.
class EnvField extends StatelessWidget {
  final String value;
  final Map<String, bool> env;
  final bool allowDefault;
  final ValueChanged<String>? onChanged;
  const EnvField({
    super.key,
    required this.value,
    required this.env,
    this.allowDefault = false,
    this.onChanged,
  });
  @override
  Widget build(BuildContext context) {
    String set(String name) =>
        env[name] == true ? tr('已设置', 'Set') : tr('未设置', 'Not set');
    if (onChanged == null) {
      return Text(
        value.isEmpty ? tr('默认', 'Default') : '$value · ${set(value)}',
      );
    }
    final names = {...env.keys, if (value.isNotEmpty) value}.toList()..sort();
    return ChoicePill<String>(
      tooltip: tr('环境变量', 'Environment variable'),
      value: value,
      options: [
        if (allowDefault) ('', tr('默认', 'Default')),
        for (final n in names) (n, n),
      ],
      detail: (n) => n.isEmpty ? '' : set(n),
      onChanged: onChanged!,
    );
  }
}

/// A list of objects, each edited in a dialog: [edit] returns the changed
/// object, or null when the edit was dropped.
class ObjectListField extends StatelessWidget {
  final List<Map<String, dynamic>> values;
  final String Function(Map<String, dynamic>) title, subtitle;
  final Future<Map<String, dynamic>?> Function(Map<String, dynamic>? value)
  edit;
  final ValueChanged<List<Map<String, dynamic>>>? onChanged;
  const ObjectListField({
    super.key,
    required this.values,
    required this.title,
    required this.subtitle,
    required this.edit,
    this.onChanged,
  });
  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        for (final (i, v) in values.indexed)
          Row(
            children: [
              Expanded(
                child: Column(
                  crossAxisAlignment: CrossAxisAlignment.start,
                  children: [
                    Text(title(v)),
                    Text(
                      subtitle(v),
                      overflow: TextOverflow.ellipsis,
                      style: TextStyle(
                        fontSize: FontSizes.caption,
                        color: c.subtle,
                      ),
                    ),
                  ],
                ),
              ),
              if (onChanged != null) ...[
                IconButton(
                  tooltip: tr('编辑', 'Edit'),
                  icon: const Icon(Icons.edit_outlined, size: 16),
                  onPressed: () async {
                    final changed = await edit(v);
                    if (changed != null) {
                      onChanged!([...values]..[i] = changed);
                    }
                  },
                ),
                IconButton(
                  tooltip: tr('移除', 'Remove'),
                  icon: const Icon(Icons.close, size: 16),
                  onPressed: () => onChanged!([...values]..removeAt(i)),
                ),
              ],
            ],
          ),
        if (onChanged != null)
          Align(
            alignment: Alignment.centerLeft,
            child: TextButton.icon(
              icon: const Icon(Icons.add, size: 16),
              label: Text(tr('添加', 'Add')),
              onPressed: () async {
                final added = await edit(null);
                if (added != null) onChanged!([...values, added]);
              },
            ),
          ),
      ],
    );
  }
}

/// How a field's change takes effect, unless at once; [pending] when the
/// file holds a change that waits for a restart.
class ApplyBadge extends StatelessWidget {
  final String apply;
  final bool pending;
  const ApplyBadge(this.apply, {super.key, this.pending = false});
  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    final (text, color) = switch (apply) {
      _ when pending => (tr('待重启', 'Pending restart'), c.warning),
      'new' => (tr('新建时生效', 'New only'), c.muted),
      'restart' => (tr('需重启', 'Restart'), c.warning),
      _ => ('', c.muted),
    };
    if (text.isEmpty) return const SizedBox.shrink();
    return Tooltip(
      message: pending
          ? tr(
              '文件里的新值在进程重启后生效，这里显示的是运行中的值',
              'The file holds a new value that applies once the process restarts; this is the running one',
            )
          : apply == 'new'
          ? tr(
              '只影响之后新建的任务、沙箱或待审批项',
              'Applies to runs, sandboxes and approvals created afterwards',
            )
          : tr('重启进程后生效', 'Takes effect once the process restarts'),
      child: Container(
        padding: const EdgeInsets.symmetric(horizontal: 6, vertical: 1),
        decoration: BoxDecoration(
          borderRadius: BorderRadius.circular(Radii.small),
          border: Border.all(color: color),
        ),
        child: Text(
          text,
          style: TextStyle(fontSize: FontSizes.caption, color: color),
        ),
      ),
    );
  }
}
