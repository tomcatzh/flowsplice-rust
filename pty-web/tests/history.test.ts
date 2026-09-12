import {beforeEach, expect, test, vi} from 'vitest';
import {HistoryView, renderHistoryLine} from '../src/history';
let pane:HTMLElement, history:HistoryView, requests:any[], id:string;
beforeEach(()=>{document.body.replaceChildren();pane=document.createElement('div');document.body.append(pane);requests=[];id='attachment';history=new HistoryView(pane,()=>id,op=>requests.push(op),vi.fn());Object.defineProperty(history.viewport,'clientHeight',{value:170});});
function reply(start=744,count=256,total=1000){const req=requests.at(-1);history.accept({attachment_id:id,capture_id:req.capture_id,total_lines:total,start,columns:80,lines:Array.from({length:count},(_,i)=>`row ${start+i}`)});}
function wheel(deltaY:number){pane.dispatchEvent(new WheelEvent('wheel',{deltaY,bubbles:true,cancelable:true}));}
function scroll(top:number){history.viewport.scrollTop=top;history.viewport.dispatchEvent(new Event('scroll'));}
test('upward only lazy entry, one request, cached re-entry and down arrow',()=>{wheel(40);expect(requests).toHaveLength(0);wheel(-34);wheel(-30);expect(requests).toHaveLength(1);expect(history.browsing).toBe(true);reply();expect(history.viewport.scrollTop).toBe(256*17-170-34);expect(history.bottom.hidden).toBe(false);history.bottom.click();expect(history.browsing).toBe(false);expect(history.bottom.hidden).toBe(true);wheel(-17);expect(requests).toHaveLength(1);scroll(256*17-170);expect(history.browsing).toBe(false);});
test('prepend preserves subpixel anchor and output never jumps current snapshot',()=>{wheel(-34);reply();scroll(60.5);expect(requests.at(-1).before).toBe(744);history.output();reply(488);expect(history.viewport.scrollTop).toBe(60.5+256*17);const anchor=history.viewport.scrollTop;history.output();expect(history.viewport.scrollTop).toBe(anchor);history.live();wheel(-20);expect(requests.at(-1).before).toBeNull();expect(requests[0].capture_id).not.toBe(requests.at(-1).capture_id);});
test('stale capture and attachment replies ignored; errors retry without dropping live view',()=>{wheel(-20);const old=requests[0];history.reset();id='new';wheel(-20);history.accept({attachment_id:'attachment',capture_id:old.capture_id,start:0,total_lines:1,columns:80,lines:['stale']});expect(history.content.textContent).not.toContain('stale');history.fail(old.capture_id);expect(history.pending).toBe(true);history.fail();expect(history.pending).toBe(false);expect(history.retry.hidden).toBe(false);history.retry.click();expect(requests).toHaveLength(3);reply();expect(history.retry.hidden).toBe(true);history.dispose();expect(pane.children).toHaveLength(0);});
test('50k rows retain bounded visible DOM and isolated caches',()=>{wheel(-20);reply(49744,256,50000);for(let end=49744;end>0;){const start=Math.max(0,end-256);scroll(0);reply(start,end-start,50000);end=start;}expect(history.content.style.height).toBe(`${50000*17}px`);expect(history.content.children.length).toBeLessThan(45);const other=new HistoryView(document.createElement('div'),()=>id,op=>requests.push(op),vi.fn());other.enter(20);expect(requests.at(-1).before).toBeNull();expect(requests.at(-1).capture_id).not.toBe(requests[0].capture_id);});
test('ANSI colors/attributes are inert DOM text including unicode and malicious controls',()=>{const node=document.createElement('div');node.append(renderHistoryLine('\x1b[1;4;38;2;1;2;3m中👩‍💻é<svg onload=bad>\x1b[0m\x1b]8;;https://evil\x07link\x1b]52;c;secret\x07\x1b[2J\x1bPsecret\x1b\\tail'));
expect(node.textContent).toBe('中👩‍💻é<svg onload=bad>linktail');expect(node.querySelector('svg,a')).toBeNull();const span=node.firstElementChild as HTMLElement;expect(span.style.color).toBe('rgb(1, 2, 3)');expect(span.style.fontWeight).toBe('bold');expect(span.style.textDecoration).toBe('underline');});
test('shift paging is local, touch down enters history',()=>{const key=new KeyboardEvent('keydown',{key:'PageUp',shiftKey:true,bubbles:true,cancelable:true});pane.dispatchEvent(key);expect(key.defaultPrevented).toBe(true);reply();pane.dispatchEvent(new KeyboardEvent('keydown',{key:'End',shiftKey:true,bubbles:true,cancelable:true}));expect(history.browsing).toBe(false);history.reset();pane.dispatchEvent(new TouchEvent('touchstart',{touches:[{clientY:20} as Touch],bubbles:true}));pane.dispatchEvent(new TouchEvent('touchmove',{touches:[{clientY:80} as Touch],bubbles:true,cancelable:true}));expect(history.browsing).toBe(true);});

test('initial failure can return live and reenter; hidden pending result caches without focus',()=>{
  wheel(-20);expect(history.loading.hidden).toBe(false);history.fail();expect(history.loading.hidden).toBe(true);
  history.live();wheel(-20);expect(requests).toHaveLength(2);expect(history.pending).toBe(true);
  history.live();const focus=vi.spyOn(history.viewport,'focus');reply();expect(focus).not.toHaveBeenCalled();expect(history.viewport.hidden).toBe(true);expect(history.bottom.hidden).toBe(true);
  wheel(-20);expect(requests).toHaveLength(2);expect(history.viewport.hidden).toBe(false);
});

test('invalid bounds, metadata drift and nonprogress pages fail without request loops',()=>{
  const badPages=[{total_lines:NaN},{total_lines:50257},{total_lines:1.5},{start:-1},{columns:0},{columns:513},{columns:80.5},{start:1000,lines:[]},{start:743}];
  for(const bad of badPages){history.reset();wheel(-20);const req=requests.at(-1);const count=requests.length;
    history.accept({attachment_id:id,capture_id:req.capture_id,total_lines:1000,start:744,columns:80,lines:Array(256).fill('row'),...bad});
    expect(history.pending).toBe(false);expect(history.retry.hidden).toBe(false);expect(requests.length).toBe(count);
  }
  history.reset();wheel(-20);reply();scroll(0);const req=requests.at(-1);
  history.accept({attachment_id:id,capture_id:req.capture_id,total_lines:1000,start:744,columns:80,lines:[]});expect(history.pending).toBe(false);expect(history.retry.hidden).toBe(false);
  history.retry.click();history.accept({attachment_id:id,capture_id:req.capture_id,total_lines:999,start:488,columns:80,lines:Array(256).fill('row')});expect(history.pending).toBe(false);expect(history.retry.hidden).toBe(false);
});

test('normalized inherited colon SGR supports color space, underline color and styles safely',()=>{
  const node=document.createElement('div');
  node.append(renderHistoryLine('\x1b[0;38:2::12:34:56;48:5:240;58:2:0:21:43:65;4:3;1;2;3;9m继承色🙂\x1b[22;23;24;29mnormal\x1b[8mhidden\x1b[28;38:2:1:2:3;4:2mvisible'));
  const [first,normal,hidden,visible]=Array.from(node.children) as HTMLElement[];
  expect(first.style.color).toBe('rgb(12, 34, 56)');expect(first.style.backgroundColor).toBe('rgb(88, 88, 88)');expect(first.style.textDecorationColor.replace(/ /g,'')).toBe('rgb(21,43,65)');expect(first.style.textDecorationStyle).toBe('wavy');expect(first.style.fontStyle).toBe('italic');expect(first.style.opacity).toBe('0.5');expect(first.style.textDecoration).toBe('underline line-through');
  expect(normal.style.fontWeight).toBe('');expect(normal.style.fontStyle).toBe('');expect(normal.style.opacity).toBe('');expect(normal.style.textDecoration).toBe('');expect(hidden.style.color).toBe('transparent');expect(visible.style.color).toBe('rgb(1, 2, 3)');expect(visible.style.textDecorationStyle).toBe('double');
  expect(node.textContent).toBe('继承色🙂normalhiddenvisible');
});

test('expired partial capture retains cached display until fresh retry and never mixes pages',()=>{
  wheel(-20);reply();scroll(0);
  const old=requests.at(-1);const cached=history.content.textContent;
  history.fail(old.capture_id,'This human-readable message can change','history_snapshot_expired');
  expect(history.content.textContent).toBe(cached);
  expect(history.retry.textContent).toContain('已过期');
  history.retry.click();const fresh=requests.at(-1);
  expect(fresh.capture_id).not.toBe(old.capture_id);expect(fresh.before).toBeNull();
  history.accept({attachment_id:id,capture_id:old.capture_id,total_lines:1000,start:488,columns:80,lines:Array(256).fill('stale')});
  expect(history.pending).toBe(true);
  history.accept({attachment_id:id,capture_id:fresh.capture_id,total_lines:1,start:0,columns:80,lines:['new']});
  expect(history.content.textContent).toBe('new');
});
test('hostile bidi text is retained but each displayed row isolates its direction',()=>{
  wheel(-20);const req=requests.at(-1);
  history.accept({attachment_id:id,capture_id:req.capture_id,total_lines:2,start:0,columns:80,lines:['\u202eevil','safe']});
  expect(history.content.textContent).toBe('\u202eevilsafe');
  for(const row of Array.from(history.content.children) as HTMLElement[]){expect(row.style.unicodeBidi).toBe('isolate');expect(row.style.direction).toBe('ltr');}
});

test('expiry-like text with generic code retries the existing cursor',()=>{
  wheel(-20);reply();scroll(0);const old=requests.at(-1);
  history.fail(old.capture_id,'history snapshot expired; start a new capture','operation_failed');
  expect(history.retry.textContent).not.toContain('已过期');
  history.retry.click();expect(requests.at(-1).capture_id).toBe(old.capture_id);
  expect(requests.at(-1).before).toBe(old.before);
});
