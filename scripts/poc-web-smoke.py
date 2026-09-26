#!/usr/bin/env python3
"""Run against scripts/dev.sh + scripts/serve-web.py. Requires Playwright Chromium.
Creates one local test account. No credentials are included in the report.
"""
import json
import os
from pathlib import Path
import time
from playwright.sync_api import sync_playwright

root = Path(__file__).resolve().parents[1]
base = os.environ.get('RYZHUB_WEB_URL', 'http://127.0.0.1:3000')

def cache(page):
    values = page.evaluate('''async () => {
      const db = await new Promise((resolve,reject) => {
        const r=indexedDB.open('ryzhub-core');r.onsuccess=()=>resolve(r.result);r.onerror=reject;
      });
      try { return await new Promise((resolve,reject) => {
        const r=db.transaction('cache').objectStore('cache').getAll();
        r.onsuccess=()=>resolve(r.result);r.onerror=reject;
      }); } finally { db.close(); }
    }''')
    return json.loads(values[0]) if values else {}

def wait_cache(page, predicate):
    for _ in range(150):
        state = cache(page)
        if predicate(state): return state
        page.wait_for_timeout(100)
    raise AssertionError('client cache did not reach expected state')

def items(state):
    return [item for channel in state.get('channels', {}).values() for item in channel['items'].values()]

def text(page, x, y, value):
    page.mouse.click(x, y)
    # Flutter focuses its real editing element on the next frame; filling the
    # semantics mirror directly does not update TextEditingController.
    page.wait_for_timeout(300)
    page.keyboard.insert_text(value)

with sync_playwright() as p:
    browser = p.chromium.launch(headless=True)
    page = browser.new_page(viewport={'width': 1280, 'height': 900})
    errors, external = [], []
    page.on('pageerror', lambda error: errors.append(str(error)))
    page.on('request', lambda r: external.append(r.url) if not r.url.startswith((base, 'http://127.0.0.1:8080', 'blob:', 'data:')) else None)
    page.goto(base)
    page.wait_for_selector('flutter-view')
    page.wait_for_timeout(1500)
    assert page.evaluate('crossOriginIsolated')
    text(page, 560, 487, f'poc-{time.time_ns()}@example.test')
    text(page, 560, 550, 'local-poc-password')
    with page.expect_response(lambda r: r.url.endswith('/auth/register')) as registration:
        page.mouse.click(630, 617)
    assert registration.value.status == 200
    page.wait_for_timeout(1800)
    text(page, 590, 815, '浏览器联调：你好 Rust')
    page.keyboard.press('Control+Enter')
    wait_cache(page, lambda s: any(i['kind']=='assistant' and i['state']=='completed' and '浏览器联调' in i['text'] for i in items(s)))
    text(page, 590, 815, '/approve 浏览器审批')
    page.keyboard.press('Control+Enter')
    wait_cache(page, lambda s: any(a['status']=='pending' for c in s.get('channels',{}).values() for a in c['actions'].values()))
    page.locator('flt-semantics-placeholder').evaluate('(el) => el.click()')
    page.get_by_role('button', name='允许', exact=True).click()
    state = wait_cache(page, lambda s: sum(i['kind']=='assistant' and i['state']=='completed' for i in items(s)) == 2)
    assert not any(c['waiting_events'] for c in state['channels'].values() if 'waiting_events' in c)
    # Same session reconnect: rebuild the stream from locally persisted cursor.
    page.get_by_role('button', name='重新连接', exact=True).click()
    page.wait_for_timeout(1800)
    assert sum(i['kind']=='assistant' and i['state']=='completed' for i in items(cache(page))) == 2
    assert not errors, errors
    assert not external, external
    page.screenshot(path=str(root/'docs/poc-web.png'))
    metrics = page.evaluate('''() => ({
      paints: performance.getEntriesByType('paint').map(e=>({name:e.name,ms:e.startTime})),
      resources: performance.getEntriesByType('resource').filter(e=>e.initiatorType!=='fetch' || /\\.(wasm|ttf|woff2)/.test(e.name)).map(e=>({name:new URL(e.name).pathname,bytes:e.encodedBodySize,duration_ms:e.duration})),
      cross_origin_isolated:crossOriginIsolated
    })''')
    metrics.update({'browser':browser.version,'viewport':'1280x900','environment':'Linux headless Chromium, localhost, cold browser context, uncompressed Python HTTP server','passed':['registration','Rust/Wasm protobuf stream','IndexedDB projection','approval','reconnect','no external fonts/assets']})
    (root/'docs/poc-web-metrics.json').write_text(json.dumps(metrics,ensure_ascii=False,indent=2)+'\n')
    print(json.dumps({'passed':metrics['passed'],'paints':metrics['paints']},ensure_ascii=False))
    browser.close()
