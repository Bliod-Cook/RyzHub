import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:ryzhub/link.dart';

const _a = '01a10cb9-2f0f-7270-90a9-4a2478d29971';
const _b = '01a10cb9-2f0f-7270-90a9-4a2478d29972';

void main() {
  test('an address names a conversation only as /c/<id>', () {
    expect(conversationIn(Uri.parse('/c/$_a')), _a);
    expect(conversationIn(Uri.parse('/')), '');
    expect(conversationIn(Uri.parse('/c/not-an-id')), '');
    expect(conversationIn(Uri.parse('/c/$_a/more')), '');
    const parser = ConversationLinkParser();
    expect(parser.restoreRouteInformation(_a).uri.toString(), '/c/$_a');
    expect(parser.restoreRouteInformation('').uri.toString(), '/');
  });

  test('the library has its own address', () {
    expect(conversationIn(Uri.parse('/library')), libraryLink);
    expect(conversationIn(Uri.parse('/library/x')), '');
    const parser = ConversationLinkParser();
    expect(
      parser.restoreRouteInformation(libraryLink).uri.toString(),
      '/library',
    );
  });

  testWidgets('the address follows the page and back reaches the page', (
    tester,
  ) async {
    final reported = <String>[];
    tester.binding.defaultBinaryMessenger.setMockMethodCallHandler(
      SystemChannels.navigation,
      (call) async {
        if (call.method == 'routeInformationUpdated') {
          reported.add((call.arguments as Map)['uri'] as String);
        }
        return null;
      },
    );
    final link = ConversationLink((_) => const Text('page'));
    await tester.pumpWidget(
      MaterialApp.router(
        routerDelegate: link,
        routeInformationParser: const ConversationLinkParser(),
      ),
    );
    expect(find.text('page'), findsOneWidget);
    link.show(_a);
    await tester.pump();
    expect(reported.last, '/c/$_a');
    // Back or forward: the platform pushes the address it returned to.
    await tester.binding.defaultBinaryMessenger.handlePlatformMessage(
      SystemChannels.navigation.name,
      SystemChannels.navigation.codec.encodeMethodCall(
        const MethodCall('pushRouteInformation', {
          'location': '/c/$_b',
          'state': null,
        }),
      ),
      (_) {},
    );
    await tester.pump();
    expect(link.requested.value, _b);
    // An address the page cannot open goes back to what it shows.
    link.refuse();
    await tester.pump();
    expect(reported.last, '/c/$_a');
    link.show('');
    await tester.pump();
    expect(reported.last, '/');
  });
}
