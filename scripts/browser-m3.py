#!/usr/bin/env python3
"""Release Flutter/WASM + real Chromium + actual SDK sandbox, local protocol upstreams."""
import os,importlib.util,json,time
from pathlib import Path
from playwright.sync_api import sync_playwright,expect
os.environ.setdefault('RYZHUB_API_URL','http://127.0.0.1:8280')
spec=importlib.util.spec_from_file_location('r',Path(__file__).with_name('browser-regression.py'));r=importlib.util.module_from_spec(spec);spec.loader.exec_module(r)
with sync_playwright() as p:
 browser=p.chromium.launch(headless=True);context=browser.new_context(viewport={'width':1360,'height':1000},permissions=['clipboard-read','clipboard-write'],locale='zh-CN');errors=[];external=[];passed=[]
 context.on('page',lambda page:page.on('pageerror',lambda e:errors.append(str(e))))
 context.on('page',lambda page:page.on('console',lambda m:errors.append({'text':m.text,'location':m.location}) if m.type=='error' else None))
 context.on('response',lambda res:print('HTTP',res.status,res.url,flush=True) if res.status>=400 else None)
 context.on('request',lambda req:external.append(req.url) if not req.url.startswith((r.BASE,r.API,'blob:','data:')) else None)
 page=r.start(context);auth=r.login(page,f'm3-browser-{time.time_ns()}@example.test','local-m3-browser',True);passed.append('registration and Rust/WASM websocket connection')
 def api(path):return context.request.get(r.API+path,headers={'Authorization':'Bearer '+auth['token']}).json()
 def choose(label,value):page.get_by_role('button',name=label,exact=False).click();page.get_by_role('menuitem',name=value,exact=True).click()
 def menu_item(name):return page.get_by_role('menuitem',name=name,exact=True).or_(page.get_by_role('button',name=name,exact=True)).first
 # One model picker; in Work a harness row opens a flyout with its models on
 # hover, and clicking a model row selects it (its levels fly out beside it).
 def pick_model(model,harness=None):
  page.get_by_role('button',name='模型',exact=False).click()
  if harness:menu_item(harness).hover()
  menu_item(model).click()
 def create(text):return r.create(page,text)['id']
 def done(conv,previous=None):
  def check(s):
   c=s.get('channels',{}).get('conv:'+conv,{})
   return any(i['kind']=='assistant' and i['state']=='completed' and i['id'] not in (previous or set()) for i in c.get('items',{}).values()) and not any(rn['state'] in ('running','requires_action') for rn in c.get('runs',{}).values())
  return r.wait_state(page,check)
 def previous(conv):return set(r.cache(page).get('channels',{}).get('conv:'+conv,{}).get('items',{}))
 def workspace(conv,kind):
  for _ in range(100):
   result=context.request.post(f'{r.API}/orgs/{auth["org_id"]}/conversations/{conv}/workspace',headers={'Authorization':'Bearer '+auth['token']},data={'kind':kind})
   if result.status==200:break
   assert result.status==409 and result.json()['error']=='WORKSPACE_BUSY',result.text()
   page.wait_for_timeout(200)
  assert result.status==200,result.text();job=result.json()['id']
  deadline=time.monotonic()+90
  while time.monotonic()<deadline:
   state=api(f'/orgs/{auth["org_id"]}/conversations/{conv}/workspace');jobs=[j for j in state['jobs'] if j['id']==job]
   if jobs and jobs[0]['state']=='completed':return
   assert not jobs or jobs[0]['state']!='failed',jobs
   page.wait_for_timeout(200)
  raise AssertionError('workspace timeout')
 try:
  assert page.get_by_role('checkbox',name='计划模式').count()==0
  page.get_by_role('button',name='新建项目',exact=True).click()
  r.enter(page,'项目名称','浏览器项目');r.enter(page,'项目指令','回答使用简体中文。PROJECT_BROWSER')
  choose('记忆边界','仅此项目')
  with page.expect_response(lambda res:'/projects' in res.url and res.request.method=='POST') as project_response:page.get_by_role('button',name='保存项目',exact=True).click()
  project=project_response.value.json()['id'];expect(page.get_by_role('textbox',name='文件名',exact=True)).to_be_visible()
  r.enter(page,'文件名','knowledge.txt');r.enter(page,'文件内容','项目答案为 42。')
  page.get_by_role('button',name='保存知识文件',exact=True).click();expect(page.get_by_role('button',name='knowledge.txt',exact=False)).to_be_visible()
  page.get_by_role('button',name='完成',exact=True).click();passed.append('private project, immutable memory mode and knowledge file editor')
  page.get_by_role('button',name='浏览器项目',exact=True).click();expect(page.get_by_role('heading',name='浏览器项目',exact=True)).to_be_visible()
  # Only the model is chosen; the server derives the Responses wire format from it.
  pick_model('gpt-5.4');chat=create('hello responses browser')
  assert api(f'/orgs/{auth["org_id"]}/conversations/{chat}/commands')['settings']['protocol']=='responses'
  assert next(c for c in api(f'/orgs/{auth["org_id"]}/conversations') if c['id']==chat)['project_id']==project
  done(chat);passed.append('Responses chat through release Flutter and native loop')
  # The approval mode left the composer (it is meant to return as a command);
  # the catalog still reflects the organization policy.
  assert 'full_auto' not in api(f'/orgs/{auth["org_id"]}/conversations/{chat}/commands')['approval_modes'];passed.append('organization policy hides full auto')
  page.get_by_role('button',name='对话命令',exact=True).click();page.get_by_role('menuitem',name='压缩上下文',exact=True).click()
  r.wait_state(page,lambda s:any(i['text']=='上下文已压缩' for i in r.items(s)));passed.append('compact command with completion event')
  page.get_by_role('button',name='对话命令',exact=True).click();page.get_by_role('menuitem',name='重命名',exact=True).click();r.enter(page,'重命名对话','M3 浏览器对话');page.get_by_role('button',name='保存',exact=True).click();expect(page.get_by_role('heading',name='M3 浏览器对话')).to_be_visible();passed.append('rename platform command')
  page.get_by_role('button',name='对话命令',exact=True).click();page.get_by_role('menuitem',name='导出到剪贴板',exact=True).click();page.wait_for_timeout(400);assert 'M3 浏览器对话' in page.evaluate('navigator.clipboard.readText()');passed.append('export visible conversation')
  # Switching the header to Work opens a Work draft in the same project.
  pick_model('claude-opus-4-6');expect(page.get_by_role('heading',name='浏览器项目',exact=True)).to_be_visible();expect(page.get_by_role('button',name='claude-opus-4-6',exact=False)).to_be_visible();passed.append('another provider opens a new chat with that model')
  page.get_by_role('button',name='Work',exact=True).click();pick_model('claude-sonnet-4-6','Claude Code');work=create('slow hello claude browser')
  expect(page.get_by_text('补充指令排入下一轮',exact=True)).to_be_visible();passed.append('Claude capability and emulated steer disclosure')
  r.wait_state(page,lambda s:any(rn['state']=='running' for rn in s.get('channels',{}).get('conv:'+work,{}).get('runs',{}).values()))
  r.send(page,'这是排入下一轮的补充。');r.wait_state(page,lambda s:any(i['kind']=='steer' and i['state']=='waiting' for i in r.items(s)))
  done(work);r.wait_state(page,lambda s:any(i['kind']=='steer' and i['state']=='handed' for i in r.items(s)));passed.append('queued Claude steer waits then starts the next turn')
  prev=previous(work);r.send(page,'fixture-exec');r.wait_state(page,lambda s:any(a['status']=='pending' for a in r.actions(s)));page.get_by_role('button',name='允许',exact=True).click();done(work,prev);passed.append('actual SDK Bash approval and execution')
  prev=previous(work);r.send(page,'fixture-question');r.wait_state(page,lambda s:any(a['status']=='pending' and '__answer__' in a['options'] for a in r.actions(s)))
  # QuestionCard renders a text answer; no approval buttons should be present.
  assert page.get_by_role('button',name='允许',exact=True).count()==0
  expect(page.get_by_text('你希望使用哪种颜色？',exact=True)).to_be_visible()
  page.get_by_role('button',name='蓝色',exact=True).click()
  # Flutter synchronizes the editable DOM input when it receives focus.
  page.get_by_role('textbox',name='回答',exact=True).click();expect(page.get_by_role('textbox',name='回答',exact=True)).to_have_value('蓝色')
  page.screenshot(path=str(r.ROOT/'docs/browser-m3-question.png'))
  page.set_viewport_size({'width':390,'height':844});page.screenshot(path=str(r.ROOT/'docs/browser-m3-question-mobile.png'));page.set_viewport_size({'width':1360,'height':1000})
  page.get_by_role('button',name='提交回答',exact=True).click();done(work,prev);passed.append('SDK question renders readable prompt and choices, then resolves')
  pick_model('claude-opus-4-6','Claude Code');prev=previous(work);r.send(page,'model switch browser');done(work,prev);passed.append('SDK model switch through the model picker')
  # Settings changes are silent; the command catalog confirms them.
  page.get_by_role('checkbox',name='计划模式',exact=True).click();deadline=time.monotonic()+15
  while not api(f'/orgs/{auth["org_id"]}/conversations/{work}/commands')['settings']['plan']:
   assert time.monotonic()<deadline,'plan mode was not saved';page.wait_for_timeout(200)
  expect(page.get_by_role('checkbox',name='计划模式',exact=True)).to_be_checked();passed.append('native plan configuration saved for next turn')
  page.screenshot(path=str(r.ROOT/'docs/browser-m3-desktop.png'))
  page.set_viewport_size({'width':390,'height':844});page.screenshot(path=str(r.ROOT/'docs/browser-m3-mobile.png'));page.set_viewport_size({'width':1360,'height':1000});passed.append('desktop and mobile layouts')
  workspace(work,'archive');passed.append('browser sandbox archived after acceptance')
  assert not errors,errors;assert not external,external
  (r.ROOT/'docs/browser-m3.json').write_text(json.dumps({'passed':passed,'count':len(passed),'page_errors':errors,'external_requests':external},ensure_ascii=False,indent=2)+'\n');print(json.dumps({'passed':len(passed),'page_errors':errors},ensure_ascii=False))
 except Exception:
  page.screenshot(path=str(r.ROOT/'var/m3-review/browser-m3-failure.png'));(r.ROOT/'var/m3-review/browser-m3-accessibility.txt').write_text(page.locator('body').aria_snapshot());raise
 finally:browser.close()
