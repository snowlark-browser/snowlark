// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Every message between the window, a tab and the compositor survives encoding as plain data (in
//! the format the process boundary will use, `docs/process-isolation.md` section 7).
//!
//! Each list below names every variant of its enum: the `match` it expands to has no wildcard, so a
//! new variant does not compile until it has a sample here.

use std::fmt::Debug;

use serde::de::DeserializeOwned;
use serde::Serialize;
use snowlark_dom::NodeId;
use snowlark_net::Url;
use snowlark_page::devtools::{
    BoxModel, ConsoleEntry, DomRow, DomRowKind, Highlight, NodeDetails, SourceLocation, ValueKind,
};
use snowlark_page::devtools_node::CopyKind;
use snowlark_page::embedder::ContextTarget;
use snowlark_page::popup::{PopupItem, SelectPopup};
use snowlark_paint::window::{Cursor, ImeEvent, Modifiers, MouseButton};
use snowlark_paint::{Color, DisplayList, DisplayListBuilder, Point, Rect, Size, Ticks, VideoId};

use crate::composite::CompMsg;
use crate::devtools::protocol::{
    DevtoolsCommand, DevtoolsEvent, EditKind, NetUpdate, NodeOp, ResultOp,
};
use crate::driver::{
    ContextInfo, Device, Disposition, DragInput, FindCommand, FindStatus, InternalRequest,
    KeyInput, KeyKind, Pointer, Scroller,
};
use crate::tab::{IconBitmap, MemoryUse, TabCommand, TabEvent, TabState};

fn round_trip<T: Serialize + DeserializeOwned + PartialEq + Debug>(value: &T) {
    let bytes = postcard::to_stdvec(value).expect("encodes");
    let back: T = postcard::from_bytes(&bytes).expect("decodes");
    assert_eq!(&back, value);
    // Decoding is total: cut short, it fails and does not panic.
    for len in 0..bytes.len().min(64) {
        let _ = postcard::from_bytes::<T>(&bytes[..len]);
    }
    // As the far end of a pipe would get it: framed, cut out of the stream, decoded.
    assert_eq!(
        &snowlark_ipc::loopback::pass(value).expect("crosses"),
        value
    );
    // Nor does a corrupted message panic.
    let mut seed = 0x2545_F491_4F6C_DD1Du64;
    for _ in 0..256 {
        let mut bad = bytes.clone();
        for _ in 0..3 {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            if !bad.is_empty() {
                let at = (seed >> 33) as usize % bad.len();
                bad[at] = (seed >> 24) as u8;
            }
        }
        let _ = snowlark_ipc::wire::decode::<T>(&bad);
    }
}

/// `$name` tests a sample of each variant of `$ty`: `pattern => sample`.
macro_rules! every_variant {
    ($name:ident: $ty:ty, [$($pat:pat => $sample:expr),+ $(,)?]) => {
        #[test]
        fn $name() {
            let _every_variant_is_listed = |v: &$ty| match v {
                $($pat => (),)+
            };
            $(
                let sample: $ty = $sample;
                assert!(matches!(sample, $pat), "{sample:?} is listed under another variant");
                round_trip(&sample);
            )+
        }
    };
}

fn url() -> Url {
    Url::parse("https://a.example/path?q=1#top").unwrap()
}

fn point() -> Point {
    Point::new(1.5, -2.25)
}

fn rect() -> Rect {
    Rect::new(1.0, 2.0, 30.5, 40.25)
}

/// (a `NodeId` is an arena index and a generation)
fn node(index: u32) -> NodeId {
    postcard::from_bytes(&postcard::to_stdvec(&(index, 3u32)).unwrap()).unwrap()
}

fn mods() -> Modifiers {
    Modifiers {
        ctrl: true,
        shift: false,
        alt: true,
        meta: false,
    }
}

fn list() -> DisplayList {
    let mut b = DisplayListBuilder::new();
    b.fill_rect(rect(), Color::rgb(10, 20, 30));
    b.build()
}

/// A navigation the browser hands a renderer to commit (M5): a response head with a redirect
/// behind it, a violation the request met, and the time it began.
fn commit() -> snowlark_tab::tab::Commit {
    let mut head = snowlark_net::ResponseHead::new(200, "OK");
    head.url_list = vec![Url::parse("https://a.example/").unwrap(), url()];
    head.headers
        .append("Content-Type", "text/html; charset=utf-8");
    head.timing.start = Some(std::time::Instant::now() - std::time::Duration::from_millis(40));
    snowlark_tab::tab::Commit {
        navigation: 7,
        restore: Some(point()),
        params: snowlark_page::CommitParams {
            url: Url::parse("https://a.example/").unwrap(),
            begun: Some(3),
            reload: true,
            response: snowlark_page::CommitResponse::Fetched {
                head: Box::new(head),
            },
            violations: Vec::new(),
            started: Some(std::time::Instant::now() - std::time::Duration::from_millis(50)),
        },
        body: Some(snowlark_tab::services::net::HANDED_FROM + 1),
        view_source: Some(Url::parse("view-source:https://a.example/").unwrap()),
    }
}

/// A navigation a page asks the browser to begin (M5): a form's POST with its initiator.
fn begin_navigation() -> snowlark_page::embedder::BeginNavigation {
    let mut params = snowlark_net::navigation::NavigationParams::user(url());
    params.post = Some((b"a=1".to_vec(), "application/x-www-form-urlencoded".into()));
    params.form_submission = true;
    params.user_activation = false;
    params.initiator = Some(snowlark_net::navigation::NavInitiator {
        origin: Url::parse("https://a.example/").unwrap().origin(),
        referrer_source: Url::parse("https://a.example/from").unwrap(),
        referrer_policy: snowlark_net::ReferrerPolicy::StrictOriginWhenCrossOrigin,
        csp: None,
        top_level_origin: Some(snowlark_net::Origin::new_opaque()),
        has_cross_site_ancestor: false,
        prohibits_mixed_security_contexts: true,
    });
    snowlark_page::embedder::BeginNavigation { seq: 4, params }
}

fn key(kind: KeyKind) -> KeyInput {
    KeyInput {
        kind,
        key: 0x41,
        scan: 0x1E,
        text: Some('é'),
        mods: mods(),
    }
}

/// With `SNOWLARK_IPC=loopback` in the environment, a tab's channel really goes through the wire
/// format: without this, a run with the variable lost on the way would pass and prove nothing.
#[test]
fn the_tab_channels_go_through_the_wire_format_when_asked() {
    let asked = std::env::var("SNOWLARK_IPC").is_ok_and(|v| v == "loopback");
    assert_eq!(snowlark_ipc::loopback::enabled(), asked);
    let before = snowlark_ipc::loopback::crossed();
    let (tx, rx) = snowlark_ipc::channel::<TabCommand>();
    tx.send(TabCommand::Wake).unwrap();
    assert_eq!(rx.recv().unwrap(), TabCommand::Wake);
    assert_eq!(snowlark_ipc::loopback::crossed() > before, asked);
}

every_variant!(pointer: Pointer, [
    Pointer::Move(..) => Pointer::Move(point()),
    Pointer::Down(..) => Pointer::Down(point(), MouseButton::Back, mods(), 2),
    Pointer::Up(..) => Pointer::Up(point(), MouseButton::Right, mods()),
    Pointer::Wheel(..) => Pointer::Wheel(point(), Point::new(0.0, 120.0), mods()),
    Pointer::Leave => Pointer::Leave,
]);

every_variant!(mouse_button: MouseButton, [
    MouseButton::Left => MouseButton::Left,
    MouseButton::Middle => MouseButton::Middle,
    MouseButton::Right => MouseButton::Right,
    MouseButton::Back => MouseButton::Back,
    MouseButton::Forward => MouseButton::Forward,
]);

every_variant!(key_kind: KeyKind, [
    KeyKind::Down { .. } => KeyKind::Down { repeat: true },
    KeyKind::Up => KeyKind::Up,
    KeyKind::Char(_) => KeyKind::Char('ß'),
]);

every_variant!(ime_event: ImeEvent, [
    ImeEvent::Start => ImeEvent::Start,
    ImeEvent::Update { .. } => ImeEvent::Update { text: "にほ".into(), cursor: 2 },
    ImeEvent::Commit(_) => ImeEvent::Commit("日本".into()),
    ImeEvent::End => ImeEvent::End,
]);

every_variant!(find_command: FindCommand, [
    FindCommand::Open { .. } => FindCommand::Open { previous: "last".into() },
    FindCommand::Query(_) => FindCommand::Query("text".into()),
    FindCommand::Next => FindCommand::Next,
    FindCommand::Previous => FindCommand::Previous,
    FindCommand::Close { .. } => FindCommand::Close { select: true },
]);

every_variant!(disposition: Disposition, [
    Disposition::CurrentTab => Disposition::CurrentTab,
    Disposition::NewBackgroundTab => Disposition::NewBackgroundTab,
    Disposition::NewForegroundTab => Disposition::NewForegroundTab,
]);

every_variant!(internal_request: InternalRequest, [
    InternalRequest::Settings(_) => InternalRequest::Settings("theme=dark".into()),
    InternalRequest::History(_) => InternalRequest::History("r=1-2+https%3A%2F%2Fa.example%2F".into()),
    InternalRequest::Bookmarks(_) => InternalRequest::Bookmarks("op=delete&id=3".into()),
    InternalRequest::Downloads(_) => InternalRequest::Downloads("op=remove&id=3".into()),
]);

every_variant!(cursor: Cursor, [
    Cursor::Arrow => Cursor::Arrow,
    Cursor::Hand => Cursor::Hand,
    Cursor::IBeam => Cursor::IBeam,
    Cursor::Wait => Cursor::Wait,
    Cursor::Progress => Cursor::Progress,
    Cursor::Crosshair => Cursor::Crosshair,
    Cursor::Move => Cursor::Move,
    Cursor::NotAllowed => Cursor::NotAllowed,
    Cursor::ResizeEW => Cursor::ResizeEW,
    Cursor::ResizeNS => Cursor::ResizeNS,
    Cursor::ResizeNWSE => Cursor::ResizeNWSE,
    Cursor::ResizeNESW => Cursor::ResizeNESW,
    Cursor::Help => Cursor::Help,
    Cursor::AutoScroll(..) => Cursor::AutoScroll(-1, 1),
    Cursor::Hidden => Cursor::Hidden,
    Cursor::VerticalText => Cursor::VerticalText,
    Cursor::Alias => Cursor::Alias,
    Cursor::Copy => Cursor::Copy,
    Cursor::ContextMenu => Cursor::ContextMenu,
    Cursor::ZoomIn => Cursor::ZoomIn,
    Cursor::ZoomOut => Cursor::ZoomOut,
    Cursor::Grab => Cursor::Grab,
    Cursor::Grabbing => Cursor::Grabbing,
]);

every_variant!(copy_kind: CopyKind, [
    CopyKind::OuterHtml => CopyKind::OuterHtml,
    CopyKind::Selector => CopyKind::Selector,
    CopyKind::JsPath => CopyKind::JsPath,
    CopyKind::XPath => CopyKind::XPath,
    CopyKind::FullXPath => CopyKind::FullXPath,
]);

every_variant!(edit_kind: EditKind, [
    EditKind::Html => EditKind::Html,
    EditKind::Text => EditKind::Text,
    EditKind::Attribute(_) => EditKind::Attribute("data-x".into()),
    EditKind::AddAttribute => EditKind::AddAttribute,
]);

every_variant!(node_op: NodeOp, [
    NodeOp::Copy(_) => NodeOp::Copy(CopyKind::JsPath),
    NodeOp::Delete => NodeOp::Delete,
    NodeOp::Duplicate => NodeOp::Duplicate,
    NodeOp::ToggleHide => NodeOp::ToggleHide,
    NodeOp::ScrollIntoView => NodeOp::ScrollIntoView,
    NodeOp::Focus => NodeOp::Focus,
    NodeOp::ExpandRecursively => NodeOp::ExpandRecursively,
    NodeOp::CollapseChildren => NodeOp::CollapseChildren,
    NodeOp::StoreGlobal => NodeOp::StoreGlobal,
    NodeOp::BeginEdit(_) => NodeOp::BeginEdit(EditKind::Attribute("class".into())),
    NodeOp::SetAttributes { .. } => NodeOp::SetAttributes {
        replacing: Some("class".into()),
        text: "class=\"a b\" id=x".into(),
    },
    NodeOp::SetOuterHtml(_) => NodeOp::SetOuterHtml("<p>
 x</p>".into()),
    NodeOp::SetText(_) => NodeOp::SetText("text".into()),
    NodeOp::Cut => NodeOp::Cut,
    NodeOp::Paste(_) => NodeOp::Paste("<p>x</p>".into()),
    NodeOp::CopyStyles => NodeOp::CopyStyles,
    NodeOp::BreakOnSubtree => NodeOp::BreakOnSubtree,
    NodeOp::BreakOnAttributes => NodeOp::BreakOnAttributes,
    NodeOp::BreakOnRemoval => NodeOp::BreakOnRemoval,
    NodeOp::AccessibilityTree => NodeOp::AccessibilityTree,
    NodeOp::Screenshot => NodeOp::Screenshot,
    NodeOp::ForceState(..) => NodeOp::ForceState(6, true),
]);

every_variant!(devtools_command: DevtoolsCommand, [
    DevtoolsCommand::Attach(_) => DevtoolsCommand::Attach(true),
    DevtoolsCommand::Eval(..) => DevtoolsCommand::Eval(7, "1 + 1".into()),
    DevtoolsCommand::Result(..) => DevtoolsCommand::Result(7, ResultOp::Store),
    DevtoolsCommand::ClearResults => DevtoolsCommand::ClearResults,
    DevtoolsCommand::Tree(_) => DevtoolsCommand::Tree(vec![node(1), node(2)]),
    DevtoolsCommand::Select(_) => DevtoolsCommand::Select(Some(node(3))),
    DevtoolsCommand::Hover(_) => DevtoolsCommand::Hover(None),
    DevtoolsCommand::Pick(_) => DevtoolsCommand::Pick(true),
    DevtoolsCommand::InspectAt(_) => DevtoolsCommand::InspectAt(point()),
    DevtoolsCommand::Node(..) => DevtoolsCommand::Node(node(5), NodeOp::ForceState(4, false)),
]);

every_variant!(net_update: NetUpdate, [
    NetUpdate::Started { .. } => NetUpdate::Started {
        id: 1,
        url: url().to_string(),
        method: "GET".into(),
        destination: "script".into(),
        headers: vec![("accept".into(), "*/*".into())],
        at: Ticks::now(),
    },
    NetUpdate::Head { .. } => NetUpdate::Head {
        id: 1,
        status: 404,
        status_text: "Not Found".into(),
        headers: vec![("content-type".into(), "text/html".into())],
        at: Ticks::now(),
    },
    NetUpdate::Body { .. } => NetUpdate::Body { id: 1, len: 3, data: Some(vec![1, 2, 3]) },
    NetUpdate::Done { .. } => NetUpdate::Done { id: 1, at: Ticks::now() },
    NetUpdate::Failed { .. } => NetUpdate::Failed {
        id: 1,
        error: "canceled".into(),
        blocked: true,
        at: Ticks::now(),
    },
]);

every_variant!(dom_row_kind: DomRowKind, [
    DomRowKind::Open { .. } => DomRowKind::Open {
        tag: "div".into(),
        attrs: vec![("id".into(), "main".into())],
        inline_text: Some("hello".into()),
        empty: false,
    },
    DomRowKind::Close { .. } => DomRowKind::Close { tag: "div".into() },
    DomRowKind::Text(_) => DomRowKind::Text("text".into()),
    DomRowKind::Comment(_) => DomRowKind::Comment("note".into()),
    DomRowKind::Doctype(_) => DomRowKind::Doctype("html".into()),
]);

every_variant!(devtools_event: DevtoolsEvent, [
    DevtoolsEvent::Console(_) => DevtoolsEvent::Console(
        ["log", "info", "warn", "error", "debug", "clear"]
            .into_iter()
            .map(|level| ConsoleEntry {
                level,
                text: format!("a {level} message"),
                source: Some(SourceLocation {
                    url: url().to_string(),
                    line: 4,
                    column: 12,
                }),
                group: 1,
                realm: None,
                styles: vec![(
                    2,
                    snowlark_page::devtools::ConsoleStyle {
                        color: Some([0, 102, 204, 255]),
                        background: Some([0, 0, 0, 128]),
                        bold: true,
                        italic: true,
                    },
                )],
            })
            .collect(),
    ),
    DevtoolsEvent::EvalResult(..) => DevtoolsEvent::EvalResult(7, Ok(("[1, 2]".into(), ValueKind::Object))),
    DevtoolsEvent::Network(_) => DevtoolsEvent::Network(vec![NetUpdate::Done {
        id: 1,
        at: Ticks::now(),
    }]),
    DevtoolsEvent::Tree(_) => DevtoolsEvent::Tree(vec![DomRow {
        node: node(5),
        depth: 2,
        kind: DomRowKind::Text("text".into()),
        expandable: false,
        expanded: true,
    }]),
    DevtoolsEvent::Details(_) => DevtoolsEvent::Details(Some((
        node(5),
        NodeDetails {
            title: "div#main".into(),
            computed: vec![("color".into(), "red".into())],
            box_model: Some(BoxModel {
                margin: [1.0, 2.0, 3.0, 4.0],
                border: [0.5; 4],
                padding: [0.0; 4],
                content: (100.0, 50.5),
            }),
        },
    ))),
    DevtoolsEvent::Highlight(_) => DevtoolsEvent::Highlight(Some(Highlight {
        margin: rect(),
        border: rect(),
        padding: rect(),
        content: rect(),
        label: "div.card 320 × 48".into(),
    })),
    DevtoolsEvent::Picked(..) => DevtoolsEvent::Picked(node(9), vec![node(1), node(4)]),
    DevtoolsEvent::PickEnded => DevtoolsEvent::PickEnded,
    DevtoolsEvent::Clipboard(_) => DevtoolsEvent::Clipboard("<p>x</p>".into()),
    DevtoolsEvent::Forced(..) => DevtoolsEvent::Forced(node(5), 0b1010),
    DevtoolsEvent::EditText { .. } => DevtoolsEvent::EditText {
        node: node(6),
        kind: EditKind::Html,
        text: "<p>x</p>".into(),
    },
    DevtoolsEvent::Stored { .. } => DevtoolsEvent::Stored {
        name: "temp1".into(),
        text: "<p>x</p>".into(),
    },
    DevtoolsEvent::Navigated => DevtoolsEvent::Navigated,
]);

every_variant!(tab_command: TabCommand, [
    TabCommand::Navigate { .. } => TabCommand::Navigate {
        url: url(),
        restore: Some(point()),
        navigation: 12,
        reload: true,
    },
    TabCommand::Commit(_) => TabCommand::Commit(Box::new(commit())),
    TabCommand::GoInDocument { .. } => TabCommand::GoInDocument {
        url: url(),
        index: 2,
        navigation: 7,
    },
    TabCommand::Stop => TabCommand::Stop,
    TabCommand::Memory => TabCommand::Memory,
    TabCommand::Device(_) => TabCommand::Device(Device {
        viewport: Size::new(1280.0, 720.5),
        scale: 1.25,
        scrollbar: 17.0,
        screen: snowlark_page::ScreenPlacement {
            origin: (10, 90),
            outer: Some((2, 3, 1300, 900)),
            zoom: 1.25,
        },
    }),
    TabCommand::Scrolled(..) => TabCommand::Scrolled(
        point(),
        snowlark_tab::comp::ShownScroll {
            generation: 7,
            serial: 3,
        },
    ),
    TabCommand::Pointer(_) => TabCommand::Pointer(Pointer::Move(point())),
    TabCommand::Drag(_) => TabCommand::Drag(DragInput::Drop {
        at: point(),
        data: Box::new(snowlark_page::dnd::ExternalDrag {
            files: vec![snowlark_page::dnd::DroppedFile {
                name: "a.txt".into(),
                type_: "text/plain".into(),
                bytes: vec![1, 2, 3],
                modified: 7,
            }],
            text: Some("text".into()),
            allowed: "copyLink".into(),
            ..Default::default()
        }),
        mods: Modifiers::default(),
    }),
    TabCommand::Key(_) => TabCommand::Key(key(KeyKind::Down { repeat: false })),
    TabCommand::ScrollContainer(..) => TabCommand::ScrollContainer(node(4), point()),
    TabCommand::SnapScroll(..) => TabCommand::SnapScroll(Some(node(4)), point()),
    TabCommand::SelectChosen(..) => TabCommand::SelectChosen(node(4), 2),
    TabCommand::Ime(_) => TabCommand::Ime(ImeEvent::Commit("text".into())),
    TabCommand::Devtools(_) => TabCommand::Devtools(DevtoolsCommand::Pick(true)),
    TabCommand::Find(_) => TabCommand::Find(FindCommand::Next),
    TabCommand::ClearSiteData(_) => TabCommand::ClearSiteData(Some("a.example".into())),
    TabCommand::ColorScheme(_) => TabCommand::ColorScheme(true),
    TabCommand::Shown(_) => TabCommand::Shown(false),
    TabCommand::Fullscreen(_) => TabCommand::Fullscreen(true),
    TabCommand::ContextMenu(_) => TabCommand::ContextMenu(Some(point())),
    TabCommand::Wake => TabCommand::Wake,
    TabCommand::UseBeginFrames => TabCommand::UseBeginFrames,
    TabCommand::BeginFrame(_) => TabCommand::BeginFrame(snowlark_tab::comp::BeginFrameArgs {
        frame_time: Ticks::now(),
        interval: std::time::Duration::from_micros(5556),
        missed: true,
    }),
    TabCommand::AttachView { .. } => TabCommand::AttachView {
        window: 3,
        tab: 9,
        device: Device {
            viewport: Size::new(500.0, 600.0),
            scale: 1.5,
            ..Device::default()
        },
    },
    TabCommand::View { .. } => TabCommand::View {
        tab: 9,
        command: Box::new(TabCommand::Pointer(Pointer::Move(point()))),
    },
    TabCommand::Repaint => TabCommand::Repaint,
    TabCommand::Close => TabCommand::Close,
]);

fn state() -> TabState {
    TabState {
        url: url().to_string(),
        title: "A page".into(),
        loading: true,
        scrollers: vec![Scroller {
            node: node(6),
            rect: rect(),
            scroll: point(),
            max: Point::new(100.0, 2000.0),
        }],
        snap_viewport: true,
        error_page: false,
        navigations: 4,
    }
}

every_variant!(tab_event: TabEvent, [
    TabEvent::State(_) => TabEvent::State(state()),
    TabEvent::DragOperation(_) => TabEvent::DragOperation(snowlark_page::dnd::DragOperation::Link),
    TabEvent::Committed { .. } => TabEvent::Committed { url: url(), scroll: point() },
    TabEvent::DocumentUrl { .. } => TabEvent::DocumentUrl { url: url(), navigation: 12 },
    TabEvent::BeginNavigation(_) => TabEvent::BeginNavigation(Box::new(begin_navigation())),
    TabEvent::SameDocument { .. } => TabEvent::SameDocument {
        change: snowlark_page::embedder::SameDocumentChange {
            url: "https://a.example/p#x".into(),
            push: true,
            index: 3,
        },
        navigation: 5,
    },
    TabEvent::Traverse(_) => TabEvent::Traverse(-2),
    TabEvent::Fullscreen(_) => TabEvent::Fullscreen(true),
    TabEvent::NavigationReady(_) => TabEvent::NavigationReady(9),
    TabEvent::Hover { .. } => TabEvent::Hover {
        cursor: Cursor::Hand,
        link: Some(url()),
        title: Some("A tooltip".into()),
        press: Some(crate::driver::PressOutcome {
            canceled: true,
            blocked: false,
        }),
    },
    TabEvent::Open(..) => TabEvent::Open(url(), Disposition::NewForegroundTab),
    TabEvent::Favicon(..) => TabEvent::Favicon(
        Box::new(IconBitmap {
            width: 1,
            height: 2,
            pixels: vec![1, 2, 3, 4, 5, 6, 7, 8],
            orientation: 6,
        }),
        vec![0x89, b'P', b'N', b'G'],
    ),
    TabEvent::SelectPopup(_) => TabEvent::SelectPopup(Box::new(SelectPopup {
        select: node(8),
        anchor: rect(),
        items: vec![PopupItem {
            label: "One".into(),
            disabled: false,
            group: false,
            indented: true,
        }],
        selected: Some(0),
        font_size: 13.5,
    })),
    TabEvent::Crashed(_) => TabEvent::Crashed("a panic".into()),
    TabEvent::Devtools(_) => TabEvent::Devtools(Box::new(DevtoolsEvent::Navigated)),
    TabEvent::Find(_) => TabEvent::Find(FindStatus {
        query: "find me".into(),
        total: 12,
        current: Some(3),
        searching: true,
        truncated: false,
    }),
    TabEvent::Internal(_) => TabEvent::Internal(InternalRequest::Settings("a=b".into())),
    TabEvent::ContextMenu(_) => TabEvent::ContextMenu(Box::new(ContextInfo {
        point: point(),
        target: ContextTarget {
            element: Some(node(2)),
            link: Some(url()),
            image: None,
            selection: Some("selected".into()),
            editable: true,
        },
        keyboard: true,
    })),
    TabEvent::Blocked => TabEvent::Blocked,
    TabEvent::Memory(_) => TabEvent::Memory(MemoryUse {
        js: 1,
        dom: 2,
        images: 3,
        tiles: 4,
        received: 5,
    }),
    TabEvent::Audible(_) => TabEvent::Audible(true),
    TabEvent::OpenWindow { .. } => TabEvent::OpenWindow {
        window: 3,
        popup: true,
        size: (500.0, 600.0),
        position: (Some(100), None),
    },
    TabEvent::CloseWindow => TabEvent::CloseWindow,
    TabEvent::WindowRect(..) => TabEvent::WindowRect(100, -20, 500, 600),
]);

every_variant!(comp_msg: CompMsg, [
    CompMsg::Resize { .. } => CompMsg::Resize {
        width: 1920,
        height: 1080,
        chrome_height: 80,
        dpi_scale: 1.5,
        chrome: Some((list(), Color::WHITE)),
        overlay: Some(list()),
    },
    CompMsg::Chrome(..) => CompMsg::Chrome(list(), Color::rgb(1, 2, 3)),
    CompMsg::Overlay(_) => CompMsg::Overlay(list()),
    CompMsg::RemoveTab(_) => CompMsg::RemoveTab(3),
    CompMsg::Activate(_) => CompMsg::Activate(3),
    CompMsg::ScrollBy { .. } => CompMsg::ScrollBy {
        tab: 3,
        delta: point(),
        smooth: true,
        input: Some(Ticks::now()),
    },
    CompMsg::ScrollTo { .. } => CompMsg::ScrollTo {
        tab: 3,
        to: point(),
        smooth: false,
        with: Some(crate::composite::FrameRef {
            frame: 41,
            generation: 6,
            serial: 9,
        }),
    },
    CompMsg::AutoScroll { .. } => CompMsg::AutoScroll {
        tab: 3,
        target: crate::composite::AutoTarget::Container(node(8)),
        velocity: Some(point()),
    },
    CompMsg::ScrollbarActive(_) => CompMsg::ScrollbarActive(true),
    CompMsg::PageArea(_) => CompMsg::PageArea(Some((800, 600, 240))),
    CompMsg::Dark(_) => CompMsg::Dark(true),
    CompMsg::VideoFrame(_) => CompMsg::VideoFrame(VideoId(9)),
    CompMsg::NeedsBeginFrames { .. } => CompMsg::NeedsBeginFrames {
        tab: 3,
        needs: true,
    },
    CompMsg::Ack(_) => CompMsg::Ack(7),
    CompMsg::Bind { .. } => CompMsg::Bind { tab: 3, pipe: 1 },
    CompMsg::Videos { .. } => CompMsg::Videos { store: 2 },
    CompMsg::Surface { .. } => CompMsg::Surface {
        window: crate::composite::WindowId(0x10f2b),
        handle: 1,
    },
    CompMsg::Forward { .. } => CompMsg::Forward {
        tab: 3,
        window: Some(crate::composite::WindowId(-0x1234)),
    },
    CompMsg::DevtoolsWindow(_) => CompMsg::DevtoolsWindow(Some(Box::new(
        crate::composite::DevtoolsFrame {
            window: crate::composite::WindowId(0x10f2a),
            width: 800,
            height: 600,
            dpi_scale: 1.25,
            list: list(),
            background: Color::rgb(4, 5, 6),
        },
    ))),
    CompMsg::Quit => CompMsg::Quit,
]);

every_variant!(comp_event: crate::composite::CompEvent, [
    crate::composite::CompEvent::Presented(_) => crate::composite::CompEvent::Presented(7),
    crate::composite::CompEvent::Lost => crate::composite::CompEvent::Lost,
]);

every_variant!(gpu_up: crate::gpu_process::GpuUp, [
    crate::gpu_process::GpuUp::Ready { .. } => crate::gpu_process::GpuUp::Ready {
        adapter: "an adapter".into(),
        software: true,
    },
    crate::gpu_process::GpuUp::Presented { .. } => crate::gpu_process::GpuUp::Presented {
        window: crate::composite::WindowId(-7),
        seq: 9,
    },
    crate::gpu_process::GpuUp::Scroll { .. } => crate::gpu_process::GpuUp::Scroll {
        window: crate::composite::WindowId(3),
        tab: 4,
        view: Some(crate::composite::ScrollView {
            scroll: point(),
            target: point(),
            max: point(),
            viewport: snowlark_paint::Size::new(800.0, 600.0),
            content: snowlark_paint::Size::new(800.0, 6000.0),
            bar: 17.0,
        }),
    },
    crate::gpu_process::GpuUp::Tab { .. } => crate::gpu_process::GpuUp::Tab {
        window: crate::composite::WindowId(3),
        tab: 4,
        command: TabCommand::Scrolled(point(), Default::default()),
    },
    crate::gpu_process::GpuUp::Taken { .. } => crate::gpu_process::GpuUp::Taken { tab: 4 },
    crate::gpu_process::GpuUp::Refused { .. } => crate::gpu_process::GpuUp::Refused {
        tab: 4,
        why: "lists not as long as the job says".into(),
    },
    crate::gpu_process::GpuUp::NoCompositor { .. } => crate::gpu_process::GpuUp::NoCompositor {
        window: crate::composite::WindowId(3),
        why: "The desired element already exists.".into(),
    },
]);

#[test]
fn an_empty_list_and_no_lists_round_trip() {
    round_trip(&CompMsg::Overlay(DisplayList::new()));
    round_trip(&CompMsg::Resize {
        width: 1,
        height: 1,
        chrome_height: 0,
        dpi_scale: 1.0,
        chrome: None,
        overlay: None,
    });
}

#[test]
fn what_is_not_valid_does_not_decode() {
    // A node's generation is never zero (it is what makes `Option<NodeId>` 8 bytes).
    let bytes = postcard::to_stdvec(&(1u32, 0u32)).unwrap();
    assert!(postcard::from_bytes::<NodeId>(&bytes).is_err());
    // Not a URL.
    let bytes = postcard::to_stdvec(&"no scheme").unwrap();
    assert!(postcard::from_bytes::<Url>(&bytes).is_err());
    // A console level the console does not have.
    let entry = ("shout", "text", None::<SourceLocation>, 0usize);
    let bytes = postcard::to_stdvec(&entry).unwrap();
    assert!(postcard::from_bytes::<ConsoleEntry>(&bytes).is_err());
    // A display list goes through its own validation (a bad header, here).
    let mut encoded = Vec::new();
    list().encode(&mut encoded);
    encoded[0] ^= 0xFF;
    let bytes = postcard::to_stdvec(&serde_bytes_like(&encoded)).unwrap();
    assert!(postcard::from_bytes::<DisplayList>(&bytes).is_err());
}

/// Bytes as `Serializer::serialize_bytes` writes them.
fn serde_bytes_like(bytes: &[u8]) -> impl Serialize + '_ {
    struct Bytes<'a>(&'a [u8]);
    impl Serialize for Bytes<'_> {
        fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
            s.serialize_bytes(self.0)
        }
    }
    Bytes(bytes)
}
