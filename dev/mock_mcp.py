#!/usr/bin/env python3
"""Minimal stdio MCP server for the isolated Settings protocol smoke test."""
import json
import sys

for line in sys.stdin:
    request = json.loads(line)
    if 'id' not in request:
        continue
    method = request.get('method')
    if method == 'initialize':
        result = {
            'protocolVersion': request.get('params', {}).get('protocolVersion', '2024-11-05'),
            'capabilities': {'tools': {}},
            'serverInfo': {'name': 'ocdesk-settings-smoke', 'version': '1.0.0'},
        }
    elif method == 'tools/list':
        result = {'tools': []}
    elif method == 'resources/list':
        result = {'resources': []}
    elif method == 'prompts/list':
        result = {'prompts': []}
    elif method == 'ping':
        result = {}
    else:
        print(json.dumps({'jsonrpc': '2.0', 'id': request['id'], 'error': {'code': -32601, 'message': 'Method not found'}}), flush=True)
        continue
    print(json.dumps({'jsonrpc': '2.0', 'id': request['id'], 'result': result}), flush=True)
