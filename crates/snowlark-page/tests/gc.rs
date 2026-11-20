// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Wrapper lifetime (ARCHITECTURE.md "Wrappers and GC"): connected nodes keep their wrappers, detached
//! subtrees' wrappers live and die as a group, unreferenced detached nodes and objects are freed, and a
//! script that makes a million detached elements runs in a bounded V8 heap.

use std::time::Duration;

use snowlark_net::{NetConfig, NetThread};
use snowlark_page::{Page, PageOptions, RenderSetup};

fn page() -> Page {
    let mut page = Page::new(PageOptions::default());
    page.load_html(
        "<!doctype html><body><div id=host></div></body>",
        &url::Url::parse("http://example.test/").unwrap(),
    );
    page
}

fn eval(page: &mut Page, js: &str) -> String {
    match page.eval(js, "test.js") {
        Ok(v) => v,
        Err(e) => panic!("{js}: threw {e}"),
    }
}

/// Collects until nothing more is freed.
fn collect(page: &mut Page) {
    for _ in 0..4 {
        assert!(page.collect_garbage());
    }
}

/// V8's foreground tasks (its garbage collector's jobs, a `FinalizationRegistry`'s cleanup) are low
/// priority: a task the page already has ready runs first.
#[test]
fn v8_tasks_wait_for_the_pages_ready_task() {
    let mut page = page();
    eval(
        &mut page,
        "var log = [];
         var registry = new FinalizationRegistry(() => log.push('cleanup'));
         (() => { registry.register({}, 0); })();",
    );
    // (idle: the loop ran V8's tasks just now, so they are not overdue below)
    page.run_until_settled(Duration::from_secs(1));
    eval(&mut page, "setTimeout(() => log.push('timeout'), 0);");
    std::thread::sleep(Duration::from_millis(2));
    // The object dies: V8 posts the registry's cleanup as a foreground task.
    assert!(page.collect_garbage());
    for _ in 0..200 {
        page.turn();
        if eval(&mut page, "log.length") == "2" {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(eval(&mut page, "log.join()"), "timeout,cleanup");
}

#[test]
fn a_million_detached_elements_in_a_bounded_heap() {
    const LIMIT: usize = 48 << 20;
    let mut page = Page::new(PageOptions {
        heap_limit: Some(LIMIT),
        ..Default::default()
    });
    page.load_html(
        "<!doctype html><body>",
        &url::Url::parse("http://example.test/").unwrap(),
    );
    collect(&mut page);
    let (nodes_before, _) = page.engine().live_counts();
    let started = std::time::Instant::now();
    // Each element gets an expando property, and a few stay referenced: those keep theirs.
    let r = eval(
        &mut page,
        "var kept = [];
         for (let i = 0; i < 1000000; i++) {
           const d = document.createElement('div');
           d.n = i;
           d.appendChild(document.createElement('span')).m = i;
           if (i % 100000 === 0) kept.push(d);
         }
         kept.map(d => d.n + d.firstChild.m).join()",
    );
    let elapsed = started.elapsed();
    assert_eq!(
        r,
        "0,200000,400000,600000,800000,1000000,1200000,1400000,1600000,1800000"
    );
    assert!(
        !page.heap_limit_reached(),
        "the heap stayed under its limit"
    );
    let heap = page.heap_used();
    assert!(heap < LIMIT, "heap {heap}");
    collect(&mut page);
    let (nodes_after, _) = page.engine().live_counts();
    // The kept elements and their children are all that is left.
    assert_eq!(nodes_after, nodes_before + 20, "nodes after the collection");
    assert_eq!(
        eval(&mut page, "kept.map(d => d.n + d.firstChild.m).join()"),
        r
    );
    let stats = page.engine().gc_stats();
    eprintln!(
        "1M detached elements (2M nodes): {elapsed:?}, V8 heap after {} KiB, freed {} nodes",
        heap / 1024,
        stats.nodes_freed
    );
}

#[test]
fn detached_groups_keep_expandos_together() {
    let mut page = page();
    eval(
        &mut page,
        "// Only the root is referenced: the child's expando stays.
         window.root = document.createElement('div');
         (() => { const c = document.createElement('span'); c.foo = 'child'; root.appendChild(c); })();
         // Only a grandchild is referenced: the root's and the middle node's stay.
         (() => {
           const r = document.createElement('section'); r.bar = 'root';
           const m = r.appendChild(document.createElement('p')); m.mid = 'middle';
           window.leaf = m.appendChild(document.createElement('b'));
         })();
         // A connected node nobody references keeps its expando.
         (() => { const p = document.createElement('p'); p.baz = 'connected'; document.body.appendChild(p); })();
         // A shadow tree belongs to its host's group, a template's contents to the template's.
         (() => {
           const h = document.createElement('div'); h.attachShadow({mode: 'open'}).appendChild(document.createElement('i')).sh = 'shadow';
           window.hostRef = h;
           const t = document.createElement('template'); t.content.appendChild(document.createElement('u')).tc = 'template';
           window.templateRef = t;
         })();
         // An attribute node belongs to its element's group.
         (() => { const e = document.createElement('div'); e.setAttribute('x', '1'); e.getAttributeNode('x').ax = 'attr'; window.attrOwner = e; })();
         // Unreferenced detached elements go.
         window.weak = new WeakRef(document.createElement('div'));
         'ok'",
    );
    let (nodes_before, _) = page.engine().live_counts();
    collect(&mut page);
    assert_eq!(
        eval(
            &mut page,
            "[root.firstChild.foo, leaf.parentNode.mid, leaf.parentNode.parentNode.bar,
              document.body.lastChild.baz, hostRef.shadowRoot.firstChild.sh,
              templateRef.content.firstChild.tc, attrOwner.getAttributeNode('x').ax,
              weak.deref()].join()"
        ),
        "child,middle,root,connected,shadow,template,attr,"
    );
    let (nodes_after, _) = page.engine().live_counts();
    assert!(nodes_after < nodes_before, "the unreferenced div was freed");
}

#[test]
fn removed_subtrees_are_freed() {
    let mut page = page();
    collect(&mut page);
    let (base, _) = page.engine().live_counts();
    eval(
        &mut page,
        "const h = document.getElementById('host');
         for (let i = 0; i < 100; i++) h.innerHTML = '<ul>' + '<li>x</li>'.repeat(50) + '</ul>';
         h.innerHTML = ''; 'ok'",
    );
    collect(&mut page);
    let (after, _) = page.engine().live_counts();
    assert!(after <= base + 4, "{after} nodes, {base} before");
}

#[test]
fn owned_objects_keep_their_element() {
    let mut page = page();
    eval(
        &mut page,
        "window.cl = (() => { const e = document.createElement('div'); e.flag = 'owner'; e.className = 'a'; return e.classList; })();
         window.ds = (() => { const e = document.createElement('div'); e.dataset.k = 'v'; return e.dataset; })();
         window.st = (() => { const e = document.createElement('div'); e.style.color = 'red'; return e.style; })();
         'ok'",
    );
    collect(&mut page);
    assert_eq!(
        eval(&mut page, "cl.add('b'); [cl.value, ds.k, st.color].join()"),
        "a b,v,red"
    );
    let (nodes, _) = page.engine().live_counts();
    eval(&mut page, "cl = ds = st = null; 'ok'");
    collect(&mut page);
    let (after, _) = page.engine().live_counts();
    assert!(after < nodes, "the elements went with their objects");
}

#[test]
fn rust_references_pin_groups() {
    let mut page = page();
    eval(
        &mut page,
        "window.range = (() => {
           const d = document.createElement('div'); d.tag = 'range';
           const r = document.createRange(); r.selectNodeContents(d); return r;
         })();
         window.walker = (() => {
           const d = document.createElement('div'); d.tag = 'walker';
           d.appendChild(document.createElement('span'));
           return document.createTreeWalker(d);
         })();
         window.records = [];
         window.mo = new MutationObserver(rs => records.push(...rs));
         (() => {
           const d = document.createElement('div'); mo.observe(d, {childList: true});
           const c = d.appendChild(document.createElement('em')); c.tag = 'record';
           d.removeChild(c);
         })();
         'ok'",
    );
    // The mutation records are delivered in a microtask.
    page.run_until_settled(Duration::from_secs(1));
    collect(&mut page);
    assert_eq!(
        eval(
            &mut page,
            "[range.startContainer.tag, walker.root.tag, records[1].removedNodes[0].tag].join()"
        ),
        "range,walker,record"
    );
}

#[test]
fn events_and_rects_are_freed() {
    let mut page = page();
    collect(&mut page);
    let (_, before) = page.engine().live_counts();
    eval(
        &mut page,
        "const t = new EventTarget(); let n = 0; t.addEventListener('x', () => n++);
         for (let i = 0; i < 10000; i++) { t.dispatchEvent(new Event('x')); document.body.getBoundingClientRect(); }
         n",
    );
    collect(&mut page);
    let (_, after) = page.engine().live_counts();
    assert!(after < before + 100, "{after} objects, {before} before");
}

#[test]
fn freed_nodes_leave_the_renderer() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        render: Some(RenderSetup::new().unwrap()),
        ..Default::default()
    });
    page.load_html(
        "<!doctype html><body><div id=a><p>one</p><input value=x><img src='data:,'></div>",
        &url::Url::parse("http://example.test/").unwrap(),
    );
    assert!(page.run_until_settled(Duration::from_secs(5)));
    eval(
        &mut page,
        "document.getElementById('a').getBoundingClientRect();
         document.getElementById('a').remove(); 'ok'",
    );
    collect(&mut page);
    assert_eq!(
        eval(&mut page, "document.body.getBoundingClientRect().height"),
        "0"
    );
    assert!(page.engine().gc_stats().nodes_freed > 0);
}

#[test]
fn documents_without_a_browsing_context_live_while_script_reaches_them() {
    let mut page = page();
    collect(&mut page);
    let (base, _) = page.engine().live_counts();
    // What jQuery's parseHTML does per call: a document of its own, parsed into and dropped.
    eval(
        &mut page,
        "for (let i = 0; i < 50; i++) {
           const b = document.implementation.createHTMLDocument('').body;
           b.innerHTML = '<form></form><form></form>';
           new DOMParser().parseFromString('<p>x</p>', 'text/html').body.firstChild;
         }
         'ok'",
    );
    collect(&mut page);
    let (after, _) = page.engine().live_counts();
    assert!(after <= base + 4, "{after} nodes, {base} before");
    // One that script keeps stays, with its state (its title, its body's content).
    eval(
        &mut page,
        "window.kept = document.implementation.createHTMLDocument('t');
         kept.body.innerHTML = '<p>kept</p>'; 'ok'",
    );
    collect(&mut page);
    assert_eq!(
        eval(&mut page, "kept.title + ' ' + kept.body.textContent"),
        "t kept"
    );
}

#[test]
fn listeners_that_reach_their_target_do_not_keep_it() {
    let mut page = page();
    collect(&mut page);
    let (base, _) = page.engine().live_counts();
    // Each element's listener and handler close over the element: only the element's wrapper
    // keeps them, so the detached elements go.
    eval(
        &mut page,
        "for (let i = 0; i < 100; i++) {
           const d = document.createElement('div');
           d.appendChild(document.createElement('span'));
           d.addEventListener('click', () => d.classList.toggle('on'));
           d.onclick = () => d.firstChild;
         }
         'ok'",
    );
    collect(&mut page);
    let (after, _) = page.engine().live_counts();
    assert!(after <= base + 4, "{after} nodes, {base} before");
    // A referenced one keeps its listeners, which still run, also after a pass made them traced
    // by its wrapper, and after it is connected again.
    eval(
        &mut page,
        "window.kept = document.createElement('div');
         kept.addEventListener('click', () => kept.textContent += 'a');
         kept.onclick = () => kept.textContent += 'b'; 'ok'",
    );
    collect(&mut page);
    eval(
        &mut page,
        "kept.addEventListener('click', () => kept.textContent += 'c'); 'ok'",
    );
    collect(&mut page);
    assert_eq!(eval(&mut page, "kept.click(); kept.textContent"), "abc");
    eval(&mut page, "document.body.appendChild(kept); 'ok'");
    collect(&mut page);
    assert_eq!(eval(&mut page, "kept.click(); kept.textContent"), "abcabc");
    // Removing a traced listener finds it.
    eval(
        &mut page,
        "window.f = () => kept.textContent += 'x';
         kept.remove(); kept.addEventListener('click', f); 'ok'",
    );
    collect(&mut page);
    assert_eq!(
        eval(
            &mut page,
            "kept.removeEventListener('click', f); kept.textContent = ''; kept.click(); kept.textContent"
        ),
        "abc"
    );
}

/// A `DOMParser` holds nothing but its realm (DOM Parsing §3.1): it lives while script reaches it, as
/// Blink's (garbage collected with its wrapper), not for the page's life. TodoMVC-ES5 makes one per render.
#[test]
fn dom_parsers_are_freed() {
    let mut page = page();
    collect(&mut page);
    let (_, before) = page.engine().live_counts();
    eval(
        &mut page,
        "let n = 0;
         for (let i = 0; i < 1000; i++) n += new DOMParser().parseFromString('<p>x</p>', 'text/html').body.childNodes.length;
         window.kept = new DOMParser();
         n",
    );
    collect(&mut page);
    let (_, after) = page.engine().live_counts();
    assert!(after < before + 50, "{after} objects, {before} before");
    assert_eq!(
        eval(
            &mut page,
            "kept.parseFromString('<b>y</b>', 'text/html').body.textContent"
        ),
        "y"
    );
}

fn rendered_page() -> (NetThread, Page) {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        viewport: (800, 600),
        render: Some(RenderSetup::new().unwrap()),
        ..Default::default()
    });
    page.load_html(
        "<!doctype html><p>x</p>",
        &url::Url::parse("http://example.test/").unwrap(),
    );
    assert!(page.run_until_settled(Duration::from_secs(5)));
    (net, page)
}

/// A `MediaQueryList` (CSSOM View §4.2) lives while script reaches it or it has a `change`
/// listener: nothing else can observe it. The lists a page makes to read `matchMedia(..).matches`,
/// once a frame on some pages, are freed; one that listens stays, reached or not, and hears its
/// change.
#[test]
fn media_query_lists_live_while_reached_or_listening() {
    let (_net, mut page) = rendered_page();
    collect(&mut page);
    let (_, before) = page.engine().live_counts();
    let r = eval(
        &mut page,
        "window.changes = [];
         let n = 0;
         for (let i = 0; i < 1000; i++) n += matchMedia('(min-width: 700px)').matches;
         (() => {
           matchMedia('(min-width: 700px)')
             .addEventListener('change', e => changes.push('listener ' + e.matches));
           matchMedia('(min-width: 600px)').onchange = e => changes.push('handler ' + e.matches);
           // (one that stopped listening goes)
           const f = () => changes.push('removed');
           const gone = matchMedia('(min-width: 650px)');
           gone.addEventListener('change', f);
           gone.removeEventListener('change', f);
         })();
         window.kept = matchMedia('(min-width: 750px)');
         n",
    );
    assert_eq!(r, "1000");
    collect(&mut page);
    let (_, after) = page.engine().live_counts();
    assert!(after < before + 20, "{after} objects, {before} before");
    assert_eq!(page.engine().media_list_counts().0, 3);
    // A list whose wrapper a pass made weak is kept from its first listener on.
    eval(
        &mut page,
        "kept.addEventListener('change', e => changes.push('kept ' + e.matches));
         kept = null; 'ok'",
    );
    collect(&mut page);
    assert_eq!(page.engine().media_list_counts().0, 3);
    page.engine()
        .set_viewport(snowlark_paint::Size::new(500.0, 400.0));
    page.run_until_settled(Duration::from_secs(2));
    assert_eq!(
        eval(&mut page, "changes.join()"),
        "listener false,handler false,kept false"
    );
}

/// HTML "update the rendering" evaluates the media query lists and reports the changes. A list
/// matches as it did while nothing a media query reads has changed, so only a change of those
/// (here the viewport) makes a rendering update evaluate the lists again: a page that holds many
/// pays nothing for them in its other frames.
#[test]
fn rendering_updates_evaluate_media_query_lists_when_their_media_changed() {
    let (_net, mut page) = rendered_page();
    eval(
        &mut page,
        "window.changes = [];
         window.lists = [];
         for (let i = 0; i < 100; i++) lists.push(matchMedia(`(min-width: ${700 + i}px)`));
         lists[0].onchange = e => changes.push(e.media + ' ' + e.matches);
         'ok'",
    );
    let (lists, evaluated) = page.engine().media_list_counts();
    assert_eq!(lists, 100);
    for i in 0..5 {
        eval(
            &mut page,
            &format!("document.body.firstChild.textContent = 'frame {i}'; 'ok'"),
        );
        page.update_the_rendering();
    }
    assert_eq!(page.engine().media_list_counts().1, evaluated);
    page.engine()
        .set_viewport(snowlark_paint::Size::new(750.0, 400.0));
    page.update_the_rendering();
    assert_eq!(page.engine().media_list_counts().1, evaluated + 100);
    page.update_the_rendering();
    assert_eq!(page.engine().media_list_counts().1, evaluated + 100);
    // The state a change is reported against is the one of the last report, whatever was read
    // since.
    assert_eq!(
        eval(
            &mut page,
            "[changes.join(), lists[49].matches, lists[50].matches, lists[51].matches].join()"
        ),
        ",true,true,false"
    );
    page.engine()
        .set_viewport(snowlark_paint::Size::new(650.0, 400.0));
    assert_eq!(eval(&mut page, "lists[0].matches"), "false");
    page.update_the_rendering();
    assert_eq!(
        eval(&mut page, "changes.join()"),
        "(min-width: 700px) false"
    );
}

/// A CSS animation's objects (CSS Animations 2 §5: its `CSSAnimation` and `KeyframeEffect`) live while
/// the animation is relevant or script reaches them; once its element is gone and nothing refers to
/// them, both go.
#[test]
fn css_animations_of_removed_elements_are_freed() {
    let mut page = page();
    eval(
        &mut page,
        "const s = document.createElement('style');
         s.textContent = '@keyframes spin { from { opacity: 0 } to { opacity: 1 } } .a { animation: spin 1s infinite }';
         document.head.appendChild(s); 'ok'",
    );
    collect(&mut page);
    let (_, before) = page.engine().live_counts();
    eval(
        &mut page,
        "let n = 0;
         for (let i = 0; i < 1000; i++) {
           const d = document.body.appendChild(document.createElement('div'));
           d.className = 'a';
           n += d.getAnimations().length;
           d.remove();
         }
         n",
    );
    collect(&mut page);
    let (_, after) = page.engine().live_counts();
    assert!(after < before + 50, "{after} objects, {before} before");
}

/// CSS transitions' objects (CSS Transitions 2 §3: `CSSTransition` and its `KeyframeEffect`) of removed
/// elements go too, whether script asked for them or only their events needed them.
#[test]
fn css_transitions_of_removed_elements_are_freed() {
    let mut page = page();
    eval(
        &mut page,
        "const s = document.createElement('style');
         s.textContent = 'div { color: red; transition: color 0.4s } div.done { color: blue }';
         document.head.appendChild(s); 'ok'",
    );
    collect(&mut page);
    let (_, before) = page.engine().live_counts();
    eval(
        &mut page,
        "window.ds = [];
         for (let i = 0; i < 1000; i++) {
           const d = document.body.appendChild(document.createElement('div'));
           getComputedStyle(d).color;
           d.className = 'done';
           getComputedStyle(d).color;
           if (i % 2) d.getAnimations();
           ds.push(d);
         }
         'ok'",
    );
    eval(&mut page, "for (const d of ds) d.remove(); ds = null; 'ok'");
    collect(&mut page);
    let (_, after) = page.engine().live_counts();
    assert!(after < before + 50, "{after} objects, {before} before");
}

/// Transitions of elements that came from another document (adopted from a `DOMParser` document, as
/// TodoMVC-ES5 renders, or imported from a template's contents) go with them too.
#[test]
fn transitions_of_adopted_elements_are_freed() {
    for how in ["adopt", "import"] {
        let mut page = page();
        eval(
            &mut page,
            "const s = document.createElement('style');
             s.textContent = 'li { color: red; transition: color 10s } li.done { color: blue }';
             document.head.appendChild(s);
             window.list = document.body.appendChild(document.createElement('ul'));
             window.tpl = document.createElement('template'); tpl.innerHTML = '<li>t</li>'; 'ok'",
        );
        collect(&mut page);
        let (_, before) = page.engine().live_counts();
        let make = if how == "adopt" {
            "list.replaceChildren(...new DOMParser().parseFromString('<li>x</li>', 'text/html').body.childNodes)"
        } else {
            "list.replaceChildren(document.importNode(tpl.content, true))"
        };
        eval(
            &mut page,
            &format!(
                "for (let i = 0; i < 300; i++) {{
                   {make};
                   getComputedStyle(list.firstChild).color;
                   list.firstChild.className = 'done';
                   getComputedStyle(list.firstChild).color;
                 }}
                 list.replaceChildren(); 'ok'"
            ),
        );
        collect(&mut page);
        let (_, after) = page.engine().live_counts();
        eprintln!("{how}: {before} objects before, {after} after");
        if after >= before + 20 {
            eprintln!("{}", page.engine().live_census().join("\n"));
        }
        assert!(
            after < before + 20,
            "{how}: {after} objects, {before} before"
        );
    }
}

/// Removes `n` elements in one task and returns how many collections ran after it.
fn collections_after_removing(page: &mut Page, n: usize) -> u64 {
    collect(page);
    let before = page.engine().gc_stats().collections;
    eval(
        page,
        &format!(
            "setTimeout(() => {{ for (let i = 0; i < {n}; i++) {{\
             const d = document.createElement('div'); document.body.append(d); d.remove(); }} }}); 'ok'"
        ),
    );
    page.run_until_settled(Duration::from_secs(5));
    page.engine().gc_stats().collections - before
}

/// Runs `body` in one task that queues another (so the loop stays busy after it) and returns how
/// much `count` grew in the turn of the event loop that ran it, and once the loop was idle after it.
fn in_the_turn_and_idle(page: &mut Page, body: &str, count: impl Fn(&Page) -> u64) -> (u64, u64) {
    let (in_turn, next, idle) = in_the_turn_the_next_and_idle(page, body, count);
    (in_turn, next + idle)
}

/// [`in_the_turn_and_idle`], with the growth in the next turn (which runs the queued task, after
/// which none is ready) apart.
fn in_the_turn_the_next_and_idle(
    page: &mut Page,
    body: &str,
    count: impl Fn(&Page) -> u64,
) -> (u64, u64, u64) {
    collect(page);
    eval(
        page,
        &format!(
            "window.done = false; window.channel = new MessageChannel();             channel.port1.onmessage = () => {{}};             setTimeout(() => {{ {body}; channel.port2.postMessage(0); window.done = true; }}); 'ok'"
        ),
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let in_turn = loop {
        let before = count(page);
        page.turn();
        if eval(page, "String(window.done)") == "true" {
            break count(page) - before;
        }
        assert!(std::time::Instant::now() < deadline, "the task never ran");
    };
    let before = count(page);
    page.turn();
    let next = count(page) - before;
    let before = count(page);
    page.run_until_settled(Duration::from_secs(5));
    (in_turn, next, count(page) - before)
}

/// [`in_the_turn_and_idle`] for collections, with `n` elements removed.
fn collections_in_the_turn_and_idle(page: &mut Page, n: usize) -> (u64, u64) {
    in_the_turn_and_idle(
        page,
        &format!(
            "for (let i = 0; i < {n}; i++) {{             const d = document.createElement('div'); document.body.append(d); d.remove(); }}"
        ),
        |p| p.engine().gc_stats().collections,
    )
}

/// [`in_the_turn_the_next_and_idle`] for collections, with `n` elements removed.
fn collections_in_the_turn_the_next_and_idle(page: &mut Page, n: usize) -> (u64, u64, u64) {
    in_the_turn_the_next_and_idle(
        page,
        &format!(
            "for (let i = 0; i < {n}; i++) {{             const d = document.createElement('div'); document.body.append(d); d.remove(); }}"
        ),
        |p| p.engine().gc_stats().collections,
    )
}

/// [`in_the_turn_and_idle`] for passes over the pending wrappers, with `n` new element wrappers.
fn passes_in_the_turn_and_idle(page: &mut Page, n: usize) -> (u64, u64) {
    in_the_turn_and_idle(
        page,
        &format!(
            "window.kept = []; for (let i = 0; i < {n}; i++) kept.push(document.createElement('div'))"
        ),
        |p| p.engine().gc_stats().passes,
    )
}

/// While script runs, V8's interrupt stops it for a pass over the new wrappers only once four times
/// the pass threshold wait; fewer wait for the loop's idle time.
#[test]
fn a_pass_waits_for_the_loop_to_be_idle() {
    let mut page = page();
    let (in_turn, idle) = passes_in_the_turn_and_idle(&mut page, 6_000);
    assert_eq!(in_turn, 0, "6,000 new wrappers wait while the loop is busy");
    assert!(idle > 0, "and get a pass once it is idle");
    let (in_turn, _) = passes_in_the_turn_and_idle(&mut page, 20_000);
    assert!(in_turn > 0, "20,000 get one in the task");
}

/// The time of V8's interrupt passes inside script, of their snapshots and the snapshots' walk of
/// every object's references, and of the collections after a task and in idle time, are kept.
#[test]
fn passes_and_collections_are_timed() {
    let mut page = page();
    let before = page.engine().gc_stats();
    let (in_turn, _) = passes_in_the_turn_and_idle(&mut page, 20_000);
    assert!(in_turn > 0, "20,000 new wrappers get a pass in the task");
    let s = page.engine().gc_stats();
    let passes = s.interrupt_passes - before.interrupt_passes;
    assert!(passes > 0 && s.interrupts - before.interrupts >= passes);
    assert!(
        s.interrupt_wrappers - before.interrupt_wrappers >= 16_384,
        "a pass begins at four times the threshold"
    );
    let ns = s.interrupt_ns - before.interrupt_ns;
    let snapshot = s.interrupt_snapshot_ns - before.interrupt_snapshot_ns;
    let refs = s.interrupt_refs_ns - before.interrupt_refs_ns;
    assert!(
        ns >= snapshot && snapshot >= refs && refs > 0,
        "{ns} {snapshot} {refs}"
    );

    let before = page.engine().gc_stats();
    let (_, next, _) = collections_in_the_turn_the_next_and_idle(&mut page, 5_000);
    assert!(next > 0);
    assert!(page.engine().gc_stats().after_task_ns > before.after_task_ns);
    let before = page.engine().gc_stats();
    let (in_turn, idle) = collections_in_the_turn_and_idle(&mut page, 2_000);
    assert!(in_turn == 0 && idle > 0);
    let s = page.engine().gc_stats();
    assert!(s.idle_ns > before.idle_ns);
}

/// A harness that watches the `performance.mark` calls gets the names of those made since it last
/// asked; the page sees no change.
#[test]
fn a_harness_sees_the_performance_marks_made() {
    let mut page = page();
    eval(&mut page, "performance.mark('before')");
    assert_eq!(page.engine().take_watched_marks(), None);
    page.engine().watch_marks();
    assert_eq!(page.engine().take_watched_marks(), Some(vec![]));
    let entries = eval(
        &mut page,
        "performance.mark('a'); performance.mark('b.c-sync-end'); \
         performance.getEntriesByType('mark').map(m => m.name).join()",
    );
    assert_eq!(entries, "before,a,b.c-sync-end");
    assert_eq!(
        page.engine().take_watched_marks(),
        Some(vec!["a".to_owned(), "b.c-sync-end".to_owned()])
    );
    assert_eq!(page.engine().take_watched_marks(), Some(vec![]));
}

/// A busy loop leaves to its idle time a collection that only a backlog of removed subtrees asks
/// for, as Chromium's collectors work in idle tasks between frames, and collects without waiting
/// for idle time only when four times that waits, after the first task that leaves none ready.
#[test]
fn a_backlog_is_collected_when_the_loop_is_idle() {
    let mut page = page();
    let (in_turn, idle) = collections_in_the_turn_and_idle(&mut page, 2_000);
    assert_eq!(
        in_turn, 0,
        "2,000 removed subtrees wait while the loop is busy"
    );
    assert!(idle > 0, "and are collected once it is idle");
    let (_, next, _) = collections_in_the_turn_the_next_and_idle(&mut page, 5_000);
    assert!(next > 0, "5,000 are collected after the next task");
}

/// A due collection does not go ahead of a task that is ready to run: it waits for the first turn
/// after which none is ready (Chromium's collectors run their work that no allocation forces as
/// idle tasks, after every other task).
#[test]
fn a_collection_waits_for_the_task_that_is_ready() {
    let mut page = page();
    let deferred = |p: &Page| p.engine().gc_stats().deferred;
    let before = deferred(&page);
    let (in_turn, next, _) = collections_in_the_turn_the_next_and_idle(&mut page, 5_000);
    assert_eq!(in_turn, 0, "the queued message runs first");
    assert!(deferred(&page) > before, "the collection waited for it");
    assert!(next > 0, "and ran after it");
}

/// A collection waits for ready tasks only so long: a page that always has one is still collected.
#[test]
fn a_collection_waits_for_ready_tasks_only_so_long() {
    let mut page = page();
    collect(&mut page);
    let before = page.engine().gc_stats().collections;
    eval(
        &mut page,
        "window.stop = false; const c = new MessageChannel(); \
         c.port1.onmessage = () => { if (!window.stop) c.port2.postMessage(0); }; \
         setTimeout(() => { for (let i = 0; i < 5000; i++) { \
         const d = document.createElement('div'); document.body.append(d); d.remove(); } \
         c.port2.postMessage(0); }); 'ok'",
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while page.engine().gc_stats().collections == before {
        page.turn();
        assert!(
            std::time::Instant::now() < deadline,
            "never collected while a task was always ready"
        );
    }
    eval(&mut page, "window.stop = true; 'ok'");
}

/// A collection after a task waits for a backlog of candidates in proportion to what the last
/// snapshot walked, as V8 sizes its next collection by the heap: a page whose engine holds many
/// objects is not collected after every 1,024 removed subtrees, a small page still is.
#[test]
fn collections_wait_for_a_backlog_in_proportion_to_what_the_page_holds() {
    let mut small = page();
    assert!(
        collections_after_removing(&mut small, 2_000) > 0,
        "a small page collects after 2,000 removed subtrees"
    );
    let mut large = page();
    eval(
        &mut large,
        "window.kept = Array.from({length: 100000}, () => new Event('x')); 'ok'",
    );
    assert_eq!(
        collections_after_removing(&mut large, 2_000),
        0,
        "a page holding 100,000 objects waits for more"
    );
    assert!(
        collections_after_removing(&mut large, 40_000) > 0,
        "and collects once the backlog is in proportion"
    );
}
