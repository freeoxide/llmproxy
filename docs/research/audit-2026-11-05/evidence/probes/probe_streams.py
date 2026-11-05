#!/usr/bin/env python3
import json,sys,time,urllib.request,urllib.error,sqlite3
BASE='http://127.0.0.1:28080'; DB=sys.argv[1]; T0=float(sys.argv[2])
def call(path,body):
    req=urllib.request.Request(BASE+path,data=json.dumps(body).encode(),headers={'content-type':'application/json'},method='POST')
    t=time.time()
    try:
        r=urllib.request.urlopen(req,timeout=20); hd=time.time()-t; return r.status,hd,r.read().decode()
    except urllib.error.HTTPError as e: return e.code,time.time()-t,e.read().decode()
msg=[{'role':'user','content':'hi'}]
print('A. Anthropic documented stream -> Anthropic client')
s,h,t=call('/providers/ant/v1/messages',{'model':'ant-stream','max_tokens':5,'stream':True,'messages':msg})
for l in t.splitlines():
    if l.startswith('data:') and ('message_delta' in l or 'message_start' in l): print('  ',l[:300])
print('B. OpenAI client include_usage=true, usage after finish')
s,h,t=call('/providers/oai/v1/chat/completions',{'model':'m2','stream':True,'stream_options':{'include_usage':True},'messages':msg})
print('  ',[l[:200] for l in t.splitlines() if 'usage' in l][-2:])
print('C. Upstream 200 + error frame -> OpenAI client')
s,h,t=call('/providers/oai/v1/chat/completions',{'model':'err-frame','stream':True,'messages':msg}); print('   HTTP',s,repr(t[-260:]))
print('D. Upstream truncated mid-stream -> OpenAI client')
s,h,t=call('/providers/oai/v1/chat/completions',{'model':'trunc','stream':True,'messages':msg}); print('   HTTP',s,repr(t[-260:]))
print('E. Upstream truncated -> Anthropic client')
s,h,t=call('/providers/oai/v1/messages',{'model':'trunc','max_tokens':5,'stream':True,'messages':msg}); print('   HTTP',s,repr(t[-330:]))
time.sleep(1.2)
c=sqlite3.connect(DB)
for r in c.execute("select model_requested,streaming,input_tokens,output_tokens,cache_read_tokens,usage_provenance,stop_reason,error_kind,completed_at is not null from requests where model_requested in('ant-stream','m2','err-frame','trunc') order by received_at"): print('  ledger',r)
print('F. Lifetime: poll /health')
last=None
while time.time()-T0<45:
    try:
        urllib.request.urlopen(BASE+'/health',timeout=1).read(); ok=True
    except Exception: ok=False
    if ok!=last: print(f'   t={time.time()-T0:5.1f}s health={"up" if ok else "DOWN"}'); last=ok
    if not ok: break
    time.sleep(1)
