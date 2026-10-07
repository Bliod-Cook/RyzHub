import 'package:flutter/gestures.dart';
import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:ryzhub/prefs.dart';
import 'package:ryzhub/ui/messages.dart';

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
      home: Scaffold(
        body: ListView(
          children: [
            for (final row in rows)
              row['kind'] == 'steps'
                  ? StepsGroup(
                      steps: row['steps'],
                      running: running,
                      latest: row['latest'] == true,
                    )
                  : MessageCard(item: row),
          ],
        ),
      ),
    );

void main() {
  setUp(() => prefs.language = 'zh');

  test('a run folds its tool steps before one answer', () {
    final rows = timeline(_run);
    expect(rows.map((r) => r['id']), ['ask', 'steps:search', 'answer']);
    expect((rows[1]['steps'] as List).map((s) => s['id']), ['search']);
    expect(rows[1]['latest'], isFalse);
  });

  test('a searched reply shows its search between its texts', () {
    final rows = timeline([
      {'id': 'ask', 'kind': 'user', 'state': 'completed', 'text': '新闻'},
      _item('intro', 'assistant', text: '先搜索一下'),
      _item(
        'web',
        'tool',
        tool: {'name': 'web_search', 'arguments_json': '{"query":"news"}'},
      ),
      // What a failed attempt showed before the step was retried.
      _item('lost', 'assistant', text: '', state: 'discarded'),
      _item('answer', 'assistant', text: '今天的新闻'),
    ]);
    expect(rows.map((r) => r['id']), ['ask', 'steps:intro', 'answer']);
    expect((rows[1]['steps'] as List).map((s) => s['id']), ['intro', 'web']);
  });

  test('a run still at work has no answer yet', () {
    final rows = timeline(_run.take(4));
    expect(rows.map((r) => r['id']), ['ask', 'steps:search']);
    expect(rows.last['latest'], isTrue);
  });

  testWidgets('a finished run shows one toolbar and a folded summary', (
    tester,
  ) async {
    await tester.pumpWidget(_render(timeline(_run)));
    expect(find.byTooltip('复制消息'), findsNWidgets(2));
    // The user's bubble has the other copy button.
    expect(find.text('模拟 Agent'), findsOneWidget);
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
      _run[3],
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
        home: Scaffold(body: StepsGroup(steps: steps, running: true)),
      ),
    );
    expect(
      find.textContaining('Searched memories “用户” · No results'),
      findsOneWidget,
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

  testWidgets('a finished group counts its web searches apart', (tester) async {
    Map<String, dynamic> web(String id) => _item(
      id,
      'tool',
      tool: {'name': 'web_search', 'arguments_json': '{"query":"q"}'},
    );
    final image = _item(
      'image',
      'tool',
      tool: {'name': 'view_image', 'arguments_json': '{"path":"a.png"}'},
    );
    for (final (language, label) in [
      ('en', 'Searched the web twice, viewed 1 image, used 1 tool'),
      ('zh', '已搜索网页 2 次，查看 1 张图片，调用 1 个工具'),
    ]) {
      prefs.language = language;
      await tester.pumpWidget(
        MaterialApp(
          home: Scaffold(
            body: StepsGroup(steps: [web('a'), _run[3], image, web('b')]),
          ),
        ),
      );
      expect(find.text(label), findsOneWidget);
    }
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
        home: Scaffold(
          body: StepsGroup(
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
    await tester.pumpWidget(const SizedBox());
    await show([web('a'), exec('d')]);
    expect(find.text('Searched the web once, ran 1 command'), findsOneWidget);
    prefs.language = 'zh';
    await tester.pumpWidget(const SizedBox());
    await show(steps, worked: const Duration(seconds: 8));
    expect(find.text('已工作 8 秒'), findsOneWidget);
    await tester.pumpWidget(const SizedBox());
    await show([web('a'), exec('d')], worked: const Duration(seconds: 8));
    expect(find.text('已工作 8 秒，搜索网页 1 次，运行 1 条命令'), findsOneWidget);
    await tester.pumpWidget(const SizedBox());
  });

  testWidgets('a streaming reply shows a link once the server cites it', (
    tester,
  ) async {
    prefs.language = 'en';
    const news = 'https://www.news.cn/a';
    const ap = 'https://apnews.com/b';
    Future<String> show(String text, {String state = 'streaming'}) async {
      await tester.pumpWidget(
        MaterialApp(
          home: Scaffold(
            body: MessageCard(
              item: {
                ..._item('answer', 'assistant', text: text, state: state),
                'detail': {
                  'kind': {
                    'Answer': {
                      'citations': [
                        {'url': news, 'title': 'News'},
                      ],
                    },
                  },
                },
              },
            ),
          ),
        ),
      );
      return tester
          .widgetList<SelectableText>(find.byType(SelectableText))
          .map((t) => t.textSpan?.toPlainText() ?? t.data ?? '')
          .join('\n');
    }

    const cited = '体育。([news.cn]($news))\n\n巴西。';
    // Not yet cited, or not yet written out: held back at the tail.
    var shown = await show('$cited([apnews.com]($ap)) ');
    expect(find.text('news.cn'), findsOneWidget);
    expect(shown, isNot(contains('apnews')));
    shown = await show('$cited([apnews.com](https://apnews.co');
    expect(shown, isNot(contains('apnews')));
    expect(shown, contains('巴西。'));
    // An uncited link inside the text, or in a finished reply, stays a link.
    shown = await show('$cited([apnews.com]($ap)) 继续');
    expect(shown, contains('apnews.com'));
    shown = await show('$cited([apnews.com]($ap))', state: 'completed');
    expect(shown, contains('apnews.com'));
  });

  testWidgets('citation links fold into chips that open their pages', (
    tester,
  ) async {
    prefs.language = 'en';
    const news = 'https://www.news.cn/a?utm_source=openai';
    const ap = 'https://apnews.com/b';
    const wiki = 'https://en.wikipedia.org/wiki/Asian_Games_(2026)';
    final opened = <String>[];
    await tester.pumpWidget(
      MaterialApp(
        home: Scaffold(
          body: MessageCard(
            item: {
              ..._item(
                'answer',
                'assistant',
                text:
                    '体育消息。([news.cn]($news))\n\n'
                    '巴西消息。([apnews.com]($ap)) ([wikipedia.org]($wiki))\n\n'
                    '再次提到 ([apnews.com]($ap), [news.cn]($news))，'
                    '另见 [普通链接](https://plain.example)。',
              ),
              'detail': {
                'kind': {
                  'Answer': {
                    'citations': [
                      {'url': news, 'title': ''},
                      {'url': ap, 'title': 'Lula seeks shake-up'},
                      {'url': wiki, 'title': 'Asian Games'},
                      {
                        'url': 'https://unlinked.example/p',
                        'title': 'Unlinked',
                      },
                    ],
                  },
                },
              },
            },
            onSource: opened.add,
          ),
        ),
      ),
    );
    final shown = tester
        .widgetList<SelectableText>(find.byType(SelectableText))
        .map((t) => t.textSpan?.toPlainText() ?? t.data ?? '')
        .join('\n');
    expect(shown, contains('体育消息。'));
    expect(shown, contains('普通链接'));
    expect(shown, isNot(contains('](')));
    expect(find.text('news.cn'), findsOneWidget);
    expect(find.text('apnews.com +1'), findsNWidgets(2));
    // Only the page the text does not link is listed below it.
    expect(find.text('Sources'), findsOneWidget);
    expect(find.textContaining('Unlinked'), findsOneWidget);
    expect(find.textContaining('Lula seeks shake-up'), findsNothing);
    await tester.tap(find.text('news.cn'));
    expect(opened, [news]);
    await tester.tap(find.text('apnews.com +1').first);
    await tester.pumpAndSettle();
    expect(find.text('Lula seeks shake-up'), findsOneWidget);
    await tester.tap(find.text('Asian Games'));
    await tester.pumpAndSettle();
    expect(opened, [news, wiki]);
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
}
