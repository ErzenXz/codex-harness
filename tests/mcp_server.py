"""Loopback fixture, never a paid model or external MCP service."""
import json
import os
import sys
from pathlib import Path

log=Path(sys.argv[1])
mode=sys.argv[2] if len(sys.argv)>2 else "normal"
for line in sys.stdin:
    request=json.loads(line)
    method=request.get('method','client-response')
    with log.open('a') as f:
        f.write(json.dumps({'method':method,'pid':os.getpid(),'api_key_inherited':'OPENAI_API_KEY' in os.environ,'explicit_env':os.environ.get('MCP_FIXTURE_VALUE')})+'\n')
    if 'id' not in request:
        continue
    if method=='client-response': continue
    if method=='initialize': result={'protocolVersion':'2025-11-25','capabilities':{'tools':{}},'serverInfo':{'name':'fixture','version':'1'}}
    elif method=='tools/list':
        result={'tools':[{'name':name,'description':'Return the provided text','inputSchema':{'type':'object','properties':{'text':{'type':'string'}},'required':['text']}} for name in (['a.b','a-b','a_b'] if mode=='collision' else ['echo'])]}
    elif method=='tools/call':
        if mode=='hold': continue
        result={'content':[{'type':'text','text':request['params']['arguments']['text']}]}
        if mode=='image': result['content'].append({'type':'image','mimeType':'image/png','data':'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAusB9Wl6mNQAAAAASUVORK5CYII='})
        if mode=='dynamic': print(json.dumps({'jsonrpc':'2.0','method':'notifications/tools/list_changed'}),flush=True)
        if mode=='error': result['isError']=True
    else:
        print(json.dumps({'jsonrpc':'2.0','id':request['id'],'error':{'code':-32601,'message':'Unsupported method'}}),flush=True)
        continue
    print(json.dumps({'jsonrpc':'2.0','id':request['id'],'result':result}),flush=True)
    if mode=='ping' and method=='initialize':
        print(json.dumps({'jsonrpc':'2.0','id':'server-ping','method':'ping'}),flush=True)
