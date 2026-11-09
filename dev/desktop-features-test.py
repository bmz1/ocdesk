#!/usr/bin/env python3
"""Offline 2.2.6 contract tests for attachments, permission modes, review and usage.
Uses a temporary Git repo, mock provider and isolated config/data. No credentials.
Usage: python3 dev/desktop-features-test.py /path/to/opencode2
"""
import base64
import json
import os
from pathlib import Path
import subprocess
import sys
import time
import urllib.parse
import urllib.error
from harness import Harness


def check_saved_approvals_in_another_project(h, supervised):
    # The desktop sidecar starts in HOME, not in the session's project.
    project = h.work / 'other-project'
    project.mkdir()
    git = ['git', '-C', str(project)]
    subprocess.run(git + ['init', '-q'], check=True)
    (project / 'README.md').write_text('A distinct project for saved approvals\n')
    subprocess.run(git + ['add', '.'], check=True)
    subprocess.run(git + ['-c', 'user.name=QA', '-c', 'user.email=qa@example.com',
                          'commit', '-qm', 'other project'], check=True)
    sid = h.new_session(project)
    root = f'/api/session/{sid}'
    h.request(root, {'permissions': supervised}, 'PATCH')
    h.request(root + '/prompt', {'text': 'tool:bash please'})
    asked = h.wait_event('permission.asked', sid)['data']
    assert asked['save'], 'the real shell tool must propose saved patterns'
    h.request(root + f"/permission/{asked['id']}/reply", {'decision': 'always'})
    assert h.wait_event('terminal', sid)['type'] == 'session.execution.succeeded'

    project_id = h.request('/api/location?' + urllib.parse.urlencode({
        'location[directory]': str(project)}))['project']['id']
    query = urllib.parse.urlencode({'projectID': project_id})
    grants = h.request('/api/permission/saved?' + query)['data']
    assert {g['resource'] for g in grants} == set(asked['save'])
    assert all(g['projectID'] == project_id and g['action'] == asked['action'] for g in grants)
    assert h.request('/api/permission/saved')['data'] == [], 'unscoped reads miss the session project'

    # Saved rules apply to later real tool calls and to new sessions in that project.
    h.request(root + '/prompt', {'text': 'tool:bash again'})
    assert h.wait_event('terminal', sid)['type'] == 'session.execution.succeeded'
    next_root = f'/api/session/{h.new_session(project)}'
    h.request(next_root, {'permissions': supervised}, 'PATCH')
    for resource in asked['resources']:
        decision = h.request(next_root + '/permission', {
            'action': asked['action'], 'resources': [resource]})['data']
        assert decision['effect'] == 'allow', (resource, decision)
    for grant in grants:
        h.request('/api/permission/saved/' + grant['id'], method='DELETE')
    assert h.request('/api/permission/saved?' + query)['data'] == []
    pending = h.request(next_root + '/permission', {
        'action': asked['action'], 'resources': asked['resources']})['data']
    assert pending['effect'] == 'ask', 'revocation must restore approval prompts'
    h.request(next_root + f"/permission/{pending['id']}/reply", {'decision': 'reject'})
    print('PASS: real Always allow saves project grants outside the sidecar directory; reuse and revoke')


def check_archive_hides_sessions(h):
    """Archive must hide a session from the list ocdesk reads, including one
    whose first turn failed. The plain list keeps archived sessions."""
    ok = h.new_session()
    h.request(f'/api/session/{ok}/prompt', {'text': 'hi'})
    assert h.wait_event('terminal', ok)['type'] == 'session.execution.succeeded'
    failed = h.new_session()
    h.request(f'/api/session/{failed}/model', {'model': {'providerID': 'mock', 'id': 'missing-model'}})
    h.request(f'/api/session/{failed}/prompt', {'text': 'hi'})
    assert h.wait_event('terminal', failed)['type'] == 'session.execution.failed'
    def listed(query):
        return {s['id'] for s in h.request('/api/session?limit=50&order=desc' + query)['data']}
    for sid in (ok, failed):
        assert h.request(f'/api/session/{sid}', {'archived': True}, 'PATCH') is None
        h.wait_event('session.archived', sid)
        assert h.request(f'/api/session/{sid}')['data']['time'].get('archived'), sid
    assert {ok, failed} <= listed(''), 'the default list includes archived sessions'
    assert not {ok, failed} & listed('&archived=false'), 'archived=false hides them'
    h.request(f'/api/session/{failed}', {'archived': False}, 'PATCH')
    assert failed in listed('&archived=false'), 'restoring lists it again'
    print('PASS: archive hides succeeded and failed-first-turn sessions from archived=false; restore lists again')


def check_streaming_block_has_no_history_text(h):
    """ocdesk marks blocks joined mid-stream because history has no text for
    a block that is still streaming. If this changes, revisit missed_starts."""
    sid = h.new_session()
    h.request(f'/api/session/{sid}/prompt', {'text': 'slow'})
    h.wait_event('session.text.delta', sid)
    running = [m for m in h.request(f'/api/session/{sid}/message')['data'] if m['type'] == 'assistant']
    assert running and 'completed' not in running[0]['time'], running
    texts = [c['text'] for c in running[0]['content'] if c['type'] == 'text']
    assert texts == [''], f'a streaming text block has no history text: {texts}'
    assert h.wait_event('terminal', sid, timeout=60)['type'] == 'session.execution.succeeded'
    print('PASS: a streaming text block has empty history text until it ends')


def main():
    binary=sys.argv[1] if len(sys.argv)>1 else os.environ.get('OCDESK_OPENCODE','opencode2')
    with Harness(binary,files={'file space.txt':'attachment probe\n'}) as h:
        check_archive_hides_sessions(h)
        check_streaming_block_has_no_history_text(h)
        sid=h.new_session();root=f'/api/session/{sid}'
        supervised=[{'action':'*','resource':'*','effect':'ask'}]+[{'action':a,'resource':'*','effect':'allow'} for a in ['read','glob','grep']]
        auto=supervised+[{'action':'edit','resource':'*','effect':'allow'}]
        full=[{'action':'*','resource':'*','effect':'allow'}]
        def decide(action,resource):
            return h.request(root+'/permission',{'action':action,'resources':[resource],'save':[resource]})['data']
        def reject(v):
            if v['effect']=='ask':h.request(root+f"/permission/{v['id']}/reply",{'decision':'reject'})
        h.request(root,{'permissions':supervised},'PATCH')
        assert h.request(root)['data']['permissions']==supervised
        v=decide('shell','echo saved');assert v['effect']=='ask'
        h.request(root+f"/permission/{v['id']}/reply",{'decision':'always'})
        grants=h.request('/api/permission/saved')['data'];assert len(grants)==1
        assert decide('shell','echo saved')['effect']=='allow','saved project grants precede session ask rules'
        h.request('/api/permission/saved/'+grants[0]['id'],method='DELETE')
        v=decide('shell','echo saved');assert v['effect']=='ask';reject(v)
        check_saved_approvals_in_another_project(h, supervised)
        h.request(root,{'permissions':auto},'PATCH')
        assert decide('edit',str(h.project/'note.txt'))['effect']=='allow'
        v=decide('shell','echo other');assert v['effect']=='ask';reject(v)
        h.request(root,{'permissions':full},'PATCH')
        assert decide('shell','echo other')['effect']=='allow'
        h.request(root,{'permissions':supervised},'PATCH')
        uri=(h.project/'file space.txt').as_uri()
        h.request(root+'/prompt',{'text':'','files':[{'uri':uri,'name':'file space.txt'}]})
        assert h.wait_event('terminal',sid)['type']=='session.execution.succeeded'
        messages=h.request(root+'/message?limit=200&order=asc')['data']
        user=next(m for m in messages if m['type']=='user')
        assert base64.b64decode(user['files'][0]['data'])==b'attachment probe\n'
        assert user['files'][0]['source']['uri']==uri
        image=h.project/'pasted-image.png'
        image.write_bytes(base64.b64decode('iVBORw0KGgoAAAANSUhEUgAAAAIAAAACCAYAAABytg0kAAAAEUlEQVR4nGP4z8DwH4QZYAwAR8oH+WdZbrcAAAAASUVORK5CYII='))
        h.request(root+'/prompt',{'text':'image attachment','files':[{'uri':image.as_uri(),'name':image.name}]})
        assert h.wait_event('terminal',sid)['type']=='session.execution.succeeded'
        messages=h.request(root+'/message?limit=200&order=asc')['data']
        user=next(m for m in messages if m['type']=='user' and m.get('files',[{}])[0].get('name')==image.name)
        assert user['files'][0]['mime']=='image/png'
        assert base64.b64decode(user['files'][0]['data'])==image.read_bytes()
        # Up to 100 real files, without mention offsets, including spaces in URIs.
        files=[]
        for i in range(100):
            p=h.project/f'file {i}.txt';p.write_text(f'file {i}\n');files.append({'uri':p.as_uri(),'name':p.name})
        h.request(root+'/prompt',{'text':'100 attachments','files':files})
        assert h.wait_event('terminal',sid)['type']=='session.execution.succeeded'
        messages=h.request(root+'/message?limit=200&order=asc')['data']
        assert any(m['type']=='user' and len(m.get('files',[]))==100 for m in messages)
        git=['git','-C',str(h.project)]
        subprocess.run(git+['checkout','-qb','feature'],check=True)
        (h.project/'README.md').write_text('branch change\n')
        subprocess.run(git+['add','README.md'],check=True)
        subprocess.run(git+['-c','user.name=QA','-c','user.email=qa@example.com','commit','-qm','review commit'],check=True)
        (h.project/'README.md').write_text('working change\n')
        def diff(mode):
            q=urllib.parse.urlencode({'location[directory]':str(h.project),'mode':mode,'base':'main','context':'3','untracked':'files'})
            return h.request('/api/vcs/diff?'+q)['data']
        working=diff('working');branch=diff('branch');committed=diff('committed')
        assert any('working change' in f['patch'] and 'branch change' in f['patch'] for f in working)
        assert any('working change' in f['patch'] and 'harness' in f['patch'] for f in branch)
        assert any('branch change' in f['patch'] and 'harness' in f['patch'] for f in committed)
        assert any(f['file']=='file 0.txt' for f in working)
        q=urllib.parse.urlencode({'location[directory]':str(h.project)})
        assert h.request('/api/vcs/status?'+q)['data']
        info=h.request(root)['data'];assert info['tokens']['input']>0
        context=h.request(root+'/context')['data'];assistant=next(m for m in reversed(context) if m['type']=='assistant')
        models=h.request('/api/model?'+q)['data']
        model=next(m for m in models if m['id']==assistant['model']['id'] and m['providerID']==assistant['model']['providerID'])
        assert model['limit']['context']>0
        updates=[e for e in h.seen if e['type']=='session.usage.updated' and e['data']['sessionID']==sid]
        assert updates and updates[-1]['data']['tokens']==info['tokens']
        assert 'data' in h.request('/api/experimental/usage/limits?'+q)
        # Settings edits the real JSONC file; a plain new session must inherit it.
        config_file = h.project / 'opencode.jsonc'
        config_file.write_text('''{
  // Preserve this comment when editing in Settings.
  "default_agent": "plan",
  "permissions": [{"action": "shell", "resource": "*", "effect": "ask"}],
}
''')
        h.request('/api/location/reload', {}, 'POST')
        sources = h.request('/api/config?' + q)
        assert any(entry.get('info', {}).get('default_agent') == 'plan' for entry in sources)
        inherited_root = f'/api/session/{h.new_session()}'
        assert not h.request(inherited_root)['data'].get('permissions'), 'new sessions must not need an ocdesk permission preset'
        h.request(inherited_root + '/prompt', {'text': 'tool:bash config inheritance'})
        pending = h.wait_event('permission.asked', inherited_root.rsplit('/', 1)[-1])['data']
        assert pending['action'] == 'shell', pending
        h.request(inherited_root + f"/permission/{pending['id']}/reply", {'decision': 'reject'})
        h.wait_event('terminal', inherited_root.rsplit('/', 1)[-1])
        print('PASS: real JSONC config discovery, reload, default agent and inherited permissions')
        config_file.write_text(json.dumps({'mcp': {'servers': {'settings-probe': {
            'type': 'local', 'command': [sys.executable, str(Path(__file__).with_name('mock_mcp.py').resolve())],
        }}}}))
        h.request('/api/location/reload', {}, 'POST')
        def mcp_status(expected):
            until = time.monotonic() + 10
            while time.monotonic() < until:
                servers = h.request('/api/mcp?' + q)['data']
                server = next((s for s in servers if s['name'] == 'settings-probe'), None)
                if server and (server['status']['status'] == 'connected') == expected:
                    return server
                time.sleep(.1)
            raise AssertionError(f'MCP connection did not settle: {servers}')
        mcp_status(True)
        h.request('/api/experimental/mcp/settings-probe/disconnect?' + q, {}, 'POST')
        mcp_status(False)
        h.request('/api/experimental/mcp/settings-probe/connect?' + q, {}, 'POST')
        mcp_status(True)
        print('PASS: real MCP server discovery, connect and disconnect in the selected project')
        print('PASS: attachment-only, encoded URI, 100 files, permissions, saved grants/revoke, working/branch/committed diffs, usage/context/limits')

if __name__=='__main__':main()
