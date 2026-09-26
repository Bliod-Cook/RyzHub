#!/usr/bin/env python3
"""Local Anthropic Messages wire fixture; never calls a model or external service."""
import json
import os
import time
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


class Fixture(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_POST(self):
        if self.path.split('?')[0] != '/v1/messages' or self.headers.get('x-api-key') != 'local-fixture-key':
            self.send_error(401)
            return
        request = json.loads(self.rfile.read(int(self.headers['content-length'])))
        last = request['messages'][-1]['content']
        prompt = last if isinstance(last, str) else ' '.join(b.get('text','') for b in last if isinstance(b,dict))
        tools = [t.get('name') for t in request.get('tools',[])]
        claude = 'Bash' in tools
        if os.environ.get('RYZHUB_FIXTURE_REQUESTS'):
            with open(os.environ['RYZHUB_FIXTURE_REQUESTS'],'a') as out: out.write(json.dumps(request,ensure_ascii=False)+'\n')
        tool_result = isinstance(last, list) and any(b.get('type') == 'tool_result' for b in last)
        content = [{'type': 'thinking', 'thinking': 'fixture reasoning', 'signature': 'fixture-signature'}]
        if not tool_result and request.get('tools') and (('fixture-exec' in prompt or prompt.strip()=='exec') if claude else ('exec' in prompt or '执行' in prompt)):
            content += [{'type': 'text', 'text': '将执行一条沙箱命令，请确认。'}, {'type': 'tool_use', 'id': 'fixture_exec_'+uuid.uuid4().hex, 'name': 'Bash' if claude else 'exec', 'input': {'command': "printf 'claude-once\\n' >> /workspace/claude-marker; cat /workspace/claude-marker", 'description':'Local test command'} if claude else {'cmd': "printf 'containerd 沙箱验收成功\\n'; pwd", 'timeout_ms': 5000}}]
        elif not tool_result and request.get('tools') and (('fixture-question' in prompt or prompt.strip()=='question') if claude else ('question' in prompt or '提问' in prompt)):
            content += [{'type': 'tool_use', 'id': 'fixture_question_'+uuid.uuid4().hex, 'name': 'AskUserQuestion' if claude else 'ask_user', 'input': {'questions':[{'question':'你希望使用哪种颜色？','header':'颜色','options':[{'label':'蓝色','description':'Blue'},{'label':'绿色','description':'Green'}],'multiSelect':False}]} if claude else {'question': '你希望使用哪种颜色？'}}]
        else:
            text = ('工具结果：\n\n' + '\n'.join(str(b.get('content', '')) for b in last)) if tool_result else ('本地协议测试响应：' + prompt)
            # SDK adds large system reminders to user messages; keep local fixtures bounded.
            if claude: text = 'Claude 本地协议测试响应：' + text[-256:]
            content += [{'type': 'text', 'text': text}]
        events = [{'type': 'message_start', 'message': {'id': 'msg_'+uuid.uuid4().hex, 'type': 'message', 'role': 'assistant', 'content': [], 'model': request['model'], 'usage': {'input_tokens': 25, 'output_tokens': 1}}}, {'type': 'ping'}]
        for index, block in enumerate(content):
            start = dict(block)
            if block['type'] == 'text':
                start['text'] = ''
            if block['type']=='tool_use': start['input']={}
            if block['type']=='thinking': start.update(thinking='',signature='')
            events.append({'type': 'content_block_start', 'index': index, 'content_block': start})
            if block['type'] == 'text':
                for offset in range(0, len(block['text']), 8):
                    events.append({'type': 'content_block_delta', 'index': index, 'delta': {'type': 'text_delta', 'text': block['text'][offset:offset+8]}})
            if block['type']=='thinking':
                events.append({'type':'content_block_delta','index':index,'delta':{'type':'thinking_delta','thinking':block['thinking']}})
                events.append({'type':'content_block_delta','index':index,'delta':{'type':'signature_delta','signature':block['signature']}})
            if block['type']=='tool_use': events.append({'type':'content_block_delta','index':index,'delta':{'type':'input_json_delta','partial_json':json.dumps(block['input'])}})
            events.append({'type': 'content_block_stop', 'index': index})
        events += [{'type': 'message_delta', 'delta': {'stop_reason': 'tool_use' if any(b['type'] == 'tool_use' for b in content) else 'end_turn'}, 'usage': {'output_tokens': 8}}, {'type': 'message_stop'}]
        if not request.get('stream',False):
            result={'id':'msg_'+uuid.uuid4().hex,'type':'message','role':'assistant','model':request['model'],'content':content,'stop_reason':'end_turn','stop_sequence':None,'usage':{'input_tokens':25,'output_tokens':8}}
            self.send_response(200);self.send_header('Content-Type','application/json');self.end_headers();self.wfile.write(json.dumps(result,ensure_ascii=False).encode());return
        self.send_response(200)
        self.send_header('Content-Type', 'text/event-stream')
        self.send_header('Cache-Control', 'no-store')
        self.end_headers()
        try:
            for event in events:
                self.wfile.write(('event: '+event['type']+'\ndata: '+json.dumps(event, ensure_ascii=False)+'\n\n').encode())
                self.wfile.flush()
                time.sleep(0.2 if 'slow' in prompt or '慢速' in prompt else 0.02)
        except (BrokenPipeError, ConnectionResetError):
            pass


if __name__ == '__main__':
    port = int(os.environ.get('RYZHUB_FIXTURE_PORT', '9081'))
    print(f'Local Anthropic protocol fixture: http://127.0.0.1:{port}', flush=True)
    ThreadingHTTPServer(('127.0.0.1', port), Fixture).serve_forever()
