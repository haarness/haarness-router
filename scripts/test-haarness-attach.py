#!/usr/bin/env python3
"""Opt-in live Haarness attach verification; sends synthetic prompts through the router.

Requires the Stackforward credential YAML, a built router and Haarness binary,
plus tmux. Never prints or writes upstream provider keys. HAARNESS_LIVE_MODELS
may select a comma-separated subset; omitted means all current default cases.
"""
import json, os, pathlib, queue, re, secrets, shlex, socket, subprocess, tempfile, threading, time, urllib.request, urllib.error

root = pathlib.Path(tempfile.mkdtemp(prefix='haarness-router-attach-')).resolve()
repo = pathlib.Path(__file__).resolve().parents[2]
binary = pathlib.Path(os.environ.get('HAARNESS_BIN', str(repo/'haarness/target/debug/haarness'))).resolve()
router_repo = repo/'haarness-router'
secrets_file = os.environ.get('HAARNESS_STACKFORWARD_SECRETS', '/Users/alex/work/stackforward/robowork/robowork-backend-stack/environments/prod/group_vars/all.yml')
router_binary = pathlib.Path(os.environ.get('HAARNESS_ROUTER_BIN', str(router_repo/'target/debug/haarness-router'))).resolve()
server_workspace = root/'server-workspace'
client_workspace = root/'client-workspace'
for p in [server_workspace/'.haarness', client_workspace, root/'client-home', root/'server-home']:
    p.mkdir(parents=True, exist_ok=True)
(server_workspace/'proof.txt').write_text('The verification marker is cedar-7392.\n')
router_key, server_token = secrets.token_hex(32), secrets.token_hex(32)
require_key = os.environ.get('HAARNESS_ROUTER_AUTH') == 'required'
env = {k:v for k,v in os.environ.items() if not k.startswith(('HAARNESS_', 'OPENAI_', 'ANTHROPIC_', 'GEMINI_', 'GOOGLE_', 'OPENROUTER_'))}
router_env = {**env, 'HAARNESS_ROUTER_KEY':router_key, 'HAARNESS_UI_USERNAME':'test-admin', 'HAARNESS_UI_PASSWORD':secrets.token_hex(24)}
server_env = {**env, 'HAARNESS_HOME':str(root/'server-home'), 'HAARNESS_DATA_DIR':str(root/'server-data'), 'HAARNESS_SERVER_TOKEN':server_token}
client_env = {**env, 'HAARNESS_HOME':str(root/'client-home'), 'HAARNESS_DATA_DIR':str(root/'client-data'), 'HAARNESS_SERVER_TOKEN':server_token}
sock=socket.socket(); sock.bind(('127.0.0.1',0)); port=sock.getsockname()[1]; sock.close()
router_url = f'http://127.0.0.1:{port}'
(server_workspace/'.haarness/config.toml').write_text(f'''[defaults]
provider = "router"
model = "openai/gpt-5-nano"
[permissions]
read = "allow"
[providers.ollama]
enabled = false
[providers.lmstudio]
enabled = false
''')
logs=[]; processes=[]; checks=[]; failures=[]
session='router-live-'+str(os.getpid())
def check(label):
    checks.append(label); print('PASS '+label, flush=True)
def request(base,path,key=None,value=None):
    headers={'Content-Type':'application/json'}
    if key: headers['Authorization']='Bearer '+key
    req=urllib.request.Request(base+path, data=None if value is None else json.dumps(value).encode(), headers=headers)
    with urllib.request.urlopen(req,timeout=30) as response:
        return json.load(response)
def api(path,value=None): return request(url,path,server_token,value)
def until(fn, label, seconds=60):
    deadline=time.monotonic()+seconds
    while time.monotonic()<deadline:
        try:
            value=fn()
            if value:return value
        except (urllib.error.URLError, ConnectionError): pass
        time.sleep(.2)
    raise AssertionError(label)
def run(args,env=client_env,timeout=150):
    return subprocess.run(args,env=env,text=True,capture_output=True,timeout=timeout)
def tmux(*args):
    p=run(['tmux',*args],timeout=15)
    if p.returncode: raise AssertionError('tmux '+args[0]+' failed')
    return p.stdout
def capture():return tmux('capture-pane','-p','-t',session+':0.0')
def send(text):tmux('send-keys','-l','-t',session+':0.0',text);tmux('send-keys','-t',session+':0.0','Enter')
def start_tui(thread_id):
    # Environment containing the server token goes through an inherited tmux
    # session environment, never provider credentials or command arguments.
    args=[str(binary),'--workspace',str(client_workspace),'--session',thread_id,'attach',url]
    tmux('new-session','-d','-x','155','-y','42','-s',session,'/bin/sh')
    tmux('set-environment','-t',session,'HAARNESS_SERVER_TOKEN',server_token)
    tmux('set-environment','-t',session,'HAARNESS_DATA_DIR',client_env['HAARNESS_DATA_DIR'])
    tmux('set-environment','-t',session,'HAARNESS_HOME',client_env['HAARNESS_HOME'])
    tmux('respawn-pane','-k','-t',session+':0.0',shlex.join(args))
try:
    log=(root/'router.log').open('w');logs.append(log)
    router=subprocess.Popen(['ruby',str(router_repo/'scripts/run-stackforward.rb'),secrets_file,str(router_binary),str(router_repo/'config.stackforward.toml'),'--bind',f'127.0.0.1:{port}','--set','web_ui.enabled=false', '--require-key' if require_key else '--no-auth'],env=router_env,stdout=log,stderr=log)
    processes.append(router)
    until(lambda:request(router_url,'/healthz'),'router startup')
    for path in ['/healthz','/readyz']:
        request(router_url,path)
    check('router health and readiness')
    for key in ([None,'invalid-test-key'] if require_key else []):
        try:request(router_url,'/v1/models',key);raise AssertionError('router allowed invalid auth')
        except urllib.error.HTTPError as e:assert e.code==401
    models=request(router_url,'/v1/models',router_key if require_key else None)
    (root/'router-models.json').write_text(json.dumps(models,indent=2))
    check('router model discovery with '+('required key and invalid-key rejection' if require_key else 'no key'))
    log=(root/'server.log').open('w');logs.append(log)
    server=subprocess.Popen([str(binary),'--workspace',str(server_workspace),'--trust-project','serve','--port','0'],env=server_env,stdout=subprocess.PIPE,stderr=log,text=True)
    processes.append(server);ready=queue.Queue()
    def reader():
        for line in server.stdout:
            m=re.search(r'Haarness server listening on (http://\S+)',line)
            if m:ready.put(m[1])
        ready.put(None)
    threading.Thread(target=reader,daemon=True).start()
    url=ready.get(timeout=90);assert url,'Haarness startup'
    assert api('/v1/info')['workspace']==str(server_workspace)
    connect={'action':'router_connect','url':router_url,'label':'Haarness Router'}
    result=api('/v1/manage/providers',connect)
    if require_key:
        assert any(field['key']=='key' and field['secret'] for field in result['fields'])
        result=api('/v1/manage/providers',{**connect,'key':router_key})
    assert result['title']=='Router connected'
    check('shared router integration saves a server-owned connection with '+('key challenge' if require_key else 'no key prompt'))
    def find_provider():
        providers=api('/v1/providers/refresh',{})
        return next((p for p in providers if p['id']=='router'),None)
    provider=until(find_provider,'router connection hot reload',45)
    assert provider['authenticated']
    expected={m['id'] for m in models['data']}
    actual={m['id'] for m in provider['models']}
    print('Catalog difference:', sorted(expected-actual), sorted(actual-expected), flush=True)
    (root/'haarness-router-catalog.json').write_text(json.dumps(provider,indent=2))
    assert {'openai/gpt-6-astra','openai/gpt-5.6-sol','anthropic/claude-fable-5-1','gemini/gemini-3.8-flash'} <= actual
    (root/'haarness-router-catalog.json').write_text(json.dumps(provider,indent=2))
    check('Haarness discovers current router models and aliases through the saved server connection')
    successful=[]
    defaults='openai/gpt-6-astra,openai/gpt-5.6-sol,openai/gpt-5.6-terra,openai/gpt-5.6-luna,anthropic/claude-fable-5-1,anthropic/claude-opus-5,anthropic/claude-sonnet-5,gemini/gemini-3.8-flash,fast'
    for model in os.environ.get('HAARNESS_LIVE_MODELS', defaults).split(','):
        title='Router live '+model
        p=run([str(binary),'--workspace',str(client_workspace),'run','--attach',url,'--provider','router','--model','router/'+model,'--title',title,'Read proof.txt using the read tool. Reply with exactly the verification marker from the file. Do not use other tools.'])
        slug=model.replace('/','_')
        (root/(slug+'-cli.txt')).write_text(p.stdout+'\n'+p.stderr)
        threads=api('/v1/threads'); thread=next((t for t in threads if t['title']==title),None)
        items=api('/v1/threads/'+thread['id']+'/items') if thread else []
        (root/(slug+'-items.json')).write_text(json.dumps(items,indent=2))
        has_read=any(i['data']['type']=='tool_call' and i['data'].get('name','').split('.')[-1]=='read' for i in items)
        if p.returncode or 'cedar-7392' not in p.stdout or not has_read:
            label=model+' attached CLI/tool failed'; failures.append(label);print('FAIL '+label,flush=True);continue
        assert not (client_workspace/'proof.txt').exists()
        check(model+' attached CLI stream and server-side read tool round trip')
        successful.append(thread)
    assert successful,'No provider completed live attach'
    thread=successful[0];thread_id=thread['id']
    p=run([str(binary),'--workspace',str(client_workspace),'--session',thread_id,'run','--attach',url,'--provider','router','--model','router/'+thread['model'],'Repeat the verification marker you read in the preceding turn.'])
    assert p.returncode==0 and 'cedar-7392' in p.stdout
    check('attached CLI resumes conversation and recalls earlier tool result')
    start_tui(thread_id)
    until(lambda:'cedar-7392' in capture(),'TUI history restored',45)
    (root/'tui-restored.txt').write_text(capture())
    send('Reply with exactly: TUI-ROUTER-LIVE-481')
    def tui_done():
        items=api('/v1/threads/'+thread_id+'/items')
        return not api('/v1/turns/active') and any(i['data']['type']=='assistant_message' and 'TUI-ROUTER-LIVE-481' in i['data'].get('text','') for i in items)
    until(tui_done,'live TUI reply',120)
    until(lambda:'TUI-ROUTER-LIVE-481' in capture(),'TUI rendered reply')
    (root/'tui-live-response.txt').write_text(capture())
    check('real tmux attach restores CLI history and completes live model response')
    send('/models')
    until(lambda:'Select model' in capture(),'TUI model picker')
    (root/'tui-model-picker.txt').write_text(capture())
    check('attached TUI opens router model picker')
    tmux('send-keys','-t',session+':0.0','Escape')
    tmux('kill-session','-t',session)
    assert server.poll() is None and router.poll() is None
    start_tui(thread_id)
    until(lambda:'TUI-ROUTER-LIVE-481' in capture(),'TUI reattached history',45)
    (root/'tui-reattached.txt').write_text(capture())
    check('TUI detach leaves both servers alive and reattach restores live response')
    send('/providers')
    until(lambda:'Connect Haarness Router' in capture(),'TUI providers integration entry')
    send('Connect Haarness Router')
    until(lambda:'Router address' in capture(),'TUI router address form')
    send('Router address')
    send(router_url)
    send('Connect')
    if require_key:
        until(lambda:'Router access key' in capture(),'TUI optional key challenge')
        send('Router access key')
        send(router_key)
        send('Connect')
    until(lambda:'Router connected' in capture(),'TUI router connection saved')
    (root/'tui-router-connected.txt').write_text(capture())
    until(lambda:any(p['id']=='router_2' for p in api('/v1/providers/refresh',{})),'TUI router hot reload')
    check('real tmux TUI connects a second self-hosted router with '+('key challenge' if require_key else 'address only'))
    (root/'final-items.json').write_text(json.dumps(api('/v1/threads/'+thread_id+'/items'),indent=2))
    cli_env={**env,'HAARNESS_HOME':str(root/'cli-setup-home'),'HAARNESS_DATA_DIR':str(root/'cli-setup-data')}
    args=[str(binary),'--workspace',str(client_workspace),'provider','connect-router',router_url,'--name','CLI router']
    if require_key:args.append('--key-stdin')
    p=subprocess.run(args,env=cli_env,input=router_key+'\n' if require_key else '',text=True,capture_output=True,timeout=60)
    (root/'cli-connect-router.txt').write_text(p.stdout+'\n'+p.stderr)
    assert p.returncode==0 and 'Connected router' in p.stdout
    check('CLI connect-router '+('with a key supplied only on stdin' if require_key else 'without a key'))
    p=subprocess.run([str(binary),'--workspace',str(client_workspace),'provider','list'],env=cli_env,text=True,capture_output=True,timeout=60)
    (root/'cli-provider-list.txt').write_text(p.stdout+'\n'+p.stderr)
    assert p.returncode==0 and 'CLI router' in p.stdout
    check('new CLI process reloads the saved router connection')
except Exception as e:
    import traceback
    traceback.print_exc()
    failures.append(type(e).__name__+': '+str(e))
    print('FAIL '+failures[-1],flush=True)
finally:
    subprocess.run(['tmux','kill-session','-t',session],capture_output=True)
    for p in reversed(processes):
        p.terminate()
        try:p.wait(timeout=10)
        except subprocess.TimeoutExpired:p.kill();p.wait()
    for log in logs:log.close()
    (root/'summary.json').write_text(json.dumps({'checks':checks,'failures':failures},indent=2))
    print(json.dumps({'passed':len(checks),'failures':failures,'evidence':str(root)}),flush=True)
    raise SystemExit(bool(failures))
