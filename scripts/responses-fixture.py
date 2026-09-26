#!/usr/bin/env python3
"""Local Responses wire fixture for the actual Codex app-server; no upstream calls."""
import json,os,time,uuid,gzip,zlib
from http.server import BaseHTTPRequestHandler,ThreadingHTTPServer
class Fixture(BaseHTTPRequestHandler):
 def log_message(self,*_):pass
 def do_POST(self):
  if self.path not in ('/v1/responses','/v1/responses/compact') or self.headers.get('authorization')!='Bearer local-responses-key':self.send_error(401);return
  raw=self.rfile.read(int(self.headers['content-length']))
  if self.headers.get('content-encoding')=='gzip':raw=gzip.decompress(raw)
  if self.headers.get('content-encoding')=='zstd':
   import compression.zstd
   raw=compression.zstd.decompress(raw)
  body=json.loads(raw)
  inp=body.get('input',[])
  users=[(n,item) for n,item in enumerate(inp) if item.get('role')=='user'];user_at,last=users[-1] if users else (0,{})
  content=last.get('content',[]);prompt=content if isinstance(content,str) else ' '.join(b.get('text','') for b in content if isinstance(b,dict))
  rid='resp_'+uuid.uuid4().hex
  if self.path.endswith('/compact'):
   result={'id':rid,'object':'response.compaction','output':[{'type':'compaction','id':'cp_local','encrypted_content':'local-compaction'}],'usage':{'input_tokens':20,'output_tokens':4}}
   self.send_response(200);self.send_header('Content-Type','application/json');self.end_headers();self.wfile.write(json.dumps(result).encode());return
  tool_done=any(i.get('type')=='function_call_output' for i in inp[user_at:])
  tool_names=[t.get('name') for t in body.get('tools',[])]
  text='Codex 本地协议联调成功：你好 Rust。' if not tool_done else 'Codex 已验证真实命令结果。'
  if 'exec' in prompt and not tool_done and any(t in tool_names for t in ('exec_command','exec')):
   item={'id':'fc_'+uuid.uuid4().hex,'type':'function_call','call_id':'call_fixture','name':'exec_command' if 'exec_command' in tool_names else 'exec','arguments':json.dumps({'cmd':"printf 'codex-once\\n' >> /workspace/native-marker; cat /workspace/native-marker",'max_output_tokens':300}),'status':'completed'}
  else:item={'id':'msg_'+uuid.uuid4().hex,'type':'message','role':'assistant','status':'completed','content':[{'type':'output_text','text':text,'annotations':[]}]}
  response={'id':rid,'object':'response','created_at':int(time.time()),'model':body['model'],'status':'completed','output':[item],'usage':{'input_tokens':31,'output_tokens':9,'total_tokens':40,'input_tokens_details':{'cached_tokens':7},'output_tokens_details':{'reasoning_tokens':2}}}
  events=[{'type':'response.created','response':{**response,'status':'in_progress','output':[]}}, {'type':'response.output_item.added','output_index':0,'item':{**item,'status':'in_progress','content':[]} if item['type']=='message' else item}]
  if item['type']=='message':
   events.append({'type':'response.content_part.added','item_id':item['id'],'output_index':0,'content_index':0,'part':{'type':'output_text','text':'','annotations':[]}})
   for start in range(0,len(text),4):events.append({'type':'response.output_text.delta','item_id':item['id'],'output_index':0,'content_index':0,'delta':text[start:start+4]})
   events.extend([{'type':'response.output_text.done','item_id':item['id'],'output_index':0,'content_index':0,'text':text},{'type':'response.content_part.done','item_id':item['id'],'output_index':0,'content_index':0,'part':item['content'][0]}])
  events.extend([{'type':'response.output_item.done','output_index':0,'item':item},{'type':'response.completed','response':response}])
  self.send_response(200);self.send_header('Content-Type','text/event-stream');self.end_headers()
  try:
   for n,event in enumerate(events):
    event['sequence_number']=n
    self.wfile.write(('event: '+event['type']+'\ndata: '+json.dumps(event,ensure_ascii=False)+'\n\n').encode());self.wfile.flush();time.sleep(.4 if 'slow' in prompt else .02)
  except (BrokenPipeError,ConnectionResetError):pass
if __name__=='__main__':ThreadingHTTPServer(('127.0.0.1',int(os.environ.get('RYZHUB_RESPONSES_FIXTURE_PORT','9082'))),Fixture).serve_forever()
