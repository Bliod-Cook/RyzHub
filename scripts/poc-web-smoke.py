#!/usr/bin/env python3
"""Run against scripts/dev.sh + scripts/serve-web.py (RYZHUB_API_URL, default :8080). Requires Playwright Chromium.
Creates one local test account. No credentials are included in the report.
"""
import importlib.util
import json
from pathlib import Path
import time
from playwright.sync_api import sync_playwright

# Shares the semantics-based helpers; fixed click coordinates break on layout changes.
spec = importlib.util.spec_from_file_location('regression', Path(__file__).with_name('browser-regression.py'))
r = importlib.util.module_from_spec(spec)
spec.loader.exec_module(r)
root = r.ROOT

with sync_playwright() as p:
    browser = p.chromium.launch(headless=True)
    context = browser.new_context(viewport={'width': 1280, 'height': 900}, locale='zh-CN')
    r.track_sockets(context)
    errors, external = [], []
    context.on('page', lambda page: page.on('pageerror', lambda error: errors.append(str(error))))
    context.on('request', lambda q: external.append(q.url) if not q.url.startswith((r.BASE, r.API, 'blob:', 'data:')) else None)
    page = r.start(context)
    assert page.evaluate('crossOriginIsolated')
    r.login(page, f'poc-{time.time_ns()}@example.test', 'local-poc-password', register=True)
    r.create(page, '浏览器联调：你好 Rust')
    r.wait_state(page, lambda s: any(i['kind']=='assistant' and i['state']=='completed' and '浏览器联调' in i['text'] for i in r.items(s)))
    r.send(page, '/approve 浏览器审批')
    r.wait_state(page, lambda s: any(a['status']=='pending' for a in r.actions(s)))
    page.get_by_role('button', name='允许', exact=True).click()
    state = r.wait_state(page, lambda s: sum(i['kind']=='assistant' and i['state']=='completed' for i in r.items(s)) == 2)
    assert not any(c['waiting_events'] for c in state['channels'].values() if 'waiting_events' in c)
    # Same session reconnect: rebuild the stream from locally persisted cursor.
    r.force_reconnect(page)
    page.wait_for_timeout(1800)
    assert sum(i['kind']=='assistant' and i['state']=='completed' for i in r.items(r.cache(page))) == 2
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
