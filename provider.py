import json, time, threading, re
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
lock = threading.Lock()
seen = set()
class Handler(BaseHTTPRequestHandler):
    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        model = body.get('model', '')
        msgs = body.get('messages', [])
        last_user = max((i for i,m in enumerate(msgs) if m.get('role') == 'user'), default=-1)
        prompt = str(msgs[last_user].get('content', '')) if last_user >= 0 else ''
        markers = re.findall(r'(?:multi call fixture|background fixture) (?:web|tui)(?: [0-9]+)?', json.dumps(msgs))
        marker = markers[-1] if markers else ''
        with lock:
            tools_done = marker in seen
            if model == 'parent_demo' and marker: seen.add(marker)
        if marker: prompt = marker
        message = {'role':'assistant','content':'Scripted response complete. Each observed call costs $0.0020.'}
        finish = 'stop'
        if model == 'worker_demo':
            if 'background' in prompt.lower(): time.sleep(8)
            message['content'] = 'Scripted delegated work completed.'
        elif not tools_done and ('multi' in prompt.lower() or 'background' in prompt.lower()):
            background = 'background' in prompt.lower()
            message = {'role':'assistant','content':None,'tool_calls':[{'id':'call_fixture','type':'function','function':{'name':'delegate','arguments':json.dumps({'agent':'worker','prompt':'background fixture' if background else 'synchronous fixture','background':background})}}]}
            finish = 'tool_calls'
        mode_path = Path('/tmp/conversation-cost-demo/provider-mode.json')
        mode = json.loads(mode_path.read_text()).get('usage') if mode_path.exists() else 'normal'
        usage = {'prompt_tokens':1000,'completion_tokens':500,'total_tokens':1500}
        if mode == 'tiny':
            usage = {'prompt_tokens':1,'completion_tokens':0,'total_tokens':1}
            message = {'role':'assistant','content':'Scripted tiny usage: one input token.'}
        if mode == 'missing':
            usage = None
            message = {'role':'assistant','content':'Scripted response without a usage observation.'}
        with lock:
            with open('/tmp/conversation-cost-demo/provider-calls.jsonl','a') as f: f.write(json.dumps({'model':model,'usage':usage,'tool_call':finish=='tool_calls','time':time.time()})+'\n')
        if body.get('stream'):
            delta = dict(message); delta.pop('role',None)
            for idx, call in enumerate(delta.get('tool_calls',[])): call['index'] = idx
            chunks = [ {'id':'fixture','model':model,'choices':[{'index':0,'delta':{'role':'assistant',**delta}}]}, {'id':'fixture','model':model,'choices':[{'index':0,'delta':{},'finish_reason':finish}],'usage':usage} ]
            data = ''.join('data: '+json.dumps(c)+'\n\n' for c in chunks)+'data: [DONE]\n\n'
            self.send_response(200); self.send_header('Content-Type','text/event-stream'); self.end_headers(); self.wfile.write(data.encode())
        else:
            data=json.dumps({'id':'fixture','object':'chat.completion','model':model,'choices':[{'index':0,'message':message,'finish_reason':finish}],'usage':usage}).encode()
            self.send_response(200); self.send_header('Content-Type','application/json'); self.send_header('Content-Length',str(len(data))); self.end_headers(); self.wfile.write(data)
    def log_message(self, *_): pass
ThreadingHTTPServer(('127.0.0.1',42992),Handler).serve_forever()
