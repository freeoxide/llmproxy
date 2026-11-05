#!/usr/bin/env python3
# Local-only fake LLM upstream that records what the proxy actually sends.
import json,sys,time,threading
from http.server import BaseHTTPRequestHandler,ThreadingHTTPServer
LOG=sys.argv[2]; PORT=int(sys.argv[1])
lock=threading.Lock()
class H(BaseHTTPRequestHandler):
    protocol_version='HTTP/1.1'
    def log_message(self,*a): pass
    def _rec(self,body):
        with lock:
            open(LOG,'a').write(json.dumps({'port':PORT,'t':time.time(),'path':self.path,'headers':{k.lower():v for k,v in self.headers.items()},'body':body})+'\n')
    def _json(self,obj,code=200):
        b=json.dumps(obj).encode(); self.send_response(code); self.send_header('content-type','application/json'); self.send_header('content-length',str(len(b))); self.end_headers(); self.wfile.write(b)
    def _sse(self,frames):
        self.send_response(200); self.send_header('content-type','text/event-stream'); self.send_header('connection','close'); self.end_headers()
        for f in frames:
            if isinstance(f,(int,float)): time.sleep(f); continue
            self.wfile.write(f.encode()); self.wfile.flush()
        self.close_connection=True
    def do_POST(self):
        n=int(self.headers.get('content-length',0)); raw=self.rfile.read(n)
        try: body=json.loads(raw)
        except Exception: body=raw.decode('utf8','replace')
        self._rec(body)
        model=(body.get('model') if isinstance(body,dict) else '') or ''
        if model=='redir' and PORT==28081:
            self.send_response(307); self.send_header('location','http://127.0.0.1:28082'+self.path); self.send_header('content-length','0'); self.end_headers(); return
        if model=='slow': time.sleep(2)
        if model=='ant-stream':
            ev=lambda t,d:'event: '+t+'\ndata: '+json.dumps(d)+'\n\n'
            self._sse([ev('message_start',{'type':'message_start','message':{'id':'msg_1','type':'message','role':'assistant','model':model,'content':[],'stop_reason':None,'stop_sequence':None,'usage':{'input_tokens':25,'output_tokens':1,'cache_read_input_tokens':7,'cache_creation_input_tokens':3}}}),ev('content_block_start',{'type':'content_block_start','index':0,'content_block':{'type':'text','text':''}}),ev('content_block_delta',{'type':'content_block_delta','index':0,'delta':{'type':'text_delta','text':'hello'}}),ev('content_block_stop',{'type':'content_block_stop','index':0}),ev('message_delta',{'type':'message_delta','delta':{'stop_reason':'max_tokens','stop_sequence':None},'usage':{'output_tokens':15}}),ev('message_stop',{'type':'message_stop'})]); return
        if 'messages' in self.path and 'chat' not in self.path:
            self._json({'id':'msg_1','type':'message','role':'assistant','model':model,'content':[{'type':'text','text':'ok'}],'stop_reason':'end_turn','stop_sequence':None,'usage':{'input_tokens':10,'output_tokens':2}})
            return
        if model in('err-frame','trunc') and isinstance(body,dict) and body.get('stream'):
            base=lambda d,fin=None:'data: '+json.dumps({'id':'c1','object':'chat.completion.chunk','created':1,'model':model,'choices':[{'index':0,'delta':d,'finish_reason':fin}]})+'\n\n'
            if model=='err-frame': self._sse(['data: '+json.dumps({'error':{'message':'upstream overloaded','type':'server_error','code':'overloaded'}})+'\n\n'])
            else: self._sse([base({'role':'assistant','content':''}),base({'content':'partial'})])
            return
        usage={'prompt_tokens':100,'completion_tokens':5,'total_tokens':105}
        if model=='deepseek': usage.update(prompt_cache_hit_tokens=30,prompt_cache_miss_tokens=70)
        if model=='oai-cached': usage.update(prompt_tokens_details={'cached_tokens':30},completion_tokens_details={'reasoning_tokens':2})
        if isinstance(body,dict) and body.get('stream'):
            def c(d,fin=None,u=None):
                o={'id':'chatcmpl-1','object':'chat.completion.chunk','created':1,'model':model,'choices':[{'index':0,'delta':d,'finish_reason':fin}]}
                if u: o['choices']=[]; o['usage']=u
                return 'data: '+json.dumps(o)+'\n\n'
            frames=[c({'role':'assistant','content':''})]
            if model=='role-then-slow': frames+= [3]
            frames+=[c({'content':'hello'}),c({},'stop')]
            so=body.get('stream_options') or {}
            if so.get('include_usage'): frames.append(c({},None,usage))
            frames.append('data: [DONE]\n\n')
            self._sse(frames); return
        self._json({'id':'chatcmpl-1','object':'chat.completion','created':1,'model':model,'choices':[{'index':0,'message':{'role':'assistant','content':'hi'},'finish_reason':'stop'}],'usage':usage})
ThreadingHTTPServer(('127.0.0.1',PORT),H).serve_forever()
