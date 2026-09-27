#!/usr/bin/env python3
"""Browser acceptance for dev-m1.sh --fixture and the release Web bundle."""
import importlib.util
import json
from pathlib import Path
import time
from playwright.sync_api import sync_playwright, expect

spec=importlib.util.spec_from_file_location('regression',Path(__file__).with_name('browser-regression.py'))
r=importlib.util.module_from_spec(spec)
spec.loader.exec_module(r)

with sync_playwright() as p:
    browser=p.chromium.launch(headless=True)
    context=browser.new_context(viewport={'width':1280,'height':900},locale='zh-CN');r.track_sockets(context)
    errors=[]
    external=[]
    context.on('request',lambda request:external.append(request.url) if not request.url.startswith((r.BASE,r.API,'blob:','data:')) else None)
    context.on('page',lambda page:page.on('pageerror',lambda error:errors.append(str(error))))
    page=r.start(context)
    auth=r.login(page,f'm1-browser-{time.time_ns()}@example.test','local-m1-password',register=True)
    passed=['registration']
    r.send(page,'M1 本地协议联调')
    r.wait_state(page,lambda s:any(i['kind']=='assistant' and i['state']=='completed' and 'M1 本地协议' in i['text'] for i in r.items(s)))
    passed.append('Anthropic protocol streaming through Rust/WASM')
    r.send(page,'执行沙箱验收')
    r.wait_state(page,lambda s:any(a['status']=='pending' for a in r.actions(s)))
    page.get_by_role('button',name='允许',exact=True).click()
    r.wait_state(page,lambda s:any(i['kind']=='assistant' and i['state']=='completed' and 'containerd 沙箱验收成功' in i['text'] for i in r.items(s)))
    passed.append('approval and real containerd exec result')
    r.send(page,'请向我提问')
    expect(page.get_by_role('textbox',name='回答',exact=False)).to_be_visible(timeout=15000)
    r.enter(page,'回答','绿色')
    page.get_by_role('button',name='提交回答',exact=True).click()
    r.wait_state(page,lambda s:any(i['kind']=='assistant' and i['state']=='completed' and '绿色' in i['text'] for i in r.items(s)))
    passed.append('persisted question and answer')
    r.send(page,'slow 慢速响应')
    expect(page.get_by_role('button',name='停止',exact=True)).to_be_visible()
    r.send(page,'请采用这条补充信息')
    r.wait_state(page,lambda s:any(i['kind']=='steer' and i['state']=='handed' for i in r.items(s)))
    r.wait_state(page,lambda s:all(run['state'] not in ('running','requires_action') for c in s['channels'].values() for run in c['runs'].values()))
    passed.append('steer consumed at model boundary')
    # Side questions left the composer; they are meant to return as a command.
    r.send(page,'后续主线消息')
    r.wait_state(page,lambda s:any(i['kind']=='assistant' and i['state']=='completed' and '后续主线消息' in i['text'] for i in r.items(s)))
    passed.append('follow-up message after steer')
    r.force_reconnect(page)
    page.wait_for_timeout(1000)
    state=r.cache(page)
    assert not state['pending_commands']
    passed.append('reconnect with no pending commands')
    usage=context.request.get(f'{r.API}/orgs/{auth["org_id"]}/usage',headers={'Authorization':'Bearer '+auth['token']})
    assert usage.status==200 and len(usage.json())>=6
    assert all(u['status']=='completed' and not u['estimated'] for u in usage.json())
    passed.append('usage attribution for browser-triggered model requests')
    page.screenshot(path=str(r.ROOT/'docs/browser-m1-desktop.png'))
    page.set_viewport_size({'width':390,'height':844})
    page.screenshot(path=str(r.ROOT/'docs/browser-m1-mobile.png'))
    assert not errors,errors
    assert not external,external
    report={'browser':browser.version,'environment':'localhost; local Anthropic wire fixture; real PostgreSQL/NATS/containerd/runc; Linux Chromium','passed':passed,'count':len(passed),'page_errors':errors,'external_requests':external}
    (r.ROOT/'docs/browser-m1.json').write_text(json.dumps(report,ensure_ascii=False,indent=2)+'\n')
    print(json.dumps(report,ensure_ascii=False))
    browser.close()
