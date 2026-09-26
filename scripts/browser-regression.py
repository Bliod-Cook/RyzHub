#!/usr/bin/env python3
"""Browser regression against a running M0 server and release Web client.

Creates local test accounts; outputs no credentials. Requires Playwright Chromium.
Run from the RyzHub root: uv run --with playwright python scripts/browser-regression.py
"""
import json
import os
from pathlib import Path
import time
from playwright.sync_api import sync_playwright, expect

ROOT = Path(__file__).resolve().parents[1]
BASE = os.environ.get('RYZHUB_WEB_URL', 'http://127.0.0.1:3000')
API = os.environ.get('RYZHUB_API_URL', 'http://127.0.0.1:8080')


def cache(page):
    return page.evaluate('''async () => {
      const db = await new Promise((resolve,reject) => {
        const r=indexedDB.open('ryzhub-core');
        r.onsuccess=()=>resolve(r.result);r.onerror=reject;
      });
      try { return await new Promise((resolve,reject) => {
        if (!db.objectStoreNames.contains('cache')) return resolve({});
        const r=db.transaction('cache').objectStore('cache').getAll();
        r.onsuccess=()=>resolve(r.result.length ? JSON.parse(r.result[0]) : {});
        r.onerror=reject;
      }); } finally { db.close(); }
    }''')


def wait_state(page, predicate, message='projection did not converge'):
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        state = cache(page)
        if predicate(state):
            return state
        page.wait_for_timeout(100)
    raise AssertionError(message)


def items(state):
    return [i for c in state.get('channels', {}).values() for i in c['items'].values()]


def actions(state):
    return [a for c in state.get('channels', {}).values() for a in c['actions'].values()]


def enter(page, name, value):
    page.get_by_role('textbox', name=name, exact=False).click()
    page.wait_for_timeout(100)
    page.keyboard.press('Control+A')
    page.keyboard.insert_text(value)


def send(page, text):
    enter(page, '写下你的想法', text)
    page.keyboard.press('Control+Enter')


def start(context):
    page = context.new_page()
    page.goto(BASE)
    page.locator('flt-semantics-placeholder').wait_for()
    page.locator('flt-semantics-placeholder').evaluate('(e) => e.click()')
    expect(page.get_by_role('textbox', name='邮箱', exact=True)).to_be_visible()
    return page


def login(page, email, password, register=False):
    if not register:
        page.get_by_role('button', name='已有账号？登录').click()
    enter(page, '服务器地址', API)
    enter(page, '邮箱', email)
    enter(page, '密码', password)
    with page.expect_response(lambda r: r.url.endswith('/auth/register' if register else '/auth/login')) as response:
        page.get_by_role('button', name='创建个人空间' if register else '登录', exact=True).click()
    result = response.value
    assert result.status == 200
    expect(page.get_by_role('button', name='重新连接', exact=True)).to_be_visible()
    wait_state(page, lambda s: any(c.startswith('conv:') for c in s.get('channels', {})))
    return result.json()


def main():
    with sync_playwright() as playwright:
        browser = playwright.chromium.launch(headless=True)
        contexts = []
        errors, external, passed = [], [], []

        def context():
            c = browser.new_context(viewport={'width': 1280, 'height': 900}, permissions=['clipboard-read', 'clipboard-write'])
            c.add_init_script('''window.__sockets=[]; const NativeWS=window.WebSocket;
              window.WebSocket=class extends NativeWS {
                constructor(...args){super(...args);window.__sockets.push(this);}
              };''')
            c.on('page', lambda p: p.on('pageerror', lambda e: errors.append(str(e))))
            c.on('request', lambda r: external.append(r.url) if not r.url.startswith((BASE, API, 'blob:', 'data:')) else None)
            contexts.append(c)
            return c

        primary = context()
        page = start(primary)
        email, password = f'regression-{time.time_ns()}@example.test', 'local-browser-regression'
        auth = login(page, email, password, register=True)
        passed.append('registration through UI')
        send(page, '中文流式测试\n```rust\nprintln!("你好");\n```')
        state = wait_state(page, lambda s: any(i['kind']=='assistant' and i['state']=='completed' for i in items(s)))
        assert sum(i['kind']=='user' for i in items(state)) == 1
        page.get_by_role('button', name='复制代码', exact=True).first.click()
        assert 'println!' in page.evaluate('navigator.clipboard.readText()')
        passed.extend(['Unicode streaming and Markdown', 'code clipboard'])
        page.screenshot(path=str(ROOT/'docs/browser-desktop.png'))

        send(page, '/approve 拒绝测试')
        wait_state(page, lambda s: any(a['status']=='pending' for a in actions(s)))
        page.get_by_role('button', name='拒绝', exact=True).click()
        wait_state(page, lambda s: any(a['status']=='deny' for a in actions(s)))
        passed.append('approval denial')

        second = context()
        other = start(second)
        login(other, email, password)
        passed.append('second device login')
        send(page, '/approve 多设备测试')
        wait_state(other, lambda s: any(a['status']=='pending' for a in actions(s)))
        other.get_by_role('button', name='允许', exact=True).click()
        wait_state(page, lambda s: not any(a['status']=='pending' for a in actions(s)))
        wait_state(page, lambda s: sum(i['kind']=='assistant' and i['state']=='completed' for i in items(s)) == 2)
        passed.append('approval synchronized across devices')

        send(page, '取消测试 ' * 200)
        expect(page.get_by_role('button', name='停止', exact=True)).to_be_visible()
        page.wait_for_timeout(500)
        page.get_by_role('button', name='停止', exact=True).click()
        wait_state(page, lambda s: sum(i['state']=='cancelled' for i in items(s)) >= 2)
        passed.append('cancel during stream')

        page.get_by_role('button', name='Work', exact=True).click()
        page.get_by_role('button', name='新建对话', exact=True).click()
        expect(page.get_by_role('heading', name='新对话 2', exact=True)).to_be_visible()
        send(page, '第二个对话')
        wait_state(page, lambda s: any(i['kind']=='assistant' and i['state']=='completed' and '第二个对话' in i['text'] for i in items(s)))
        page.get_by_role('button', name='新对话 1', exact=True).click()
        expect(page.get_by_role('heading', name='新对话 1', exact=True)).to_be_visible()
        passed.append('conversation create and switch')

        primary.set_offline(True)
        page.evaluate('window.__sockets.forEach(s => s.close())')
        page.wait_for_timeout(500)
        send(page, '离线队列只执行一次')
        wait_state(page, lambda s: bool(s.get('pending_commands')))
        primary.set_offline(False)
        page.get_by_role('button', name='重新连接', exact=True).click()
        # Projection events and command ACKs may arrive in either order.
        state = wait_state(page, lambda s: not s.get('pending_commands') and any(i['kind']=='assistant' and i['state']=='completed' and '离线队列只执行一次' in i['text'] for i in items(s)))
        assert sum(i['kind']=='user' and i['text']=='离线队列只执行一次' for i in items(state)) == 1
        assert not state['pending_commands']
        passed.append('offline persisted queue and reconnect without duplicates')

        serialized = json.dumps(cache(page), ensure_ascii=False)
        assert auth['token'] not in serialized
        assert auth['token'] not in page.evaluate('JSON.stringify(localStorage)')
        assert auth['token'] not in page.evaluate('JSON.stringify(sessionStorage)')
        page.reload()
        page.locator('flt-semantics-placeholder').wait_for()
        page.locator('flt-semantics-placeholder').evaluate('(e) => e.click()')
        login(page, email, password)
        wait_state(page, lambda s: any(i['text']=='离线队列只执行一次' for i in items(s)))
        passed.append('reload and login restore cached projection without persisted access token')

        page.set_viewport_size({'width':390, 'height':844})
        page.get_by_role('button', name='Open navigation menu', exact=True).click()
        expect(page.get_by_role('button', name='新建对话', exact=True)).to_be_visible()
        page.screenshot(path=str(ROOT/'docs/browser-mobile.png'))
        passed.append('390px responsive navigation')

        invalid = start(context())
        invalid.get_by_role('button', name='已有账号？登录').click()
        enter(invalid, '服务器地址', API)
        enter(invalid, '邮箱', email)
        enter(invalid, '密码', 'incorrect-password')
        with invalid.expect_response(lambda r: r.url.endswith('/auth/login')) as response:
            invalid.get_by_role('button', name='登录', exact=True).click()
        assert response.value.status == 401
        expect(invalid.get_by_role('button', name='登录', exact=True)).to_be_visible()
        passed.append('invalid password rejected without entering session')
        assert not errors, errors
        assert not external, external
        passed.append('no uncaught JavaScript errors or external assets')
        report = {'browser':browser.version, 'environment':'Linux headless Chromium, localhost', 'passed':passed, 'count':len(passed), 'page_errors':errors, 'external_requests':external}
        (ROOT/'docs/browser-regression.json').write_text(json.dumps(report,ensure_ascii=False,indent=2)+'\n')
        print(json.dumps(report,ensure_ascii=False))
        for c in contexts:
            c.close()
        browser.close()


if __name__ == "__main__":
    main()
