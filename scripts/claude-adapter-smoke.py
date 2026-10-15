"""Actual official SDK + local HTTP fixture. No external model calls."""
import subprocess,os,json,threading,queue,tempfile,time,pathlib,sys,socket
root=pathlib.Path(__file__).resolve().parent.parent
out=root/'var/claude-smoke';out.mkdir(parents=True,exist_ok=True)
with socket.socket() as s:s.bind(('127.0.0.1',0));port=s.getsockname()[1]
with tempfile.TemporaryDirectory(prefix='ryzhub-claude-') as home:
 env={**os.environ,'HOME':home,'RYZHUB_WORKSPACE':home,'ANTHROPIC_BASE_URL':f'http://127.0.0.1:{port}','ANTHROPIC_API_KEY':'local-fixture-key','RYZHUB_CLAUDE_MODEL':'claude-sonnet-4-6','CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC':'1','RYZHUB_FIXTURE_PORT':str(port)}
 fixture=subprocess.Popen([sys.executable,str(root/'scripts/anthropic-fixture.py')],env=env,stdout=subprocess.DEVNULL)
 adapter=subprocess.Popen(['node',str(root/'harness/claude/dist/adapter.js')],env=env,stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=open(out/'stderr.log','w'),text=True)
 events=queue.Queue();threading.Thread(target=lambda:[events.put(json.loads(line)) for line in adapter.stdout],daemon=True).start()
 def send(id,method,params):adapter.stdin.write(json.dumps({'id':id,'method':method,'params':params})+'\n');adapter.stdin.flush()
 def until(test,timeout=45):
  deadline=time.time()+timeout
  while time.time()<deadline:
   try:e=events.get(timeout=1)
   except queue.Empty:continue
   with open(out/'events.jsonl','a') as f:f.write(json.dumps(e,ensure_ascii=False)+'\n')
   if test(e):return e
  raise AssertionError('timed out, inspect adapter log')
 try:
  time.sleep(.3);send('init','initialize',{});until(lambda e:e.get('id')=='init')
  send('session','thread/start',{'model':'claude-sonnet-4-6'});thread=until(lambda e:e.get('id')=='session')['result']['thread']['id']
  send('turn','turn/start',{'threadId':thread,'input':[{'text':'hello 本地协议'}]});until(lambda e:e.get('id')=='turn')
  done=until(lambda e:e.get('method')=='turn/completed');assert done['params']['turn']['status']=='completed',done
  send('config','session/configure',{'model':'claude-sonnet-4-6','reasoning':'high','approval':'ask','plan':True});until(lambda e:e.get('id')=='config')
  print('Actual SDK start, streaming, result and configuration passed')
 finally:
  adapter.stdin.close()
  try:adapter.wait(timeout=5)
  except subprocess.TimeoutExpired:adapter.kill();adapter.wait()
  fixture.terminate();fixture.wait()
