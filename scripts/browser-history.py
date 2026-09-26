#!/usr/bin/env python3
"""History pagination against a fresh test DB and the release Rust/WASM client.

Run with RYZHUB_HISTORY_TEST_DB=ryzhub_test_...; uses the local Compose PostgreSQL.
"""
import importlib.util
import json
import os
import re
import subprocess
import time
from pathlib import Path
from uuid import UUID

from playwright.sync_api import sync_playwright, expect

spec = importlib.util.spec_from_file_location('regression', Path(__file__).with_name('browser-regression.py'))
r = importlib.util.module_from_spec(spec)
spec.loader.exec_module(r)
database = os.environ['RYZHUB_HISTORY_TEST_DB']
assert re.fullmatch(r'ryzhub_test_[a-z0-9_]+', database), 'requires an isolated test database'
with sync_playwright() as p:
    browser = p.chromium.launch(headless=True)
    context = browser.new_context(viewport={'width': 1280, 'height': 1000})
    email, password = f'history-{time.time_ns()}@example.test', 'history-test-password'
    auth = context.request.post(f'{r.API}/auth/register', data={
        'email': email, 'password': password,
    }).json()
    org = str(UUID(auth['org_id']))
    created = context.request.post(f'{r.API}/orgs/{org}/conversations',
        headers={'Authorization': 'Bearer ' + auth['token']},
        data={'title': '分页回归', 'mode': 'chat'}).json()
    conv = str(UUID(created['id']))
    # Seed only this test conversation, before any device subscribes.
    sql = f"""BEGIN;
    INSERT INTO items(id,org_id,conversation_id,revision,kind,state,text,first_seq,last_seq)
    SELECT uuidv7(),'{org}','{conv}',1,'assistant','completed','历史消息 '||n,n,n
    FROM generate_series(1,225) AS n;
    UPDATE channels SET last_seq=225 WHERE org_id='{org}' AND conversation_id='{conv}';
    COMMIT;"""
    subprocess.run(['docker', 'compose', 'exec', '-T', 'postgres', 'psql', '-U', 'postgres',
        '-d', database, '-v', 'ON_ERROR_STOP=1', '-c', sql], check=True, stdout=subprocess.DEVNULL)
    errors = []
    page = r.start(context)
    page.on('pageerror', lambda error: errors.append(str(error)))
    r.login(page, email, password)
    channel = f'conv:{conv}'
    state = r.wait_state(page, lambda s: len(s.get('channels', {}).get(channel, {}).get('items', {})) == 100)
    assert state['channels'][channel]['cursor'] == 225
    assert min(i['first_seq'] for i in state['channels'][channel]['items'].values()) == 126
    page.get_by_role('button', name='加载更早消息', exact=True).click()
    state = r.wait_state(page, lambda s: len(s['channels'][channel]['items']) == 200)
    assert state['channels'][channel]['cursor'] == 225
    page.get_by_role('button', name='加载更早消息', exact=True).click()
    state = r.wait_state(page, lambda s: len(s['channels'][channel]['items']) == 225)
    assert sorted(i['first_seq'] for i in state['channels'][channel]['items'].values()) == list(range(1,226))
    expect(page.get_by_role('button', name='已显示全部消息', exact=True)).to_be_disabled()
    page.reload()
    page.locator('flt-semantics-placeholder').wait_for()
    page.locator('flt-semantics-placeholder').evaluate('(e) => e.click()')
    r.login(page, email, password)
    state = r.wait_state(page, lambda s: len(s['channels'][channel]['items']) == 225)
    assert state['channels'][channel]['cursor'] == 225
    r.send(page, '分页后继续对话')
    r.wait_state(page, lambda s: any(i['kind']=='assistant' and i['state']=='completed' and '分页后继续对话' in i['text'] for i in r.items(s)))
    assert not errors, errors
    report = {'browser': browser.version, 'count': 4, 'passed': [
        'fresh device receives latest 100 items', 'two older pages load all 225 without moving cursor',
        'reload preserves complete cached history', 'live messages continue after pagination'], 'page_errors': errors}
    (r.ROOT/'docs/browser-history.json').write_text(json.dumps(report, ensure_ascii=False, indent=2)+'\n')
    print(json.dumps(report, ensure_ascii=False))
    browser.close()
