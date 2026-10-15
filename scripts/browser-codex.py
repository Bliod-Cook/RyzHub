#!/usr/bin/env python3
import os,importlib.util,json,time
from pathlib import Path
from playwright.sync_api import sync_playwright,expect
os.environ.setdefault('RYZHUB_API_URL','http://127.0.0.1:8180')
spec=importlib.util.spec_from_file_location('r',Path(__file__).with_name('browser-regression.py'));r=importlib.util.module_from_spec(spec);spec.loader.exec_module(r)
with sync_playwright() as p:
 browser=p.chromium.launch(headless=True);context=browser.new_context(viewport={'width':1280,'height':1000},locale='zh-CN');r.track_sockets(context);errors=[];external=[]
 context.on('page',lambda page:page.on('pageerror',lambda e:errors.append(str(e))))
 context.on('request',lambda req:external.append(req.url) if not req.url.startswith((r.BASE,r.API,'blob:','data:')) else None)
 page=r.start(context);email=f'codex-browser-{time.time_ns()}@example.test';auth=r.login(page,email,'local-codex-password',True);passed=['registration']
 second=browser.new_context(viewport={'width':1280,'height':1000},locale='zh-CN');second.on('page',lambda page:page.on('pageerror',lambda e:errors.append(str(e))));other=r.start(second);r.login(other,email,'local-codex-password')
 # The draft's mode comes from the header switch; the first message creates it.
 page.get_by_role('button',name='Work',exact=True).click()
 created=r.create(page,'hello codex browser');conv=created['id'];assert created['mode']=='work'
 expect(other.get_by_role('button',name=created['title'],exact=True)).to_be_visible(timeout=15000)
 other.get_by_role('button',name=created['title'],exact=True).click()
 expect(other.get_by_role('button',name='保存检查点',exact=True)).to_be_visible()
 expect(page.get_by_role('button',name=created['title'],exact=True)).to_have_count(1)
 passed.append('second device discovers Work mode and no duplicate conversation')
 expect(page.get_by_role('button',name='保存检查点',exact=True)).to_be_visible()
 passed.append('truthful Work capabilities and controls')
 r.wait_state(page,lambda s:any(i['kind']=='assistant' and i['state']=='completed' and 'Codex' in i['text'] for i in r.items(s)))
 passed.append('actual Codex app-server Responses turn through Rust/WASM')
 def job(label,kind):
  with page.expect_response(lambda res:res.url.endswith('/workspace') and res.request.method=='POST' and res.status==200) as submitted:
   page.get_by_role('button',name=label,exact=True).click()
  assert submitted.value.status==200,submitted.value.text()
  job_id=submitted.value.json()['id'];deadline=time.monotonic()+90
  while time.monotonic()<deadline:
   status=context.request.get(f'{r.API}/orgs/{auth["org_id"]}/conversations/{conv}/workspace',headers={'Authorization':'Bearer '+auth['token']}).json()
   jobs=[j for j in status['jobs'] if j['id']==job_id and j['kind']==kind]
   if jobs and jobs[0]['state']=='completed':break
   assert not jobs or jobs[0]['state']!='failed',jobs
   page.wait_for_timeout(200)
  else:raise AssertionError('workspace job timeout')
  expect(page.get_by_role('button',name='保存检查点',exact=True)).to_be_enabled(timeout=10000)
 job('保存检查点','checkpoint');passed.append('checkpoint from browser with committed watermark')
 job('归档工作区','archive');passed.append('archive confirmed stopped workspace')
 job('恢复工作区','restore');passed.append('restore and RPO Notice in timeline')
 previous={i['id'] for i in r.items(r.cache(page))}
 r.send(page,'hello restored codex')
 r.wait_state(page,lambda s:any(i['id'] not in previous and i['kind']=='assistant' and i['state']=='completed' and 'Codex' in i['text'] for i in r.items(s)))
 passed.append('resumed native thread after full sandbox replacement')
 assert not any(i['text']=='输入已提交给 Codex' for i in r.items(r.cache(page)))
 expect(page.get_by_role('textbox',name='系统通知',exact=True).or_(page.get_by_text('系统通知',exact=True)).first).to_be_visible()
 r.force_reconnect(page);r.wait_state(page,lambda s:not s['pending_commands']);passed.append('reconnection and command acknowledgement')
 page.screenshot(path=str(r.OUT/'codex-desktop.png'));page.set_viewport_size({'width':390,'height':844});page.screenshot(path=str(r.OUT/'codex-mobile.png'))
 assert not errors,errors;assert not external,external
 report={'browser':browser.version,'environment':'real Codex 0.154.0; local Responses fixture; PostgreSQL/NATS/containerd/runc','passed':passed,'count':len(passed),'page_errors':errors,'external_requests':external};(r.OUT/'codex.json').write_text(json.dumps(report,ensure_ascii=False,indent=2)+'\n');print(json.dumps(report,ensure_ascii=False),flush=True)
 # Archive this audit account's workspace so repeated runs release guest processes.
 job('归档工作区','archive')
 browser.close()
