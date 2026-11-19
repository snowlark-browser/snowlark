// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

(function(){
  const out = {};
  function t(name, f){ f(); f(); const r=[]; for(let i=0;i<5;i++){ const s=performance.now(); f(); r.push(performance.now()-s);} r.sort((a,b)=>a-b); out[name]=Math.round(r[2]*10)/10; }
  // Pure JavaScript: no bindings, so any gap here is V8's build, flags or the embedder's setup.
  t('js_numeric', () => { let s=0; for(let i=0;i<3e7;i++){ s=(s+i*i)%1000003; } return s; });
  t('js_objects', () => { let a=[]; for(let i=0;i<2e6;i++){ a.push({x:i,y:i+1,z:{w:i}}); if(a.length>1000)a=[]; } return a; });
  t('js_strings', () => { let n=0; for(let i=0;i<3e5;i++){ const s='k'+i+':'+(i*7); n+=s.split(':')[1].length; } return n; });
  t('js_map', () => { const m=new Map(); for(let i=0;i<1e6;i++){ m.set('k'+(i&65535),i); } return m.size; });
  t('js_sort', () => { const a=[]; let x=12345; for(let i=0;i<5e5;i++){ x=(x*1103515245+12345)&0x7fffffff; a.push(x);} a.sort((p,q)=>p-q); return a[0]; });
  t('js_json', () => { const o=[]; for(let i=0;i<2e4;i++)o.push({id:i,name:'n'+i,t:[1,2,3]}); for(let i=0;i<5;i++)JSON.parse(JSON.stringify(o)); });
  t('js_regexp', () => { const s='the quick brown fox '.repeat(2e4); let n=0; for(let i=0;i<10;i++) n+=(s.match(/q[a-z]+k/g)||[]).length; return n; });
  t('js_closures', () => { let s=0; const f=[1,2,3,4,5,6,7,8].map(k=>x=>x+k); for(let i=0;i<2e7;i++){ s=f[i&7](s)&1023; } return s; });
  // DOM through the bindings.
  const html = '<ul>' + '<li class=a><span>x</span><a href=#>y</a></li>'.repeat(2000) + '</ul>';
  t('dom_innerHTML', () => { const d=document.createElement('div'); for(let i=0;i<20;i++) d.innerHTML=html; });
  t('dom_createAppend', () => { const d=document.createElement('div'); for(let i=0;i<200000;i++){ const e=document.createElement('span'); e.setAttribute('class','x'+(i&7)); d.appendChild(e); if((i&1023)==1023) d.textContent=''; } });
  t('dom_textNodes', () => { const d=document.createElement('div'); for(let i=0;i<300000;i++){ d.appendChild(document.createTextNode('t'+i)); if((i&4095)==4095) d.textContent=''; } });
  t('dom_globalRead', () => { let n=0; for(let i=0;i<3e6;i++){ if(document) n++; } return n; });
  t('dom_getAttr', () => { const e=document.createElement('div'); e.setAttribute('id','abc'); let n=0; for(let i=0;i<3e6;i++){ n+=e.getAttribute('id').length; } return n; });
  t('dom_walk', () => { const d=document.createElement('div'); d.innerHTML=html; let n=0; for(let k=0;k<100;k++){ for(let c=d.firstChild.firstChild;c;c=c.nextSibling){ n+=c.firstChild.nodeType; } } return n; });
  t('dom_querySel', () => { const d=document.createElement('div'); d.innerHTML=html; let n=0; for(let k=0;k<100;k++){ n+=d.querySelectorAll('li.a > span').length; } return n; });
  return JSON.stringify(out);
})()
