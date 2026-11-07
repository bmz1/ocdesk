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
import urllib.parse
import urllib.error
from harness import Harness


def main():
    binary=sys.argv[1] if len(sys.argv)>1 else os.environ.get('OCDESK_OPENCODE','opencode2')
    with Harness(binary,files={'file space.txt':'attachment probe\n'}) as h:
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
        print('PASS: attachment-only, encoded URI, 100 files, permissions, saved grants/revoke, working/branch/committed diffs, usage/context/limits')

if __name__=='__main__':main()
