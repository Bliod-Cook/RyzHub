import 'dart:async';
import 'dart:convert';
import 'dart:math';

import 'package:flutter/material.dart';
import 'package:http/http.dart' as http;
import 'package:web_socket_channel/web_socket_channel.dart';
import 'package:xterm/xterm.dart';

import '../controller.dart';
import '../l10n.dart';
import '../rust/api.dart' show reconnectDelay;
import '../theme.dart';

/// A shell in the Work conversation's sandbox, over its own WebSocket.
///
/// Leaving (closing the panel, losing the connection) only detaches: the shell keeps running,
/// and reconnecting resumes from the output bytes already shown. "End session" ends the shell.
class TerminalPanel extends StatefulWidget {
  const TerminalPanel({
    super.key,
    required this.hub,
    required this.conversation,
    required this.onClose,
  });

  final HubController hub;
  final String conversation;
  final VoidCallback onClose;

  @override
  State<TerminalPanel> createState() => _TerminalPanelState();
}

class _TerminalPanelState extends State<TerminalPanel> {
  final terminal = Terminal(maxLines: 10000);
  WebSocketChannel? socket;
  StreamSubscription<dynamic>? subscription;
  late ByteConversionSink decoder;

  /// Output bytes shown so far, by the session's offsets: where a reconnect resumes.
  int offset = 0;
  String status = 'connecting';

  /// Why the server refused the terminal; it is not retried.
  String refused = '';
  int? exitCode;
  int attempt = 0;
  bool disposed = false;
  Timer? retry;

  @override
  void initState() {
    super.initState();
    decoder = const Utf8Decoder(allowMalformed: true)
        .startChunkedConversion(_TerminalSink(terminal));
    terminal.onOutput = (data) => _send(utf8.encode(data));
    terminal.onResize = (cols, rows, _, _) => _send(
      jsonEncode({
        'resize': {'cols': cols, 'rows': rows},
      }),
    );
    _connect();
  }

  void _send(Object message) {
    if (status == 'online') socket?.sink.add(message);
  }

  Future<void> _connect() async {
    if (disposed) return;
    setState(() => status = 'connecting');
    try {
      final hub = widget.hub;
      final ticket = await hub.ticket();
      final uri =
          Uri.parse(
            '${hub.base}/orgs/${hub.org}/conversations/${widget.conversation}/terminal',
          ).replace(
            scheme: hub.base.startsWith('https:') ? 'wss' : 'ws',
            queryParameters: {
              'ticket': ticket,
              'cols': '${terminal.viewWidth}',
              'rows': '${terminal.viewHeight}',
              'since': '$offset',
            },
          );
      final channel = WebSocketChannel.connect(uri);
      await channel.ready;
      if (disposed) {
        await channel.sink.close();
        return;
      }
      socket = channel;
      subscription = channel.stream.listen(
        _receive,
        onDone: _closed,
        onError: (_) => _closed(),
      );
    } on http.ClientException {
      _closed();
    } on TimeoutException {
      _closed();
    } on WebSocketChannelException {
      _closed();
    } catch (e) {
      if (!disposed) setState(() => refused = errorText(e));
    }
  }

  void _receive(dynamic message) {
    if (message is String) {
      final control = jsonDecode(message) as Map<String, dynamic>;
      if (control['attached'] case {'start': final int start}) {
        // Output older than the session still keeps is gone.
        if (start > offset) offset = start;
        attempt = 0;
        setState(() {
          status = 'online';
          exitCode = null;
        });
      } else if (control['exit'] case final int code) {
        setState(() => exitCode = code);
      }
      return;
    }
    final bytes = (message as List).cast<int>();
    offset += bytes.length;
    decoder.add(bytes);
  }

  void _closed() {
    subscription?.cancel();
    socket = null;
    if (disposed) return;
    if (exitCode != null) {
      setState(() => status = 'ended');
      return;
    }
    // Detached, not ended: resume after a pause that grows with failures.
    setState(() => status = 'reconnecting');
    final delay = reconnectDelay(
      attempt: attempt++,
      random: Random().nextInt(1 << 30),
    );
    retry = Timer(Duration(milliseconds: delay), _connect);
  }

  void _restart() {
    offset = 0;
    exitCode = null;
    terminal.buffer.clear();
    terminal.buffer.setCursor(0, 0);
    _connect();
  }

  void _end() {
    _send(jsonEncode({'close': true}));
  }

  @override
  void dispose() {
    disposed = true;
    retry?.cancel();
    subscription?.cancel();
    socket?.sink.close();
    decoder.close();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    final c = HubColors.of(context);
    final label = switch (status) {
      _ when refused.isNotEmpty => refused,
      'online' => null,
      'ended' => tr('会话已结束（退出码 $exitCode）', 'Session ended (exit $exitCode)'),
      'reconnecting' => tr('连接中断，正在重连…', 'Disconnected, reconnecting…'),
      _ => tr('正在连接…', 'Connecting…'),
    };
    return Container(
      height: 320,
      margin: const EdgeInsets.fromLTRB(Space.edge, 0, Space.edge, 6),
      decoration: BoxDecoration(
        color: c.sidebar,
        borderRadius: BorderRadius.circular(Radii.field),
        border: Border.all(color: c.border),
      ),
      clipBehavior: Clip.antiAlias,
      child: Column(
        children: [
          SizedBox(
            height: 34,
            child: Row(
              children: [
                const SizedBox(width: Space.gap),
                Icon(Icons.terminal, size: 16, color: c.muted),
                const SizedBox(width: 6),
                Text(
                  tr('终端', 'Terminal'),
                  style: TextStyle(fontSize: FontSizes.label, color: c.muted),
                ),
                if (label != null) ...[
                  const SizedBox(width: 10),
                  Flexible(
                    child: Text(
                      label,
                      overflow: TextOverflow.ellipsis,
                      style: TextStyle(
                        fontSize: FontSizes.caption,
                        color: refused.isEmpty ? c.subtle : c.danger,
                      ),
                    ),
                  ),
                ],
                const Spacer(),
                if (status == 'ended')
                  TextButton(
                    onPressed: _restart,
                    child: Text(tr('新会话', 'New session')),
                  )
                else
                  TextButton(
                    onPressed: status == 'online' ? _end : null,
                    child: Text(tr('结束会话', 'End session')),
                  ),
                IconButton(
                  tooltip: tr('收起终端', 'Hide terminal'),
                  iconSize: 18,
                  onPressed: widget.onClose,
                  icon: const Icon(Icons.close),
                ),
                const SizedBox(width: Space.gap),
              ],
            ),
          ),
          Expanded(
            child: TerminalView(
              terminal,
              autofocus: true,
              textStyle: TerminalStyle(
                fontFamily: codeStyle.fontFamily!,
                fontSize: FontSizes.label,
              ),
              padding: const EdgeInsets.all(6),
            ),
          ),
        ],
      ),
    );
  }
}

/// Writes decoded output to the terminal.
class _TerminalSink implements Sink<String> {
  _TerminalSink(this.terminal);
  final Terminal terminal;

  @override
  void add(String data) => terminal.write(data);

  @override
  void close() {}
}
