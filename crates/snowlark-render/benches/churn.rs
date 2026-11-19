// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Speedometer's TodoMVC steps through the DOM API and the renderer, timed per pipeline phase.
//!
//! The document is a TodoMVC app (header, `ul.todo-list`, footer with the item count and filters) with
//! a stylesheet of about 50 rules (descendant, child, sibling, attribute, class and state selectors, a
//! `::after`, a transition-free `:hover`). Each iteration runs the three steps Speedometer's TodoMVC
//! suites run, each followed by one rendering update (restyle, box tree, layout, display list):
//!
//! 1. **add**: 100 `<li>` items appended as TodoMVC's views make them
//!    (`li > div.view > input.toggle + label > span + button.destroy`), the count updated;
//! 2. **toggle**: every item's checkbox checked and the `completed` class set on its `<li>`;
//! 3. **delete**: every item removed.
//!
//! Then, in a 5,000-element document, one element's class toggled (a colour, then a padding) and the
//! viewport width changed.
//!
//! Prints the median over the iterations of each phase per step:
//! `cargo bench --package snowlark-render --bench churn [-- <iterations>]`.

use std::sync::Arc;
use std::time::Instant;

use snowlark_dom::{Dom, NodeId};
use snowlark_net::{NetConfig, NetThread, Url};
use snowlark_paint::{Point, Size, TextEngine};
use snowlark_render::{Renderer, Timings};
use snowlark_style::DeviceConfig;

const VIEWPORT: Size = Size {
    width: 1024.0,
    height: 768.0,
};

const CSS: &str = r#"
html, body { margin: 0; padding: 0 }
button { margin: 0; padding: 0; border: 0; background: none; font-size: 100%; vertical-align: baseline; font-family: inherit; font-weight: inherit; color: inherit }
body { font: 14px "Helvetica Neue", Helvetica, Arial, sans-serif; line-height: 1.4em; background: #f5f5f5; color: #111; min-width: 230px; max-width: 550px; margin: 0 auto; font-weight: 300 }
.hidden { display: none }
.todoapp { background: #fff; margin: 130px 0 40px 0; position: relative; box-shadow: 0 2px 4px 0 rgba(0, 0, 0, 0.2), 0 25px 50px 0 rgba(0, 0, 0, 0.1) }
.todoapp input::placeholder { font-style: italic; font-weight: 400; color: rgba(0, 0, 0, 0.4) }
.todoapp h1 { position: absolute; top: -140px; width: 100%; font-size: 80px; font-weight: 200; text-align: center; color: #b83f45 }
.new-todo, .edit { position: relative; margin: 0; width: 100%; font-size: 24px; font-family: inherit; font-weight: inherit; line-height: 1.4em; color: inherit; padding: 6px; border: 1px solid #999; box-sizing: border-box }
.new-todo { padding: 16px 16px 16px 60px; height: 65px; border: none; background: rgba(0, 0, 0, 0.003); box-shadow: inset 0 -2px 1px rgba(0, 0, 0, 0.03) }
.main { position: relative; z-index: 2; border-top: 1px solid #e6e6e6 }
.toggle-all { width: 1px; height: 1px; border: none; opacity: 0; position: absolute; right: 100%; bottom: 100% }
.toggle-all + label { display: flex; align-items: center; justify-content: center; width: 45px; height: 65px; font-size: 0; position: absolute; top: -65px; left: -0 }
.toggle-all + label:before { content: "❯"; display: inline-block; font-size: 22px; color: #949494; padding: 10px 27px 10px 27px; transform: rotate(90deg) }
.toggle-all:checked + label:before { color: #484848 }
.todo-list { margin: 0; padding: 0; list-style: none }
.todo-list li { position: relative; font-size: 24px; border-bottom: 1px solid #ededed }
.todo-list li:last-child { border-bottom: none }
.todo-list li.editing { border-bottom: none; padding: 0 }
.todo-list li.editing .edit { display: block; width: calc(100% - 43px); padding: 12px 16px; margin: 0 0 0 43px }
.todo-list li.editing .view { display: none }
.todo-list li .toggle { text-align: center; width: 40px; height: auto; position: absolute; top: 0; bottom: 0; margin: auto 0; border: none; appearance: none }
.todo-list li .toggle { opacity: 0 }
.todo-list li .toggle + label { background-repeat: no-repeat; background-position: center left }
.todo-list li .toggle:checked + label { background-color: #f9fff9 }
.todo-list li label { word-break: break-all; padding: 15px 15px 15px 60px; display: block; line-height: 1.2; transition: color 0s }
.todo-list li label span { font-weight: 400 }
.todo-list li.completed label { color: #949494; text-decoration: line-through }
.todo-list li .destroy { display: none; position: absolute; top: 0; right: 10px; bottom: 0; width: 40px; height: 40px; margin: auto 0; font-size: 30px; color: #949494 }
.todo-list li .destroy:hover, .todo-list li .destroy:focus { color: #c18585 }
.todo-list li .destroy:after { content: "×"; display: block; height: 100% }
.todo-list li:hover .destroy { display: block }
.todo-list li .edit { display: none }
.todo-list li.editing:last-child { margin-bottom: -1px }
.footer { padding: 10px 15px; height: 20px; text-align: center; font-size: 15px; border-top: 1px solid #e6e6e6 }
.footer:before { content: ""; position: absolute; right: 0; bottom: 0; left: 0; height: 50px; overflow: hidden }
.todo-count { float: left; text-align: left }
.todo-count strong { font-weight: 300 }
.filters { margin: 0; padding: 0; list-style: none; position: absolute; right: 0; left: 0 }
.filters li { display: inline }
.filters li a { color: inherit; margin: 3px; padding: 3px 7px; text-decoration: none; border: 1px solid transparent; border-radius: 3px }
.filters li a:hover { border-color: #db7676 }
.filters li a.selected { border-color: #ce4646 }
.clear-completed, html .clear-completed:active { float: right; position: relative; line-height: 19px; text-decoration: none; cursor: pointer }
.clear-completed:hover { text-decoration: underline }
.info { margin: 65px auto 0; color: #4d4d4d; font-size: 11px; text-shadow: 0 1px 0 rgba(255, 255, 255, 0.5); text-align: center }
.info p { line-height: 1 }
.info a { color: inherit; text-decoration: none; font-weight: 400 }
.info a:hover { text-decoration: underline }
[data-id] > .view { min-height: 58px }
section.main > ul > li[data-id] { color: #333 }
footer.footer > span.todo-count:only-child { float: none }
body > section > footer > ul > li + li a { margin-left: 4px }
"#;

const BODY: &str = r##"<section class="todoapp">
  <header class="header"><h1>todos</h1><input class="new-todo" placeholder="What needs to be done?" autofocus></header>
  <section class="main">
    <input id="toggle-all" class="toggle-all" type="checkbox"><label for="toggle-all">Mark all as complete</label>
    <ul class="todo-list"></ul>
  </section>
  <footer class="footer">
    <span class="todo-count"><strong>0</strong> items left</span>
    <ul class="filters"><li><a class="selected" href="#/">All</a></li> <li><a href="#/active">Active</a></li> <li><a href="#/completed">Completed</a></li></ul>
    <button class="clear-completed">Clear completed</button>
  </footer>
</section>
<footer class="info"><p>Double-click to edit a todo</p><p>Part of <a href="http://todomvc.com">TodoMVC</a></p></footer>"##;

/// One step's phase times in ms.
#[derive(Clone, Copy, Default)]
struct Phases {
    style: f64,
    boxes: f64,
    layout: f64,
    paint: f64,
    /// Boxes laid out and text items shaped by the step's layout.
    laid_out: f64,
    shaped: f64,
}

impl Phases {
    fn of(r: &Renderer, before: &Timings) -> Phases {
        let stats = r.layout_stats();
        Phases {
            laid_out: f64::from(stats.laid_out),
            shaped: f64::from(stats.shaped),
            ..Phases::between(before, &r.timings())
        }
    }

    fn between(a: &Timings, b: &Timings) -> Phases {
        Phases {
            style: b.style_ms - a.style_ms,
            boxes: b.build_ms - a.build_ms,
            layout: b.layout_ms - a.layout_ms,
            paint: b.paint_ms - a.paint_ms,
            laid_out: 0.0,
            shaped: 0.0,
        }
    }

    fn total(&self) -> f64 {
        self.style + self.boxes + self.layout + self.paint
    }
}

struct App {
    list: NodeId,
    count: NodeId,
    items: Vec<NodeId>,
}

fn find(dom: &Dom, root: NodeId, class: &str) -> NodeId {
    dom.descendants(root)
        .find(|&n| {
            dom.is_element(n)
                && dom
                    .get_attribute(n, "class")
                    .is_some_and(|c| c.split_ascii_whitespace().any(|t| t == class))
        })
        .unwrap_or_else(|| panic!("no .{class}"))
}

fn element(dom: &mut Dom, doc: NodeId, name: &str, class: Option<&str>) -> NodeId {
    let e = dom.create_element(doc, name, None).unwrap();
    if let Some(c) = class {
        dom.set_attribute(e, "class", c).unwrap();
    }
    e
}

/// TodoMVC's item view.
fn add_item(dom: &mut Dom, doc: NodeId, app: &mut App, i: usize) {
    let li = element(dom, doc, "li", None);
    dom.set_attribute(li, "data-id", &i.to_string()).unwrap();
    let view = element(dom, doc, "div", Some("view"));
    let toggle = element(dom, doc, "input", Some("toggle"));
    dom.set_attribute(toggle, "type", "checkbox").unwrap();
    let label = element(dom, doc, "label", None);
    let span = element(dom, doc, "span", None);
    let text = dom.create_text_node(doc, &format!("Something to do {i}"));
    dom.append_child(span, text).unwrap();
    dom.append_child(label, span).unwrap();
    let rest = dom.create_text_node(doc, " (due today)");
    dom.append_child(label, rest).unwrap();
    let destroy = element(dom, doc, "button", Some("destroy"));
    dom.append_child(view, toggle).unwrap();
    dom.append_child(view, label).unwrap();
    dom.append_child(view, destroy).unwrap();
    dom.append_child(li, view).unwrap();
    dom.append_child(app.list, li).unwrap();
    app.items.push(li);
}

fn set_count(dom: &mut Dom, app: &App, n: usize) {
    let text = dom.first_child(app.count).unwrap();
    dom.set_data(text, &n.to_string());
}

/// Reports the DOM changes and runs one rendering update; returns its phase times.
fn update(r: &mut Renderer) -> Phases {
    let reactions = r.dom_mut().unwrap().take_reactions();
    let before = r.timings();
    r.dom_changed(&reactions);
    r.restyle();
    r.layout(VIEWPORT).unwrap();
    let list = r.paint(VIEWPORT, Point::ZERO);
    std::hint::black_box(list.len());
    Phases::of(r, &before)
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    if v.is_empty() {
        0.0
    } else {
        v[v.len() / 2]
    }
}

/// A 5,000-element document: sections of paragraphs with a bold span, a nested block and some floats
/// (the layout crate's `--layout-demo` document, as HTML and CSS).
fn big_document() -> (String, usize) {
    let mut html = String::from(
        "<!DOCTYPE html><html><head><style>
         html { font: 14px/1.3 \"Segoe UI\", sans-serif }
         body { margin: 8px }
         section { display: block; margin-bottom: 10px; padding: 4px }
         h2 { font-size: 18px; font-weight: 700; margin: 0 }
         p { margin: 0 0 6px 0 }
         .aside { float: right; width: 120px; height: 40px; background: #e8f0fe }
         .nested { padding-left: 10px }
         p.hot { color: #d93025 }
         p.wide { padding-left: 12px }
         </style></head><body>",
    );
    let mut elements = 5;
    let mut section = 0;
    while elements < 5000 {
        html.push_str(&format!("<section><h2>Section {section}</h2>"));
        elements += 2;
        for p in 0..6 {
            let id = if section == 40 && p == 0 {
                " id=target"
            } else {
                ""
            };
            html.push_str(&format!("<p{id}>"));
            elements += 1;
            if p == 0 && section % 3 == 0 {
                html.push_str("<div class=aside>aside</div>");
                elements += 1;
            }
            html.push_str(
                "Layout performance is measured, not assumed: this paragraph has some text,                  <b>a bold span</b> and more words so that it wraps across a few lines at this                  width.<div class=nested>A nested block.</div></p>",
            );
            elements += 2;
        }
        html.push_str("</section>");
        section += 1;
    }
    html.push_str("</body></html>");
    (html, elements)
}

/// One element's class toggled, and the viewport resized, in the 5,000-element document.
fn big(net: &NetThread, text: &Arc<TextEngine>, iterations: usize) {
    let mut r = Renderer::new(
        net.client(),
        text.clone(),
        DeviceConfig {
            viewport: (1000.0, 800.0),
            scripting: true,
            ..Default::default()
        },
    );
    let (html, elements) = big_document();
    let url = Url::parse("http://localhost/big.html").unwrap();
    let viewport = Size::new(1000.0, 800.0);
    let t = Instant::now();
    r.load_html(html.as_bytes(), &url, None);
    r.restyle();
    r.layout(viewport).unwrap();
    r.paint(viewport, Point::ZERO);
    println!(
        "
{elements}-element document: first render {:.2} ms",
        t.elapsed().as_secs_f64() * 1000.0
    );
    let doc = r.document().unwrap();
    let target = {
        let dom = r.dom().unwrap();
        dom.descendants(doc)
            .find(|&n| dom.is_element(n) && dom.get_attribute(n, "id") == Some("target"))
            .unwrap()
    };
    r.dom_mut().unwrap().set_reactions_enabled(true);
    println!(
        "{:<20} {:>9} {:>9} {:>9} {:>9} {:>9} {:>9} {:>7}",
        "step", "style", "box tree", "layout", "paint", "total", "laid out", "shaped"
    );
    for (name, class) in [("class: colour", "hot"), ("class: padding", "wide")] {
        let mut samples = Vec::new();
        for i in 0..iterations + 2 {
            let mut dom = r.dom_mut().unwrap();
            if i % 2 == 0 {
                dom.set_attribute(target, "class", class).unwrap();
            } else {
                dom.remove_attribute(target, "class");
            }
            let reactions = dom.take_reactions();
            drop(dom);
            let before = r.timings();
            r.dom_changed(&reactions);
            r.restyle();
            r.layout(viewport).unwrap();
            std::hint::black_box(r.paint(viewport, Point::ZERO).len());
            if i >= 2 {
                samples.push(Phases::of(&r, &before));
            }
        }
        print_row(name, &samples);
    }
    let mut samples = Vec::new();
    for i in 0..iterations + 2 {
        let v = Size::new(if i % 2 == 0 { 900.0 } else { 1000.0 }, 800.0);
        let before = r.timings();
        r.set_viewport(v);
        r.restyle();
        r.layout(v).unwrap();
        std::hint::black_box(r.paint(v, Point::ZERO).len());
        if i >= 2 {
            samples.push(Phases::of(&r, &before));
        }
    }
    print_row("viewport width", &samples);
}

fn print_row(name: &str, s: &[Phases]) -> f64 {
    let col = |f: fn(&Phases) -> f64| median(s.iter().map(f).collect());
    let total = col(Phases::total);
    println!(
        "{name:<20} {:>9.3} {:>9.3} {:>9.3} {:>9.3} {:>9.3} {:>9} {:>7}",
        col(|p| p.style),
        col(|p| p.boxes),
        col(|p| p.layout),
        col(|p| p.paint),
        total,
        col(|p| p.laid_out),
        col(|p| p.shaped),
    );
    total
}

fn main() {
    let iterations: usize = std::env::args()
        .skip(1)
        .find_map(|a| a.parse().ok())
        .unwrap_or(20);
    let net = NetThread::start(NetConfig::default()).unwrap();
    let text = Arc::new(TextEngine::new().unwrap());
    let mut r = Renderer::new(
        net.client(),
        text.clone(),
        DeviceConfig {
            viewport: (VIEWPORT.width, VIEWPORT.height),
            scripting: true,
            ..Default::default()
        },
    );
    let html = format!(
        "<!DOCTYPE html><html><head><title>TodoMVC</title><style>{CSS}</style></head><body>{BODY}</body></html>"
    );
    let url = Url::parse("http://localhost/todomvc/index.html").unwrap();
    let t = Instant::now();
    r.load_html(html.as_bytes(), &url, None);
    r.restyle();
    r.layout(VIEWPORT).unwrap();
    r.paint(VIEWPORT, Point::ZERO);
    println!(
        "TodoMVC churn: first render {:.2} ms; {iterations} iterations of add 100 / toggle 100 / delete 100",
        t.elapsed().as_secs_f64() * 1000.0
    );

    let doc = r.document().unwrap();
    let (list, count) = {
        let dom = r.dom().unwrap();
        let list = find(&dom, doc, "todo-list");
        let count = dom.first_child(find(&dom, doc, "todo-count")).unwrap();
        (list, count)
    };
    let mut app = App {
        list,
        count,
        items: Vec::new(),
    };
    r.dom_mut().unwrap().set_reactions_enabled(true);
    let names = ["add", "toggle", "delete"];
    let mut samples: Vec<Vec<Phases>> = vec![Vec::new(); 3];
    let warmup = 3;
    for iteration in 0..warmup + iterations {
        let mut steps = [Phases::default(); 3];
        // 1. Add 100 items.
        {
            let mut dom = r.dom_mut().unwrap();
            for i in 0..100 {
                add_item(&mut dom, doc, &mut app, i);
                set_count(&mut dom, &app, app.items.len());
            }
        }
        steps[0] = update(&mut r);
        // 2. Complete every item.
        {
            let mut dom = r.dom_mut().unwrap();
            for (n, &li) in app.items.iter().enumerate() {
                let toggle = dom.first_child(dom.first_child(li).unwrap()).unwrap();
                dom.set_attribute(toggle, "checked", "").unwrap();
                dom.set_attribute(li, "class", "completed").unwrap();
                let text = dom.first_child(app.count).unwrap();
                dom.set_data(text, &(app.items.len() - n - 1).to_string());
            }
        }
        steps[1] = update(&mut r);
        // 3. Delete every item.
        {
            let mut dom = r.dom_mut().unwrap();
            for li in app.items.drain(..) {
                dom.remove_child(app.list, li).unwrap();
            }
            set_count(&mut dom, &app, 0);
        }
        steps[2] = update(&mut r);
        if iteration >= warmup {
            for (s, p) in samples.iter_mut().zip(steps) {
                s.push(p);
            }
        }
    }
    println!(
        "{:<20} {:>9} {:>9} {:>9} {:>9} {:>9} {:>9} {:>7}",
        "step", "style", "box tree", "layout", "paint", "total", "laid out", "shaped"
    );
    let mut sum = 0.0;
    for (name, s) in names.iter().zip(&samples) {
        sum += print_row(name, s);
    }
    println!("all steps (sum of medians, ms): {sum:.3}");
    big(&net, &text, iterations);
}
