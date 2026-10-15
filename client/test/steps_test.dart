import 'package:flutter/gestures.dart';
import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:ryzhub/prefs.dart';

import 'kernel.dart';

Map<String, dynamic> _item(
  String id,
  String kind, {
  String text = '',
  String state = 'completed',
  Map<String, dynamic>? tool,
}) => {
  'id': id,
  'kind': kind,
  'state': state,
  'text': text,
  'run_id': 'run',
  if (tool != null)
    'detail': {
      'kind': {'Tool': tool},
    },
};

/// One Chat run: a step that only called memory_search, then the answer.
final _run = [
  {'id': 'ask', 'kind': 'user', 'state': 'completed', 'text': '你记得我吗'},
  _item('step', 'assistant'),
  _item('context', 'memory_context', text: '[]'),
  _item(
    'search',
    'tool',
    text: '{"matches":[]}',
    tool: {
      'name': 'memory_search',
      'arguments_json': '{"query":"用户"}',
      'result_json': '{"matches":[]}',
      'output': '',
    },
  ),
  _item('answer', 'assistant', text: '没有找到相关记忆。'),
];

Widget _render(List<Map<String, dynamic>> rows, {bool running = false}) =>
    MaterialApp(
      theme: hubTheme,
      home: Scaffold(
        body: ListView(
          children: [
            for (final row in rows)
              row['kind'] == 'steps'
                  ? stepsGroup(
                      steps: row['steps'],
                      running: running,
                      latest: row['latest'] == true,
                    )
                  : messageCard(item: row),
          ],
        ),
      ),
    );

void main() {
  setUp(() => prefs.language = 'zh');

  testWidgets('a finished run shows one toolbar and a folded summary', (
    tester,
  ) async {
    await tester.pumpWidget(_render(timeline(_run)));
    expect(find.byTooltip('复制消息'), findsNWidgets(2));
    // The user's bubble has the other copy button.
    expect(find.text('Chat'), findsOneWidget);
    expect(find.text('已调用 1 个工具'), findsOneWidget);
    expect(find.textContaining('搜索记忆'), findsNothing);

    await tester.tap(find.text('已调用 1 个工具'));
    await tester.pumpAndSettle();
    expect(find.textContaining('已搜索记忆“用户” · 无结果'), findsOneWidget);
    expect(find.textContaining('"matches"'), findsNothing);
    expect(find.text('memory_search'), findsNothing);

    await tester.tap(find.textContaining('搜索记忆'));
    await tester.pumpAndSettle();
    expect(find.textContaining('"matches"'), findsOneWidget);
    expect(find.text('memory_search'), findsOneWidget);
  });

  testWidgets('hovering a summary darkens its text, flush with the column', (
    tester,
  ) async {
    await tester.pumpWidget(_render(timeline(_run)));
    final label = find.text('已调用 1 个工具');
    Color? color() => tester.widget<Text>(label).style?.color;
    final resting = color();
    expect(tester.getTopLeft(find.byIcon(Icons.auto_awesome_outlined)).dx, 0);
    final mouse = await tester.createGesture(kind: PointerDeviceKind.mouse);
    await mouse.addPointer(location: tester.getCenter(label));
    await tester.pump();
    expect(color(), isNot(resting));
    await mouse.moveTo(Offset.zero);
    await tester.pump();
    expect(color(), resting);
  });

  testWidgets('built-in tools are worded for how far they got', (tester) async {
    prefs.language = 'en';
    Map<String, dynamic> exec(String id, String state, String result) => _item(
      id,
      'tool',
      state: state,
      tool: {
        'name': 'exec',
        'arguments_json': '{"cmd":"ls"}',
        'result_json': result,
        'output': '',
      },
    );
    final steps = [
      exec('fails', 'completed', '{"state":"completed","exit_code":2}'),
      exec('pending', 'planned', ''),
      _item(
        'other',
        'tool',
        tool: {'name': 'github/search', 'arguments_json': '{}'},
      ),
      _item(
        'web',
        'tool',
        tool: {'name': 'web_search', 'arguments_json': '{"query":"rust"}'},
      ),
      _item(
        'page',
        'tool',
        tool: {
          'name': 'web_search',
          'arguments_json': '{"url":"https://a.example/post"}',
        },
      ),
      _item(
        'image',
        'tool',
        tool: {'name': 'view_image', 'arguments_json': '{"path":"chart.png"}'},
      ),
    ];
    await tester.pumpWidget(
      MaterialApp(
        theme: hubTheme,
        home: Scaffold(body: stepsGroup(steps: steps, running: true)),
      ),
    );
    expect(find.textContaining('Run command ls · Exit code 2'), findsOneWidget);
    expect(find.textContaining('Running command ls'), findsOneWidget);
    expect(find.textContaining('github/search'), findsOneWidget);
    expect(find.textContaining('Searched the web “rust”'), findsOneWidget);

    expect(
      find.textContaining('Searched the web “https://a.example/post”'),
      findsOneWidget,
    );
    expect(find.textContaining('Viewed an image chart.png'), findsOneWidget);
    await tester.pumpWidget(const SizedBox());
  });

  testWidgets('after the model speaks, each run of calls folds by itself', (
    tester,
  ) async {
    prefs.language = 'en';
    Map<String, dynamic> web(String id, {String state = 'completed'}) => _item(
      id,
      'tool',
      state: state,
      tool: {'name': 'web_search', 'arguments_json': '{"query":"news"}'},
    );
    Map<String, dynamic> exec(String id) => _item(
      id,
      'tool',
      tool: {'name': 'exec', 'arguments_json': '{"cmd":"ls"}'},
    );
    final image = _item(
      'image',
      'tool',
      tool: {'name': 'view_image', 'arguments_json': '{"path":"a.png"}'},
    );
    final steps = [
      web('a'),
      web('b'),
      _item('said', 'assistant', text: 'First round done.'),
      web('c'),
      exec('d'),
    ];
    Future<void> show(
      List<Map<String, dynamic>> steps, {
      bool running = false,
      Duration? worked,
    }) => tester.pumpWidget(
      MaterialApp(
        theme: hubTheme,
        home: Scaffold(
          body: stepsGroup(
            steps: steps,
            running: running,
            latest: running,
            worked: worked,
          ),
        ),
      ),
    );
    await show(steps, worked: const Duration(seconds: 65));
    expect(find.text('Worked for 1m 5s'), findsOneWidget);
    await tester.tap(find.text('Worked for 1m 5s'));
    await tester.pumpAndSettle();
    expect(find.text('Searched the web twice'), findsOneWidget);
    expect(find.text('First round done.'), findsOneWidget);
    expect(find.text('Searched the web once, ran 1 command'), findsOneWidget);
    expect(find.textContaining('Searched the web “news”'), findsNothing);
    await tester.tap(find.text('Searched the web twice'));
    await tester.pumpAndSettle();
    expect(find.textContaining('Searched the web “news”'), findsNWidgets(2));
    // The fold the run is at work in stays open; the finished one does not.
    await tester.pumpWidget(const SizedBox());
    await show([...steps.take(4), web('e', state: 'running')], running: true);
    await tester.pump(const Duration(milliseconds: 300));
    expect(find.text('Working…'), findsOneWidget);
    expect(find.textContaining('Searched the web “news”'), findsOneWidget);
    expect(find.textContaining('Searching the web “news”'), findsOneWidget);
    // Without the model's words the time leads what the calls did.
    await tester.pumpWidget(const SizedBox());
    await show([web('a'), exec('d')], worked: const Duration(seconds: 10));
    expect(
      find.text('Worked for 10s, searched the web once, ran 1 command'),
      findsOneWidget,
    );
    // Web searches count apart from other tools.
    await tester.pumpWidget(const SizedBox());
    await show([web('a'), _run[3], image, web('b')]);
    expect(
      find.text('Searched the web twice, viewed 1 image, used 1 tool'),
      findsOneWidget,
    );
    prefs.language = 'zh';
    await tester.pumpWidget(const SizedBox());
    await show(steps, worked: const Duration(seconds: 8));
    expect(find.text('已工作 8 秒'), findsOneWidget);
    await tester.pumpWidget(const SizedBox());
    await show([web('a'), exec('d')], worked: const Duration(seconds: 8));
    expect(find.text('已工作 8 秒，搜索网页 1 次，运行 1 条命令'), findsOneWidget);
    await tester.pumpWidget(const SizedBox());
    await show([web('a'), _run[3], image, web('b')]);
    expect(find.text('已搜索网页 2 次，查看 1 张图片，调用 1 个工具'), findsOneWidget);
    await tester.pumpWidget(const SizedBox());
  });

  testWidgets('a working run stays open and its reply has no toolbar yet', (
    tester,
  ) async {
    final rows = timeline([
      ..._run.take(4),
      _item('answer', 'assistant', text: '正在', state: 'streaming'),
    ]);
    await tester.pumpWidget(_render(rows, running: true));
    expect(find.textContaining('搜索记忆'), findsOneWidget);
    expect(find.text('模拟 Agent'), findsNothing);
    expect(find.text('生成中…'), findsOneWidget);

    await tester.pumpWidget(_render(timeline(_run.take(4)), running: true));
    expect(find.text('处理中…'), findsOneWidget);
    await tester.pumpWidget(const SizedBox());
  });

  testWidgets(
    'a subagent works under its call; changed files open their diff',
    (tester) async {
      prefs.language = 'en';
      final call = _item(
        'call',
        'tool',
        tool: {'name': 'Agent', 'arguments_json': '{"prompt":"look"}'},
      );
      final child = {
        ..._item('sub', 'assistant', text: 'subagent finding'),
        'parent_item_id': 'call',
      };
      final edit = {
        ..._item('edit', 'file_change'),
        'detail': {
          'kind': {
            'FileChange': {
              'changes': [
                {
                  'path': 'a.txt',
                  'change': 'update',
                  'diff': '@@ -1 +1 @@\n-old\n+new',
                },
              ],
            },
          },
        },
      };
      await tester.pumpWidget(
        MaterialApp(
          theme: hubTheme,
          home: Scaffold(
            body: SingleChildScrollView(
              child: stepsGroup(
                steps: [call, edit],
                running: true,
                latest: true,
                children: (id) => id == 'call' ? [child] : const [],
              ),
            ),
          ),
        ),
      );
      await tester.pump(const Duration(milliseconds: 600));
      expect(
        find.textContaining('subagent finding', findRichText: true),
        findsOneWidget,
      );
      expect(find.text('Edited a.txt'), findsOneWidget);
      expect(find.textContaining('+new', findRichText: true), findsNothing);
      await tester.tap(find.text('Edited a.txt'));
      await tester.pump(const Duration(milliseconds: 600));
      expect(find.textContaining('+new', findRichText: true), findsOneWidget);
    },
  );
}
