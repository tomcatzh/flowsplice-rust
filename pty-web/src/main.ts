import { Terminal } from '@xterm/xterm';
import { FitAddon } from '@xterm/addon-fit';
import '@xterm/xterm/css/xterm.css';
import './style.css';

type Mode = 'read_only' | 'read_write';
type Operation = { op:string; [key:string]: unknown };
type Writer = { attachment_id:string; travel_id:string; label:string };
type Session = { id:string; created_at_unix_secs:number; writer:Writer|null };
type Event = { type:string; [key:string]:any };
type Tab = { session:string; id:string; epoch:number; mode:Mode; terminal:Terminal; fit:FitAddon; pane:HTMLElement };
declare global { interface Window { flowsplicePlatform?:'macos'|'ios'|'android'; FlowSpliceNative?:{send(json:string):void}; webkit?:{messageHandlers:{pty:{postMessage(json:string):void}}}; flowsplice:{receive(event:Event):void;receiveBatch(events:Event[]):Promise<void>} } }
const app = document.querySelector<HTMLElement>('#app')!;
app.innerHTML = `<header><h1>FlowSplice PTY</h1><span id="status">等待本机</span></header><form id="access"><label id="relay-label">Relay IP:端口<input id="relay" autocomplete="off" placeholder="127.0.0.1:7000"></label><label>私钥密码<input id="password" type="password" autocomplete="current-password"></label><button id="enter">注册</button><button id="disconnect" type="button" hidden>断开</button></form><p id="notice" role="status"></p><section id="sessions"><div class="actions"><h2>会话</h2><button id="refresh">刷新</button><button id="new">新建</button></div><div id="list"></div></section><nav id="tabs" aria-label="已加入会话"></nav><div id="terminal-controls" hidden><button id="mode">切换读写</button><button id="detach">关闭标签</button><span id="mode-label"></span></div><section id="panes"></section><nav id="keys" aria-label="终端按键"></nav><dialog id="takeover"><p id="warning"></p><button id="cancel-takeover">取消</button><button id="confirm-takeover">确认接管</button></dialog>`;
const el = <T extends HTMLElement=HTMLElement>(id:string) => document.getElementById(id) as T;
let installed=false, connected=false, busy=false, canWrite=false, pendingNew=false;
let active:string|null=null;
let pendingNewId:string|null=null;
const tabs=new Map<string,Tab>();
const requests=new Map<string,Operation>();
const joining=new Set<string>();
let warning:{attachment:string; session:string; epoch:number; stale:boolean}|null=null;
function notice(text:string) { el('notice').textContent=text; }
function send(action:unknown) {
 const json=JSON.stringify(action);
 if(window.FlowSpliceNative) window.FlowSpliceNative.send(json);
 else if(window.webkit?.messageHandlers.pty) window.webkit.messageHandlers.pty.postMessage(json);
 else throw new Error('本机桥接未就绪');
}
function operation(value:Operation) { if(!connected) return; try {send({op:'operation',operation:value});} catch(error) {notice(String(error));} }
function controls() {
 el('relay-label').hidden=installed; el('enter').hidden=connected;
 el<HTMLButtonElement>('enter').disabled=busy; el('enter').textContent=installed?'连接':'注册';
 el('disconnect').hidden=!connected; el<HTMLButtonElement>('disconnect').disabled=busy;
 el<HTMLButtonElement>('refresh').disabled=!connected; el<HTMLButtonElement>('new').disabled=!connected||!canWrite||pendingNew;
 el('status').textContent=connected?'已连接':busy?'处理中':'未连接';
 el('terminal-controls').hidden=!active; const tab=active?tabs.get(active):undefined;
 el<HTMLButtonElement>('mode').disabled=!tab||!canWrite;
 el('mode-label').textContent=tab?.mode==='read_write'?'读写':'只读';
 el('mode').textContent=tab?.mode==='read_write'?'切换只读':'申请读写';
}
function fit(tab:Tab) { if(active!==tab.id) return; const size=tab.fit.proposeDimensions(); if(size) tab.terminal.resize(Math.max(2,Math.min(512,size.cols)),Math.max(1,Math.min(256,size.rows))); }
function focus(id:string) {active=id; for(const tab of tabs.values()) tab.pane.hidden=tab.id!==id; renderTabs(); controls(); const tab=tabs.get(id); if(tab) {fit(tab);tab.terminal.focus();} }
function renderTabs() {el('tabs').replaceChildren();for(const tab of tabs.values()) {const button=document.createElement('button');button.textContent=tab.session;button.setAttribute('aria-selected',String(active===tab.id));button.onclick=()=>focus(tab.id);el('tabs').append(button);} }
function remove(id:string) {const tab=tabs.get(id); if(!tab)return;tab.terminal.dispose();tab.pane.remove();tabs.delete(id);if(active===id)active=null;if(warning?.attachment===id)closeWarning();const next=tabs.keys().next().value;if(!active&&next)focus(next);renderTabs();controls();}
function clear() {for(const id of [...tabs.keys()])remove(id);requests.clear();joining.clear();pendingNew=false;pendingNewId=null;canWrite=false;closeWarning();el('list').replaceChildren();}
function input(tab:Tab,text:string) {if(!connected||tab.mode!=='read_write')return;const attachment_id=tab.id,writer_epoch=tab.epoch;const bytes=new TextEncoder().encode(text);for(let offset=0;offset<bytes.length;offset+=16384)operation({op:'input',attachment_id,writer_epoch,data:Array.from(bytes.subarray(offset,offset+16384))});}
function attach(result:any) {
 joining.delete(result.session.id);const existing=[...tabs.values()].find(tab=>tab.session===result.session.id);
 if(existing) {if(existing.id!==result.attachment_id)operation({op:'detach',attachment_id:result.attachment_id});focus(existing.id);return;}
 const pane=document.createElement('div');pane.className='terminal';el('panes').append(pane);
 // Keep xterm's normal input path: forcing screenReaderMode suppresses the
 // input-only insertText events produced by iOS software keyboards.
 const terminal=new Terminal({scrollback:0,fontSize:14,theme:{background:'#101916',foreground:'#e0e9e5'},linkHandler:{activate:()=>{},allowNonHttpProtocols:false}});
 const addon=new FitAddon();terminal.loadAddon(addon);terminal.open(pane);
 // Consume clipboard OSC even if a future terminal version adds a default handler.
 terminal.parser.registerOscHandler(52,()=>true);
 const tab:Tab={session:result.session.id,id:result.attachment_id,epoch:result.writer_epoch,mode:result.mode,terminal,fit:addon,pane};tabs.set(tab.id,tab);
 terminal.onData(data=>input(tab,data));terminal.onResize(({cols,rows})=>operation({op:'resize',attachment_id:tab.id,columns:cols,rows}));focus(tab.id);operation({op:'list'});
}
function list(sessions:Session[]) {el('list').replaceChildren();if(!sessions.length)el('list').textContent='暂无会话';for(const session of sessions) {const row=document.createElement('div');row.className='session';const label=document.createElement('span');label.textContent=session.id;row.append(label);for(const mode of ['read_only','read_write'] as const) {const button=document.createElement('button');button.textContent=mode==='read_only'?'只读加入':'读写加入';button.disabled=mode==='read_write'&&!canWrite;button.onclick=()=>{const tab=[...tabs.values()].find(tab=>tab.session===session.id);if(tab){focus(tab.id);return;}if(joining.has(session.id))return;joining.add(session.id);operation({op:'join',session_id:session.id,mode,columns:80,rows:24});};row.append(button);}el('list').append(row);} }
function closeWarning() {warning=null;el<HTMLDialogElement>('takeover').close();}
function protocol(message:any) {
 if(message.type==='hello'){canWrite=message.can_write;controls();return;}
 if(message.type==='response') {const request=requests.get(message.request_id);requests.delete(message.request_id);const result=message.result;
 if(request?.op==='new'||message.request_id===pendingNewId){pendingNew=false;pendingNewId=null;}if(request?.op==='join')joining.delete(String(request.session_id));
 if(result.status==='sessions')list(result.sessions);
 if(result.status==='attached')attach(result);
 if(result.status==='error'){notice(`${result.message}；可刷新会话列表。`);if(request?.op==='new')pendingNew=false;}
 if(result.status==='takeover_required'&&request?.op==='set_mode') {warning={attachment:String(request.attachment_id),session:result.session_id,epoch:result.epoch,stale:false};el('warning').textContent=`当前写入者：${result.writer.label||result.writer.travel_id}。接管后远端将切换为只读。`;el<HTMLDialogElement>('takeover').showModal();}
 } else if(message.type==='output'){tabs.get(message.attachment_id)?.terminal.write(new Uint8Array(message.data));}
 else if(message.type==='ownership'){for(const tab of tabs.values())if(tab.session===message.session_id){tab.epoch=message.epoch;tab.mode=message.writer?.attachment_id===tab.id?'read_write':'read_only';}if(warning&&warning.session===message.session_id&&warning.epoch!==message.epoch){warning.stale=true;el('warning').textContent='写入者已变化，请重新申请后核对新的交接提示。';}}
 else if(message.type==='detached')remove(message.attachment_id);
 else if(message.type==='session_ended'){for(const tab of [...tabs.values()])if(tab.session===message.session_id)remove(tab.id);operation({op:'list'});}
 controls();
}
window.flowsplice={async receiveBatch(events) { for(const event of events) { if(event.type==='protocol'&&event.message?.type==='output') { const tab=tabs.get(event.message.attachment_id); if(tab)await new Promise<void>(resolve=>tab.terminal.write(new Uint8Array(event.message.data),resolve)); } else window.flowsplice.receive(event); } },receive(event) {if(event.type==='state'){installed=event.installed;connected=event.connected;busy=event.busy;if(!connected)clear();controls();}else if(event.type==='protocol')protocol(event.message);else if(event.type==='submitted'){requests.set(event.request_id,event.operation);if(event.operation.op==='new')pendingNewId=event.request_id;if(requests.size>256)requests.delete(requests.keys().next().value!);}else if(event.type==='progress'){notice(event.progress.verification_code?`等待批准，校验码：${event.progress.verification_code}`:event.progress.phase);}else if(event.type==='error'){pendingNew=false;joining.clear();notice(event.message);controls();}}};
el('access').onsubmit=event=>{event.preventDefault();const password=el<HTMLInputElement>('password').value;try{send(installed?{op:'connect',password}:{op:'enroll',relay:el<HTMLInputElement>('relay').value.trim(),password});el<HTMLInputElement>('password').value='';}catch(error){notice(String(error));}};
el('disconnect').onclick=()=>{send({op:'disconnect'});connected=false;clear();controls();};
el('refresh').onclick=()=>operation({op:'list'});
el('new').onclick=()=>{if(!connected||!canWrite||pendingNew)return;pendingNew=true;controls();operation({op:'new',columns:80,rows:24});};
el('mode').onclick=()=>{const tab=active?tabs.get(active):undefined;if(tab)operation({op:'set_mode',attachment_id:tab.id,mode:tab.mode==='read_write'?'read_only':'read_write',force:false,expected_epoch:null});};
el('detach').onclick=()=>{if(active){operation({op:'detach',attachment_id:active});remove(active);}};
el('cancel-takeover').onclick=closeWarning;
el('confirm-takeover').onclick=()=>{const current=warning;if(!current)return;closeWarning();operation({op:'set_mode',attachment_id:current.attachment,mode:'read_write',force:!current.stale,expected_epoch:current.stale?null:current.epoch});};
if(window.flowsplicePlatform==='macos')el('keys').hidden=true;
else for(const [label,data] of [['Ctrl+C','\x03'],['Tab','\t'],['Esc','\x1b'],['↑','\x1b[A'],['↓','\x1b[B'],['←','\x1b[D'],['→','\x1b[C']]){const button=document.createElement('button');button.textContent=label;button.onclick=()=>{const tab=active?tabs.get(active):undefined;if(tab)input(tab,data);};el('keys').append(button);}
new ResizeObserver(()=>{const tab=active?tabs.get(active):undefined;if(tab)fit(tab);}).observe(el('panes'));
document.addEventListener('click',event=>{if((event.target as Element).closest('a'))event.preventDefault();});
controls();
if(window.webkit?.messageHandlers.pty)send({op:'ready'});
