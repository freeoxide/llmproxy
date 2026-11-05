#!/usr/bin/env python3
import json,sys,time,threading,urllib.request,urllib.error,sqlite3,os
BASE='http://127.0.0.1:28080'; LOG=sys.argv[1]; DB=sys.argv[2]
def call(path,body,headers=None,stream=False):
    h={'content-type':'application/json'}; h.update(headers or {})
    req=urllib.request.Request(BASE+path,data=json.dumps(body).encode(),headers=h,method='POST')
    try:
        with urllib.request.urlopen(req,timeout=40) as r: return r.status,dict(r.headers),r.read().decode()
    except urllib.error.HTTPError as e: return e.code,dict(e.headers),e.read().decode()
def last(n=1):
    ls=open(LOG).read().splitlines() if os.path.exists(LOG) else []
    return [json.loads(x) for x in ls[-n:]]
def clear(): open(LOG,'w').close()
def hdr(t): print('\n=== '+t)
A='/providers/ant/v1/messages'; O='/providers/oai/v1/chat/completions'; AO='/providers/oai/v1/messages'
msg=[{'role':'user','content':'hi'}]

hdr('1. OpenAI client -> OpenAI upstream: are request fields forwarded?')
clear(); b={'model':'m1','messages':msg,'response_format':{'type':'json_schema','json_schema':{'name':'x','schema':{'type':'object'}}},'seed':7,'n':2,'parallel_tool_calls':False,'logprobs':True,'frequency_penalty':0.5,'presence_penalty':0.5,'logit_bias':{'1':1},'service_tier':'flex','prompt_cache_key':'k','store':False,'reasoning_effort':'high','stop':['X'],'user':'u1','temperature':0.3}
s,_,t=call(O,b); up=last()[0]['body'] if last() else {}
print('client status',s); print('sent keys    ',sorted(b)); print('upstream keys',sorted(up)); print('DROPPED      ',sorted(set(b)-set(up)))

hdr('2. Anthropic client -> Anthropic upstream: field acceptance')
for label,extra in [('stop_sequences',{'stop_sequences':['END']}),('top_k',{'top_k':5}),('service_tier',{'service_tier':'auto'}),('output_config',{'output_config':{'effort':'high'}}),('tool with cache_control',{'tools':[{'name':'t','input_schema':{'type':'object'},'cache_control':{'type':'ephemeral'}}]}),('server tool',{'tools':[{'type':'web_search_20250305','name':'web_search'}]})]:
    clear(); s,_,t=call(A,{'model':'m1','max_tokens':5,'messages':msg,**extra}); print(f'{label:26} -> HTTP {s} forwarded={bool(last())} {t[:150]}')

hdr('3. Anthropic cache markers (what reaches upstream)')
clear(); s,_,t=call(A,{'model':'m1','max_tokens':5,'system':[{'type':'text','text':'SYSTEM PROMPT','cache_control':{'type':'ephemeral'}}],'messages':[{'role':'user','content':[{'type':'text','text':'hi','cache_control':{'type':'ephemeral'}}]}]}); print('plain 5m markers: HTTP',s,json.dumps(last()[0]['body'])[:400] if last() else None)
clear(); s,_,t=call(A,{'model':'m1','max_tokens':5,'system':[{'type':'text','text':'SYSTEM PROMPT','cache_control':{'type':'ephemeral','ttl':'1h'}}],'messages':msg}); print('system with ttl=1h: HTTP',s,json.dumps(last()[0]['body'])[:400] if last() else None)
clear(); s,_,t=call(A,{'model':'m1','max_tokens':5,'messages':[{'role':'user','content':[{'type':'text','text':'KEEP ME'},{'type':'text','text':'MUST NOT VANISH','cache_control':{'type':'ephemeral','ttl':'1h'}}]}]}); print('message text with ttl=1h: HTTP',s,json.dumps(last()[0]['body'])[:400] if last() else None)
clear(); s,_,t=call(A,{'model':'m1','max_tokens':5,'messages':[{'role':'user','content':[{'type':'tool_result','tool_use_id':'a','content':'ok','cache_control':{'type':'ephemeral'}}]}]}); print('tool_result marker: HTTP',s,json.dumps(last()[0]['body'])[:400] if last() else None)

hdr('4. Inbound headers reaching upstream')
clear(); s,_,t=call(A,{'model':'m1','max_tokens':5,'messages':msg},{'anthropic-beta':'context-1m-2025-08-07','anthropic-version':'2099-01-01','origin':'https://evil.example','authorization':'Bearer client-secret','x-claude-code-session-id':'sess-1','user-agent':'probe/1'})
print('upstream saw:',{k:v for k,v in last()[0]['headers'].items()} if last() else None)

hdr('5. Streaming usage: Anthropic client, OpenAI upstream')
clear(); s,_,t=call(AO,{'model':'m1','max_tokens':5,'stream':True,'messages':msg}); print('upstream stream_options:',last()[0]['body'].get('stream_options') if last() else None); print('client SSE tail:',t[-420:])
hdr('5b. Streaming usage: OpenAI client without stream_options')
clear(); s,_,t=call(O,{'model':'m1','stream':True,'messages':msg}); print('upstream stream_options:',last()[0]['body'].get('stream_options') if last() else None)

hdr('6. Usage mapping, ledger, and cost')
s,_,t=call(O,{'model':'deepseek','messages':msg}); print('deepseek client usage:',json.loads(t).get('usage'))
s,_,t=call(AO,{'model':'deepseek','max_tokens':5,'messages':msg}); print('deepseek via anthropic client usage:',json.loads(t).get('usage'))
s,_,t=call(O,{'model':'oai-cached','messages':msg}); print('oai-cached client usage:',json.loads(t).get('usage'))
time.sleep(1.5)
c=sqlite3.connect(DB)
for r in c.execute("select model_upstream,streaming,input_tokens,output_tokens,cache_read_tokens,cache_creation_tokens,reasoning_tokens,usage_provenance,cost_input,cost_cache_creation,cost_cache_read,cost_total from requests where model_requested in('deepseek','oai-cached') order by received_at"): print(' ledger',r)
for r in c.execute("select client_protocol,streaming,input_tokens,output_tokens,usage_provenance,stop_reason,error_kind from requests where model_requested='m1' and streaming=1"): print(' ledger stream',r)

hdr('7. Dedup semantics (window=500ms)')
clear(); res={}
def go(k,delay,hdrs=None):
    time.sleep(delay); res[k]=call(O,{'model':'slow','messages':msg+[{'role':'user','content':k[:0]+'same'}]},hdrs)[0]
ts=[threading.Thread(target=go,args=(n,d)) for n,d in [('t0',0),('t+0.1',0.1),('t+1.0',1.0)]]
[x.start() for x in ts]; [x.join() for x in ts]
print('identical requests while first is still in-flight (2s upstream):',res,' upstream hits:',len(open(LOG).read().splitlines()))
time.sleep(3)

hdr('8. Redirect: does the upstream credential follow a 307 to another port?')
clear(); s,_,t=call(O,{'model':'redir','messages':msg}); print('client',s)
for x in last(5): print(' port',x['port'],'auth=',x['headers'].get('authorization'),'x-api-key=',x['headers'].get('x-api-key'))
clear(); s,_,t=call(A,{'model':'redir','max_tokens':5,'messages':msg}); print('client',s)
for x in last(5): print(' port',x['port'],'auth=',x['headers'].get('authorization'),'x-api-key=',x['headers'].get('x-api-key'))
