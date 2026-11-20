// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The engine: the type the generated bindings call (`snowlark_bindings::Dom`). It implements every
//! generated `*Impl` trait plus `rt::Host`, `rt::Reflect` and `rt::EventHandlers` over one
//! [`snowlark_dom::Dom`] and the page state that is not a node (events, collections, observers, timers).
//!
//! # Handles
//!
//! A node's handle is its [`NodeId::to_bits`]: the generation in the high 32 bits (never zero), the arena
//! index in the low 32. Every other platform object lives in [`Engine::objects`], a slab whose handles are
//! `index + 1`, below 2^32, so the two never collide and `handle >> 32 == 0` tells them apart.
//!
//! # Borrowing
//!
//! Every trait method takes `&self`; the state is in `RefCell`s. A `RefMut` is never held across a call
//! into V8 that can run script (callbacks, wrappers of new objects are fine: they only read). Strings
//! returned to the bindings borrow through a `Ref` guard ([`DomStr::Guard`]), which the generated code
//! drops right after copying the string into V8.

pub(crate) mod animation;
mod attrs;
pub(crate) mod audio;
mod auxiliary;
pub(crate) mod blob;
pub mod broadcast;
pub(crate) mod canvas;
pub(crate) mod channel;
mod chardata;
mod close_watcher;
mod collections;
mod credentials;
pub(crate) mod crypto;
pub(crate) mod cssom;
pub(crate) mod custom;
pub(crate) mod datatransfer;
mod details;
mod device;
mod dialog;
mod document;
mod element;
pub(crate) mod encoding;
pub(crate) mod events;
pub(crate) mod fetch;
pub(crate) mod fontload;
pub(crate) mod forms;
pub(crate) mod frames;
pub(crate) mod fullscreen;
pub(crate) mod gamepad;
pub(crate) mod gc;
mod geometry;
pub(crate) mod held;
pub(crate) mod history;
pub(crate) mod html;
pub(crate) mod idb;
mod image_map;
pub(crate) mod img;
mod inner_text;
pub(crate) mod internals;
pub(crate) mod intersection;
pub(crate) mod matrix;
mod media;
pub(crate) mod media_capabilities;
mod node;
mod notification;
pub(crate) mod observer;
pub(crate) mod platform;
mod pointer_tilt;
pub(crate) mod policy;
mod popover;
pub(crate) mod ranges;
mod reactions;
mod resize;
mod sanitizer;
pub(crate) mod scheduler;
mod screen;
pub(crate) mod select;
pub(crate) mod selection;
mod shadow;
pub(crate) mod storage;
mod storage_manager;
pub(crate) mod streams;
pub(crate) mod stylesheets;
mod subtle;
pub(crate) mod svg;
pub(crate) mod timeline;
pub(crate) mod timing;
mod touch;
mod traversal;
pub(crate) mod trusted_types;
pub(crate) mod url;
pub(crate) mod view;
pub(crate) mod view_transition;
pub(crate) mod websocket;
pub mod window;
pub(crate) mod xhr;

use std::cell::{Cell, Ref, RefCell, RefMut};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use snowlark_bindings::rt::{self, Cx, DomStr, Error, ExceptionName, Handle};
use snowlark_bindings::InterfaceId;
use snowlark_dom::{Dom, DomError, NodeId};

pub use auxiliary::{HostedWindow, WindowChange};
pub use collections::Collection;
pub(crate) use element::{rejected as element_rejected, resolved as element_resolved};
pub(crate) use events::EventData;
pub(crate) use window::PageHooks;

/// Packs a node's id into its handle.
#[inline(always)]
pub fn node_handle(id: NodeId) -> Handle {
    Handle::new(id.to_bits())
}

/// The node a handle names, if it names one.
#[inline(always)]
pub fn handle_node(h: Handle) -> Option<NodeId> {
    if h.get() >> 32 == 0 {
        None
    } else {
        NodeId::from_bits(h.get())
    }
}

/// A platform object that is not a node.
pub enum Object {
    Window,
    /// `new EventTarget()`.
    EventTarget,
    InputDeviceCapabilities(bool),
    IdleDeadline {
        /// The start of the idle period the deadline belongs to (its deadline is computed from it).
        period_start: f64,
        did_timeout: bool,
    },
    Location {
        window: Handle,
    },
    Navigator,
    Screen {
        window: Handle,
    },
    ScreenOrientation {
        window: Handle,
    },
    VisualViewport {
        window: Handle,
    },
    Performance(Box<timeline::PerformanceData>),
    PerformanceEntry(Box<timeline::EntryData>),
    /// A navigator's `connection`, `userAgentData`, `clipboard` and `permissions`.
    NetworkInformation,
    NavigatorUAData,
    Clipboard,
    Permissions,
    /// A `PermissionStatus`: its name and state.
    PermissionStatus(Box<(String, snowlark_bindings::generated::PermissionState)>),
    /// HTML §8.9.1.6: a window's PluginArray and MimeTypeArray, and its PDF viewer plugin and MIME
    /// type objects (`index` into their lists).
    PluginArray {
        window: Handle,
        index: usize,
    },
    MimeTypeArray {
        window: Handle,
        index: usize,
    },
    Plugin {
        window: Handle,
        index: usize,
    },
    MimeType {
        window: Handle,
        index: usize,
    },
    /// A navigator's `credentials`.
    CredentialsContainer,
    /// A global's `scheduler` and a navigator's `scheduling`.
    Scheduler,
    Scheduling,
    /// A PasswordCredential or FederatedCredential.
    Credential(Box<credentials::CredentialData>),
    /// A `Notification` (the Notifications API).
    Notification(Box<notification::NotificationData>),
    /// Battery Status: a navigator's BatteryManager, holding its battery promise.
    BatteryManager {
        promise: Option<snowlark_bindings::rt::JsPromise>,
    },
    /// The Storage Standard: a global's StorageManager (`navigator.storage`).
    StorageManager {
        global: Handle,
    },
    /// Media Capabilities: a global's MediaCapabilities (`navigator.mediaCapabilities`).
    MediaCapabilities {
        global: Handle,
    },
    ClipboardItem(Box<platform::ClipboardItemData>),
    TrustedTypePolicyFactory(Box<trusted_types::FactoryData>),
    TrustedTypePolicy(Box<trusted_types::PolicyData>),
    TrustedHtmlParserOptions(Box<trusted_types::ParserOptionsData>),
    ViewTransition(Box<view_transition::TransitionData>),
    ViewTransitionTypeSet(Vec<String>),
    /// A Window's `performance.timing` and `performance.navigation` (their relevant global).
    PerformanceTiming(Handle),
    PerformanceNavigation(Handle),
    /// A `PerformanceServerTiming`: name, duration, description.
    PerformanceServerTiming(Box<(String, f64, String)>),
    MessageChannel {
        port1: Handle,
        port2: Handle,
    },
    MessagePort(Box<channel::PortData>),
    BroadcastChannel(Box<broadcast::ChannelData>),
    WebSocket(Box<websocket::WsData>),
    IntersectionObserver(Box<intersection::ObserverData>),
    IntersectionObserverEntry(Box<intersection::EntryData>),
    ResizeObserver(Box<resize::ObserverData>),
    ResizeObserverEntry(Box<resize::EntryData>),
    ResizeObserverSize {
        inline: f64,
        block: f64,
    },
    /// The SVG DOM's objects that are not nodes ([`svg`]).
    Svg(svg::SvgObject),
    /// Web Audio's objects ([`audio`]).
    Audio(audio::AudioObject),
    PerformanceObserver(Box<timeline::ObserverData>),
    /// A `PerformanceObserverEntryList`: its entries and its relevant global.
    PerformanceObserverEntryList(Vec<Handle>, Handle),
    /// One of a window's six BarProp objects (HTML §7.2.6): its window.
    BarProp(Handle),
    /// A navigator's `UserActivation` object (HTML §6.4.4): its window.
    UserActivation(Handle),
    /// A navigator's `MediaDevices` object (Media Capture and Streams §9.2): its window.
    MediaDevices(Handle),
    /// A window's `External` object (HTML §16.3, obsolete).
    External,
    CustomElementRegistry,
    DomImplementation {
        document: NodeId,
    },
    DomException {
        name: String,
        message: String,
    },
    Collection(Collection),
    TokenList {
        element: NodeId,
        attr: snowlark_dom::LocalName,
    },
    StringMap {
        element: NodeId,
    },
    AttrMap {
        element: NodeId,
    },
    ValidityState {
        element: NodeId,
    },
    Event(Box<EventData>),
    AbortController {
        signal: Handle,
    },
    AbortSignal(events::AbortSignalData),
    MutationObserver(observer::ObserverData),
    MutationRecord(observer::RecordData),
    Range(ranges::RangeData),
    StaticRange(ranges::BoundaryPair),
    TreeWalker(traversal::WalkerData),
    NodeIterator(traversal::IteratorData),
    /// A `DOMParser`, with the Window of the realm it was made in (its relevant global object, whose
    /// document's URL its documents take).
    DomParser {
        window: Option<Handle>,
    },
    XmlSerializer,
    /// A `Sanitizer` (HTML §8.6.2) with its configuration.
    Sanitizer(Box<sanitizer::Config>),
    TextEncoder,
    SubtleCrypto,
    /// A `CryptoKey` (Web Cryptography §13).
    CryptoKey(Box<subtle::KeyObj>),
    ReadableStream(Box<streams::ReadableStreamData>),
    ReadableStreamDefaultReader(Box<streams::ReaderData>),
    ReadableStreamBYOBReader(Box<streams::ReaderData>),
    ReadableStreamBYOBRequest(Box<streams::ByobRequestData>),
    /// A ReadableStreamDefaultController, or a ReadableByteStreamController (`byte`).
    ReadableController(Box<streams::ReadableControllerData>),
    WritableStream(Box<streams::WritableStreamData>),
    WritableStreamDefaultWriter(Box<streams::WriterData>),
    WritableController(Box<streams::WritableControllerData>),
    TransformStream(Box<streams::TransformStreamData>),
    TransformController(Box<streams::TransformControllerData>),
    /// A ByteLengthQueuingStrategy (`byte`) or CountQueuingStrategy.
    QueuingStrategy {
        byte: bool,
        high_water_mark: f64,
    },
    TextDecoderStream {
        transform: Handle,
        encoding: &'static encoding_rs::Encoding,
        fatal: bool,
        ignore_bom: bool,
    },
    TextEncoderStream {
        transform: Handle,
    },
    /// A CompressionStream or DecompressionStream: its transform stream.
    CompressionStream {
        transform: Handle,
    },
    DecompressionStream {
        transform: Handle,
    },
    TextDecoder(encoding::DecoderData),
    Url(url::UrlData),
    UrlSearchParams(url::ParamsData),
    DomRect(geometry::Rect),
    DomRectList(Vec<Handle>),
    Touch(Box<touch::TouchData>),
    TouchList(Vec<Handle>),
    DomPoint(geometry::Point4),
    DomQuad([Handle; 4]),
    DomMatrix(Box<geometry::MatrixData>),
    TrustedHtml(String),
    TrustedScript(String),
    TrustedScriptUrl(String),
    Headers(snowlark_net::Headers),
    Request(Box<fetch::RequestData>),
    Response(Box<fetch::ResponseData>),
    Blob(blob::BlobData),
    FileList(Vec<Handle>),
    DataTransfer(Box<datatransfer::DataTransferData>),
    /// A DataTransferItemList: its DataTransfer.
    DataTransferItemList(Handle),
    /// A DataTransferItem: its DataTransfer (the item is the one whose object it is).
    DataTransferItem(Handle),
    FileReader(Box<blob::ReaderData>),
    FormData(Vec<(String, fetch::FormValue)>),
    Xhr(Box<xhr::XhrData>),
    XhrUpload,
    WorkerGlobal(crate::worker::WorkerGlobalData),
    WorkerLocation(::url::Url),
    WorkerNavigator,
    Worker(Box<crate::worker::WorkerObject>),
    ElementInternals(Box<internals::InternalsData>),
    CustomStateSet {
        internals: Handle,
    },
    StyleDeclaration(cssom::Decl),
    MediaQueryList(cssom::Mql),
    FontFaceSet {
        document: NodeId,
        /// The faces a script added to the set, in order.
        script: Vec<Handle>,
        /// The wrappers of the CSS-connected faces made so far, by the renderer's face id.
        css: Vec<(usize, Handle)>,
    },
    FontFace(Box<cssom::FontFaceData>),
    /// A `Storage`: its area, and its window.
    Storage(Rc<RefCell<storage::Area>>, Handle),
    History {
        window: Handle,
    },
    StyleSheet(Box<stylesheets::SheetObj>),
    StyleSheetList {
        scope: NodeId,
    },
    CssRuleList {
        list: snowlark_style::cssom::RuleList,
        sheet: Handle,
        parent: Option<Handle>,
    },
    CssRule {
        rule: snowlark_style::cssom::Rule,
        sheet: Handle,
        parent: Option<Handle>,
    },
    MediaList(stylesheets::MediaOwner),
    Canvas2D(Box<canvas::ContextObj>),
    CanvasGradient(snowlark_render::canvas::Gradient),
    CanvasPattern(std::sync::Arc<snowlark_render::canvas::Pattern>),
    Path2D(snowlark_render::canvas::CanvasPath),
    TextMetrics {
        width: f64,
        ascent: f64,
        descent: f64,
    },
    ImageData(Box<canvas::ImageDataObj>),
    Crypto,
    Selection(Box<selection::SelectionData>),
    /// Media elements' objects (`MediaError`, `TimeRanges`, tracks, MSE): [`crate::media`].
    Media(Box<crate::media::MediaObject>),
    /// Web Animations: `Animation` (and `CSSAnimation`/`CSSTransition`), `KeyframeEffect`,
    /// `DocumentTimeline` ([`animation`]).
    Animation(Box<animation::AnimationData>),
    KeyframeEffect(Box<animation::EffectData>),
    DocumentTimeline(Box<animation::TimelineData>),
    /// IndexedDB's objects ([`idb`]).
    Idb(Box<idb::IdbObject>),
    /// A `DOMStringList` (HTML §2.6.?): its strings, and whether Rust keeps it (a Location's
    /// `ancestorOrigins`; IndexedDB's name lists are made per access and collectable).
    StringList(Vec<String>, bool),
    /// A freed slot ([`gc`]), reused by the next allocation.
    Free,
}

/// The engine behind one event loop's bindings.
pub struct Engine {
    /// The node store, shared with the HTML parser (which reaches it through the `Rc`).
    pub dom: Rc<RefCell<Dom>>,
    pub(crate) objects: RefCell<Vec<Object>>,
    /// Web Animations: the global animation list, timelines, the pending animation event queue.
    pub(crate) animations: RefCell<animation::AnimationState>,
    /// Event listeners by event target.
    pub(crate) listeners: RefCell<rustc_hash::FxHashMap<Handle, events::ListenerList>>,
    /// Counts the listeners added to [`Self::listeners`]: a dispatch passes over the targets of its
    /// path that had none when it began while this stays the same.
    pub(crate) listeners_added: Cell<u64>,
    /// The vectors of finished dispatches, for the next ones (a dispatch can start inside another).
    pub(crate) dispatch_scratch: RefCell<Vec<events::DispatchScratch>>,
    /// Event handlers (HTML §8.1.8.1) by event target and event type.
    pub(crate) handlers: RefCell<HashMap<(Handle, &'static str), events::Handler>>,
    pub(crate) custom: RefCell<custom::CustomElements>,
    pub(crate) observers: RefCell<observer::Observers>,
    pub(crate) intersection: RefCell<intersection::IntersectionState>,
    pub(crate) ranges: RefCell<Vec<Handle>>,
    /// The browsing contexts: windows, their documents and realms.
    pub(crate) frames: RefCell<frames::Frames>,
    /// Documents whose interface is XMLDocument (DOM §4.5: `createDocument` and XML parsing).
    pub(crate) xml_documents: RefCell<rustc_hash::FxHashSet<NodeId>>,
    /// Per-element HTML state that is not in the DOM (form control values, script flags).
    pub(crate) html: RefCell<html::HtmlState>,
    /// What the engine asks of the page around it: timers, scripts, loading, console, style, layout.
    pub hooks: RefCell<PageHooks>,
    /// Content blocking's per-document state ([`crate::content_filter`]).
    pub(crate) content: RefCell<crate::content_filter::ContentState>,
    /// The event currently being dispatched (`window.event`).
    pub(crate) current_event: Cell<Option<Handle>>,
    /// The global of the listener it is being delivered to: only that Window's `event` shows it.
    pub(crate) current_event_global: Cell<Option<Handle>>,
    pub(crate) slots: RefCell<shadow::Slots>,
    pub(crate) pending_slot_signal: Cell<bool>,
    /// Where the reactions of an operation that inserted only nodes it parsed (fragment parsing)
    /// start in the queue, until its batch is taken: see [`Engine::parsed_insertion`].
    pub(crate) parsed_from: Cell<Option<usize>>,
    /// The last document URL parsed: the document, the URL string it had and the parsed URL (see
    /// [`Engine::parsed_document_url`]).
    pub(crate) document_url_cache: RefCell<Option<(NodeId, String, ::url::Url)>>,
    /// The nodes of the current reaction batch that were inserted (for live range updates).
    pub(crate) inserted_batch: RefCell<rustc_hash::FxHashSet<NodeId>>,
    /// The `details` elements whose insertion the current reaction batch has yet to reach: their
    /// name groups do not count them yet (HTML runs each insertion's steps as it happens).
    pub(crate) details_pending: RefCell<rustc_hash::FxHashSet<NodeId>>,
    /// Dialogs and each document's top layer.
    pub(crate) dialogs: RefCell<dialog::DialogState>,
    pub(crate) close_watchers: RefCell<close_watcher::CloseWatcherState>,
    /// Popover stacks and each popover's state.
    pub(crate) popover: RefCell<popover::PopoverState>,
    /// The Fullscreen API's flags, pending events and requests.
    pub(crate) fullscreen: RefCell<fullscreen::FullscreenState>,
    pub(crate) permissions_policy: RefCell<policy::PolicyState>,
    /// The Streams Standard's promise reactions and per-realm functions.
    pub(crate) streams: RefCell<streams::StreamsState>,
    pub(crate) in_error_reporting: Cell<bool>,
    pub(crate) iterators: RefCell<Vec<Handle>>,
    /// The event loop's queues, timers and frames.
    pub(crate) event_loop: RefCell<crate::event_loop::EventLoop>,
    /// The scheduler task queues (Prioritized Task Scheduling).
    pub(crate) scheduler: RefCell<scheduler::SchedulerState>,
    pub(crate) cleared_while_running: RefCell<HashSet<i32>>,
    /// Fetches, the parser and the script processing model.
    pub(crate) loader: RefCell<crate::loader::Loader>,
    pub(crate) modules: RefCell<crate::script::ModuleMap>,
    /// The JavaScript execution context stack depth (entries from Rust).
    pub(crate) js_depth: Cell<u32>,
    pub(crate) performing_checkpoint: Cell<bool>,
    pub(crate) about_to_be_notified: RefCell<Vec<crate::script::Rejection>>,
    pub(crate) notified_rejections: RefCell<Vec<v8::Weak<v8::Promise>>>,
    /// `document.currentScript`.
    pub(crate) current_script: Cell<Option<NodeId>>,
    /// Fetches script started (`fetch()`, XMLHttpRequest).
    pub(crate) api_jobs: RefCell<Vec<fetch::ApiJob>>,
    pub(crate) next_api_job: Cell<u32>,
    /// A worker engine's link to its parent.
    pub(crate) worker_scope: RefCell<Option<crate::worker::WorkerScope>>,
    /// The workers this engine started.
    pub(crate) workers: RefCell<Vec<crate::worker::WorkerLink>>,
    /// The top-level document's renderer and the viewport.
    pub(crate) render: RefCell<crate::render::RenderState>,
    /// The rendered frame documents' renderers and viewports, by document.
    pub(crate) frame_renders: RefCell<rustc_hash::FxHashMap<NodeId, crate::render::RenderState>>,
    /// A drop-down select's list to show (for the embedder).
    pub(crate) select_popup: RefCell<Option<crate::popup::SelectPopup>>,
    /// List boxes' Shift+click anchors (option indices).
    pub(crate) list_anchor: RefCell<rustc_hash::FxHashMap<NodeId, usize>>,
    /// Each document's `Selection` object.
    pub(crate) selections: RefCell<rustc_hash::FxHashMap<NodeId, Handle>>,
    /// What each document's selection paint was computed from (selection, editing host, DOM
    /// version).
    #[allow(clippy::type_complexity)]
    pub(crate) selection_paint_key: RefCell<
        rustc_hash::FxHashMap<NodeId, (Option<(ranges::Bp, ranges::Bp)>, Option<NodeId>, u64)>,
    >,
    /// Documents with a `selectionchange` queued.
    pub(crate) selectionchange_pending: RefCell<rustc_hash::FxHashSet<NodeId>>,
    /// The IME composition in progress.
    pub(crate) composition: RefCell<Option<crate::ime::Composition>>,
    /// The user edits' undo and redo steps (`crate::undo`).
    pub(crate) undo: RefCell<crate::undo::UndoState>,
    /// Error page documents and the URL whose navigation failed.
    pub(crate) error_pages: RefCell<rustc_hash::FxHashMap<NodeId, ::url::Url>>,
    /// Inline style declarations, media query lists, font sets.
    pub(crate) cssom: RefCell<cssom::CssomState>,
    /// Web storage areas.
    pub(crate) storage: RefCell<storage::StorageState>,
    /// The embedder's hooks ([`crate::embedder`]).
    pub(crate) embedder: RefCell<crate::embedder::EmbedderHooks>,
    /// Session history entries of documents.
    pub(crate) history: RefCell<history::HistoryState>,
    /// Canvas 2D contexts by their element, and the canvases that drew since the last rendering
    /// update.
    pub(crate) canvases: RefCell<rustc_hash::FxHashMap<NodeId, Handle>>,
    pub(crate) canvas_dirty: RefCell<Vec<NodeId>>,
    /// The frames a rendering update asked of the canvas thread before its layout, oldest first,
    /// taken after it or, when they come later, at the next one ([`Engine::flush_canvases`]).
    pub(crate) canvas_frames_pending: RefCell<Vec<(NodeId, snowlark_render::canvas::FrameRequest)>>,
    /// [`Engine::set_canvas_frames_async`].
    pub(crate) canvas_frames_async: Cell<bool>,
    /// Navigations to open in new tabs (URL, in the background), for the embedder.
    pub(crate) open_requests: RefCell<Vec<(::url::Url, bool)>>,
    /// A hosted top-level traversable's document whose rendering state is in `render` for now,
    /// and the page's own top-level document, whose state is in `frame_renders` meanwhile
    /// ([`Engine::begin_view`]).
    pub(crate) view: Cell<Option<(NodeId, NodeId)>>,
    /// The pointer's hover, press and buttons.
    pub(crate) pointer: RefCell<crate::input::PointerState>,
    /// `:focus-visible` for the next focus (Selectors 4 §9.4's heuristics): false after a pointer
    /// focused (or unfocused) by a press, true again after keyboard input or a keyboard focus
    /// move. Script focus keeps it, so a script focus after a click shows no ring.
    pub(crate) focus_from_keyboard: Cell<bool>,
    /// `focus()`'s `focusVisible` option while its focusing steps run (HTML §6.6.6: whether to
    /// indicate focus; `None` leaves it to the heuristics).
    pub(crate) focus_visible_option: Cell<Option<bool>>,
    /// Elements whose `focus()` is running ("locked for focus", HTML §6.6.6).
    pub(crate) locked_for_focus: RefCell<Vec<NodeId>>,
    /// The focused element when it matches `:focus-visible`.
    pub(crate) focus_ring: Cell<Option<NodeId>>,
    /// States the developer tools force on elements (Force state): node and kind (see
    /// `devtools_node::FORCE_STATES`).
    pub(crate) forced_states: RefCell<Vec<(NodeId, u8)>>,
    /// The values the developer tools' console input evaluated to, by evaluation id, kept for the
    /// console's context menu (Copy object, Store as global variable) until the console is
    /// cleared or the document goes: Chrome's "console" object group.
    pub(crate) devtools_results: RefCell<std::collections::VecDeque<(u64, v8::Global<v8::Value>)>>,
    /// Form controls whose value, checkedness, selection or focus changed: their rendering follows
    /// at the next rendering update.
    pub(crate) controls_dirty: RefCell<rustc_hash::FxHashSet<NodeId>>,
    /// Every control with page state is synced at the next rendering update (a renderer was made,
    /// which has none of their states).
    pub(crate) controls_resync: Cell<bool>,
    /// The focused elements the last rendering update synced (focus leaving a field changes what it
    /// shows).
    pub(crate) controls_focused: RefCell<Vec<NodeId>>,
    /// Elements whose form pseudo-classes (`:valid`, `:in-range`, `:user-invalid`, `:disabled`...)
    /// are to be computed again (`Engine::sync_form_states`).
    /// (Each once: an element on it carries `element_flags::FORM_STATE_MARKED`.)
    pub(crate) form_states_dirty: RefCell<Vec<NodeId>>,
    /// Those of them whose state needs a script context (a `pattern` to test), brought up to date
    /// after each DOM operation: the rest wait for a read.
    pub(crate) form_states_dirty_script: RefCell<rustc_hash::FxHashSet<NodeId>>,
    /// Those of them that were not in a document when a rendering read synced the others: only
    /// DOM selector matching reads them. (A list: each sync adds distinct elements, and garbage
    /// collection prunes it.)
    pub(crate) form_states_dirty_detached: RefCell<Vec<NodeId>>,
    /// Its length when its repeats last went.
    pub(crate) form_states_detached_len: Cell<usize>,
    /// How many times a control's form pseudo-class state was computed (a cost counter).
    pub(crate) form_states_computed: Cell<u64>,
    /// The form pseudo-class state last computed for each element that has one.
    pub(crate) form_states: RefCell<rustc_hash::FxHashMap<NodeId, snowlark_style::ElementState>>,
    /// The selector lists `matches`, `closest` and `querySelector(All)` parsed, by their text.
    pub(crate) selector_cache: RefCell<select::SelectorCache>,
    /// How many selector lists those queries parsed (a cost counter).
    pub(crate) selector_lists_parsed: Cell<u64>,
    /// Wrapper lifetime and freeing bookkeeping.
    pub(crate) gc: RefCell<gc::GcState>,
    /// A node got a wrapper while the DOM was borrowed, so its [`Dom::set_wrapped`] mark is missing:
    /// moved subtrees' wrappers are touched whatever their summary says.
    pub(crate) wrap_marks_lost: Cell<bool>,
    /// Profiling (`SNOWLARK_PROFILE=1`): binding counters, forced layouts, rendering statistics.
    pub profile: Option<crate::profile::PageProfile>,
    /// The `performance.mark` calls the native sampler tags its samples with: the profile's under
    /// `SNOWLARK_PROFILE=1`, its own under `SNOWLARK_PROFILE_NATIVE=only` (no other profiling on).
    pub sample_marks: Option<snowlark_js::native_profile::SampleMarks>,
    /// Media elements and their objects ([`crate::media`]).
    pub(crate) media: RefCell<crate::media::Media>,
    /// The ids and names in a document at a DOM version, for the Window's named properties.
    #[allow(clippy::type_complexity)]
    /// Content Security Policy: policy containers, request clients, violations to report.
    pub(crate) policy: RefCell<crate::csp::PolicyState>,
    /// Inline checks the renderer ran (it holds no engine), whose violations are reported next.
    pub(crate) policy_pending: Rc<RefCell<crate::csp::PendingInline>>,
    /// The `[[CryptographicNonce]]` of elements whose slot is kept apart from their `nonce` attribute:
    /// the attribute was hidden, or the slot was set through the `nonce` IDL attribute. Shared with
    /// the renderer's `<style>` check, so a style block sees the slot as script left it.
    pub(crate) hidden_nonces: Rc<RefCell<rustc_hash::FxHashMap<NodeId, String>>>,
    /// Resource, Navigation, Paint and Event Timing bookkeeping.
    pub(crate) timing: RefCell<timing::TimingState>,
    /// The active view transition of each document or element.
    pub(crate) view_transitions: RefCell<rustc_hash::FxHashMap<NodeId, Handle>>,
    /// The WebSockets whose connections are open, polled for their events.
    pub(crate) websockets: RefCell<Vec<Handle>>,
    /// The BroadcastChannel hub and this event loop's inbox in it ([`broadcast`]).
    #[allow(clippy::type_complexity)]
    pub(crate) broadcast: RefCell<(
        Option<broadcast::BroadcastHub>,
        Option<std::sync::Arc<broadcast::Inbox>>,
    )>,
    /// The open BroadcastChannels by their hub id.
    pub(crate) broadcasts: RefCell<Vec<(u64, Handle)>>,
    /// While a message is serialized with a transfer list: each transferred port and the port it
    /// becomes ([`channel`]).
    pub(crate) transferring: RefCell<Vec<(Handle, Handle)>>,
    /// The SVG DOM's per-element state ([`svg`]).
    pub(crate) svg: RefCell<svg::SvgState>,
    /// IndexedDB: the backend, this loop's inbox, the live connections and transactions ([`idb`]).
    pub(crate) idb: RefCell<idb::IdbState>,
    pub(crate) audio: RefCell<audio::AudioState>,
    /// Set when an event listener threw (DOM's "legacyOutputDidListenersThrowFlag", which
    /// IndexedDB reads around its dispatches).
    pub(crate) listener_threw: Cell<bool>,
    /// The `img` elements' image data (HTML §4.8.4.3, [`img`]).
    pub(crate) images: RefCell<img::Images>,
}

impl Engine {
    /// An engine over an empty store, with reactions enabled.
    pub fn new() -> Rc<Engine> {
        let mut dom = Dom::new();
        dom.set_reactions_enabled(true);
        dom.set_reaction_filter(Some(reactions::REACTION_FILTER));
        let profile = crate::profile::PageProfile::from_env();
        Rc::new(Engine {
            dom: Rc::new(RefCell::new(dom)),
            objects: RefCell::new(Vec::new()),
            animations: RefCell::new(animation::AnimationState::default()),
            listeners: RefCell::default(),
            listeners_added: Cell::new(0),
            dispatch_scratch: RefCell::default(),
            handlers: RefCell::default(),
            custom: RefCell::new(custom::CustomElements::default()),
            observers: RefCell::new(observer::Observers::default()),
            intersection: RefCell::new(intersection::IntersectionState::default()),
            ranges: RefCell::new(Vec::new()),
            frames: RefCell::new(frames::Frames::default()),
            xml_documents: RefCell::new(Default::default()),
            html: RefCell::new(html::HtmlState::default()),
            hooks: RefCell::new(PageHooks::default()),
            content: RefCell::default(),
            current_event: Cell::new(None),
            current_event_global: Cell::new(None),
            slots: RefCell::new(shadow::Slots::default()),
            pending_slot_signal: Cell::new(false),
            parsed_from: Cell::new(None),
            document_url_cache: RefCell::new(None),
            inserted_batch: RefCell::default(),
            details_pending: RefCell::default(),
            streams: RefCell::default(),
            dialogs: RefCell::default(),
            close_watchers: RefCell::default(),
            popover: RefCell::default(),
            fullscreen: RefCell::default(),
            permissions_policy: RefCell::default(),
            in_error_reporting: Cell::new(false),
            iterators: RefCell::new(Vec::new()),
            event_loop: RefCell::new(crate::event_loop::EventLoop::default()),
            scheduler: RefCell::default(),
            cleared_while_running: RefCell::default(),
            loader: RefCell::new(crate::loader::Loader::default()),
            modules: RefCell::new(crate::script::ModuleMap::default()),
            js_depth: Cell::new(0),
            performing_checkpoint: Cell::new(false),
            about_to_be_notified: RefCell::new(Vec::new()),
            notified_rejections: RefCell::new(Vec::new()),
            current_script: Cell::new(None),
            api_jobs: RefCell::new(Vec::new()),
            next_api_job: Cell::new(0),
            worker_scope: RefCell::new(None),
            workers: RefCell::new(Vec::new()),
            render: RefCell::new(crate::render::RenderState::default()),
            frame_renders: RefCell::default(),
            select_popup: RefCell::new(None),
            list_anchor: RefCell::default(),
            error_pages: RefCell::default(),
            composition: RefCell::new(None),
            undo: RefCell::default(),
            selections: RefCell::default(),
            selection_paint_key: RefCell::default(),
            selectionchange_pending: RefCell::default(),
            cssom: RefCell::new(cssom::CssomState::default()),
            storage: RefCell::new(storage::StorageState::default()),
            // (the engine's own PDF viewer: on unless the embedder turns it off)
            embedder: RefCell::new(crate::embedder::EmbedderHooks {
                pdf_viewer: true,
                ..Default::default()
            }),
            history: RefCell::new(history::HistoryState::default()),
            canvases: RefCell::default(),
            canvas_dirty: RefCell::new(Vec::new()),
            canvas_frames_pending: RefCell::new(Vec::new()),
            canvas_frames_async: Cell::new(false),
            open_requests: RefCell::new(Vec::new()),
            view: Cell::new(None),
            pointer: RefCell::new(crate::input::PointerState::default()),
            // (no pointer has focused anything yet: script focus shows a focus ring)
            focus_from_keyboard: Cell::new(true),
            focus_visible_option: Cell::new(None),
            locked_for_focus: RefCell::new(Vec::new()),
            focus_ring: Cell::new(None),
            forced_states: RefCell::new(Vec::new()),
            devtools_results: RefCell::default(),
            controls_dirty: RefCell::default(),
            controls_resync: Cell::new(true),
            controls_focused: RefCell::default(),
            form_states_dirty: RefCell::default(),
            form_states_dirty_script: RefCell::default(),
            form_states_dirty_detached: RefCell::default(),
            form_states_detached_len: Cell::new(0),
            form_states_computed: Cell::new(0),
            form_states: RefCell::default(),
            selector_cache: RefCell::default(),
            selector_lists_parsed: Cell::new(0),
            gc: RefCell::new(gc::GcState::default()),
            wrap_marks_lost: Cell::new(false),
            sample_marks: match &profile {
                Some(p) => Some(p.marks.clone()),
                None => crate::profile::native_only().then(Default::default),
            },
            profile,
            media: RefCell::new(crate::media::Media::default()),
            policy: RefCell::new(crate::csp::PolicyState::default()),
            policy_pending: Rc::new(RefCell::new(Vec::new())),
            hidden_nonces: Rc::new(RefCell::default()),
            timing: RefCell::default(),
            view_transitions: RefCell::default(),
            websockets: RefCell::new(Vec::new()),
            broadcast: RefCell::new((None, None)),
            broadcasts: RefCell::new(Vec::new()),
            transferring: RefCell::new(Vec::new()),
            svg: RefCell::default(),
            idb: RefCell::default(),
            audio: RefCell::default(),
            listener_threw: Cell::new(false),
            images: RefCell::new(img::Images::default()),
        })
    }

    // ----- the store -------------------------------------------------------------------------------------

    #[inline(always)]
    pub fn dom(&self) -> Ref<'_, Dom> {
        self.dom.borrow()
    }

    #[inline(always)]
    pub fn dom_mut(&self) -> RefMut<'_, Dom> {
        self.dom.borrow_mut()
    }

    /// A string borrowed out of the store.
    #[inline(always)]
    pub(crate) fn dom_str<'a>(&'a self, f: impl FnOnce(&Dom) -> &str) -> DomStr<'a> {
        DomStr::Guard(Ref::map(self.dom.borrow(), f))
    }

    /// The node behind a handle the bindings brand-checked as a node.
    #[inline(always)]
    pub(crate) fn n(&self, h: Handle) -> NodeId {
        handle_node(h).expect("a node handle")
    }

    // ----- objects -----------------------------------------------------------------------------------------

    /// Adds an object and returns its handle (in a freed slot when there is one).
    pub(crate) fn alloc(&self, object: Object) -> Handle {
        let collectable = Engine::gc_collectable(&object);
        let mut objects = self.objects.borrow_mut();
        let mut gc = self.gc.borrow_mut();
        let h = match gc.free_slots.pop() {
            Some(i) => {
                objects[i as usize] = object;
                Handle::new(i as u64 + 1)
            }
            None => {
                objects.push(object);
                Handle::new(objects.len() as u64)
            }
        };
        if collectable {
            gc.fresh.push(h);
            if let Some(g) = &mut gc.grouping {
                g.allocated.push(h);
            }
        }
        h
    }

    /// The object behind a non-node handle.
    #[inline]
    pub(crate) fn obj(&self, h: Handle) -> Ref<'_, Object> {
        Ref::map(self.objects.borrow(), |o| &o[h.get() as usize - 1])
    }

    #[inline]
    pub(crate) fn obj_mut(&self, h: Handle) -> RefMut<'_, Object> {
        RefMut::map(self.objects.borrow_mut(), |o| &mut o[h.get() as usize - 1])
    }
}

// ----- errors ----------------------------------------------------------------------------------------------

/// The DOMException of a DOM algorithm's error.
pub(crate) fn dom_err(e: DomError) -> Error {
    let name = match e {
        DomError::IndexSize => ExceptionName::IndexSizeError,
        DomError::HierarchyRequest => ExceptionName::HierarchyRequestError,
        DomError::WrongDocument => ExceptionName::WrongDocumentError,
        DomError::InvalidCharacter => ExceptionName::InvalidCharacterError,
        DomError::NoModificationAllowed => ExceptionName::NoModificationAllowedError,
        DomError::NotFound => ExceptionName::NotFoundError,
        DomError::NotSupported => ExceptionName::NotSupportedError,
        DomError::InUseAttribute => ExceptionName::InUseAttributeError,
        DomError::InvalidState => ExceptionName::InvalidStateError,
        DomError::Syntax => ExceptionName::SyntaxError,
        DomError::InvalidModification => ExceptionName::InvalidModificationError,
        DomError::Namespace => ExceptionName::NamespaceError,
        DomError::InvalidAccess => ExceptionName::InvalidAccessError,
        DomError::TypeMismatch => ExceptionName::TypeMismatchError,
        DomError::Security => ExceptionName::SecurityError,
        DomError::Network => ExceptionName::NetworkError,
        DomError::Abort => ExceptionName::AbortError,
        DomError::QuotaExceeded => ExceptionName::QuotaExceededError,
        DomError::Timeout => ExceptionName::TimeoutError,
        DomError::InvalidNodeType => ExceptionName::InvalidNodeTypeError,
        DomError::DataClone => ExceptionName::DataCloneError,
    };
    Error::Dom(name, e.to_string().into())
}

/// `Result<T, DomError>` → bindings result.
pub(crate) trait DomResultExt<T> {
    fn js(self) -> rt::Result<T>;
}

impl<T> DomResultExt<T> for Result<T, DomError> {
    #[inline]
    fn js(self) -> rt::Result<T> {
        self.map_err(dom_err)
    }
}

/// Whether an element is form-associated (HTML §4.10.2): listed elements, `img`, `label`... the ones
/// whose event handler scope includes the form owner.
pub fn is_form_associated(dom: &Dom, el: NodeId) -> bool {
    collections::is_listed(dom, el)
        || dom.is_html_element_named(el, &snowlark_dom::local_name!("img"))
}

impl Engine {
    pub(crate) fn form_owner_of_dom(&self, dom: &Dom, el: NodeId) -> Option<NodeId> {
        collections::form_owner(dom, el)
    }
}

/// A DOMException.
pub(crate) fn exception(name: ExceptionName, message: &'static str) -> Error {
    Error::Dom(name, message.into())
}

// ----- rt::Host ----------------------------------------------------------------------------------------------

impl rt::Host for Engine {
    fn interface_of(&self, handle: Handle) -> InterfaceId {
        match handle_node(handle) {
            Some(id) => self.node_interface(id),
            None => self.object_interface(handle),
        }
    }

    fn push_ce_reactions(&self) {
        self.custom.borrow_mut().stack.push(Vec::new());
    }

    fn pop_ce_reactions(&self, cx: &mut Cx) {
        self.process_reactions(cx);
        let queue = self.custom.borrow_mut().stack.pop().unwrap_or_default();
        self.invoke_reactions(cx, queue);
    }

    fn html_constructor(
        &self,
        cx: &mut Cx,
        interface: InterfaceId,
        new_target: &rt::JsObject,
    ) -> rt::Result<Handle> {
        self.html_constructor_steps(cx, interface, new_target)
    }

    fn window_child(&self, window: Handle, index: u32) -> Option<Handle> {
        self.child_windows(window).get(index as usize).copied()
    }

    fn may_access(&self, accessing: Handle, target: Handle) -> bool {
        self.window_may_access(accessing, target)
    }

    fn cross_origin_token(&self, object: Handle) -> u64 {
        self.frames.borrow_mut().tokens.of(object)
    }

    fn cross_origin_object(&self, token: u64) -> Option<Handle> {
        self.frames.borrow().tokens.object(token)
    }

    fn cross_origin_named_child(&self, window: Handle, name: &str) -> Option<Handle> {
        self.named_child_window(window, name)
    }

    fn active_window(&self, handle: Handle) -> Option<Option<Handle>> {
        // (asked while the frames are being changed, it gives no object rather than the wrong one)
        let Ok(f) = self.frames.try_borrow() else {
            return Some(None);
        };
        let &(id, _) = f.replaced.get(&handle)?;
        let window = f.list[id].window;
        Some(f.by_window.contains_key(&window).then_some(window))
    }

    /// A node's relevant realm is its node document's window's, or its node document's own when no
    /// window shows it (`createHTMLDocument`, a frame's old document); an event's is its target's
    /// (DOM §2.10 "fire an event"), which is known from its first dispatch on; the others' here hang off
    /// a window, a node or the global they were made for. What is left is made by a binding of its realm
    /// or in a callback or task that entered it, so the running code's realm is its own.
    fn realm_of(&self, handle: Handle) -> Option<Handle> {
        let Some(n) = handle_node(handle) else {
            let objects = self.objects.try_borrow().ok()?;
            return match objects.get(handle.get() as usize - 1)? {
                Object::PerformanceEntry(e) => Some(e.global),
                Object::PerformanceObserverEntryList(_, g) => Some(*g),
                Object::Idb(o) => Some(o.global()),
                Object::MessagePort(p) => Some(p.global),
                Object::Location { window }
                | Object::Screen { window }
                | Object::ScreenOrientation { window }
                | Object::VisualViewport { window }
                | Object::History { window } => Some(*window),
                Object::DomParser { window } => *window,
                Object::PerformanceTiming(window) | Object::PerformanceNavigation(window) => {
                    Some(*window)
                }
                Object::DomImplementation { document: node }
                | Object::FontFaceSet { document: node, .. }
                | Object::TokenList { element: node, .. }
                | Object::StringMap { element: node }
                | Object::AttrMap { element: node }
                | Object::ValidityState { element: node } => Some(node_handle(*node)),
                // (the object itself, not its realm here: a node's wrapper keeps its realm when
                // the node is adopted, and an object with no realm here has a wrapper)
                Object::Event(e) => e.target.or(e.current_target),
                _ => None,
            };
        };
        let doc = {
            let dom = self.dom();
            if dom.is_document(n) {
                n
            } else {
                dom.node_document(n)
            }
        };
        Some(self.window_of_document(doc).unwrap_or(node_handle(doc)))
    }
}

impl Engine {
    fn node_interface(&self, id: NodeId) -> InterfaceId {
        use snowlark_dom::NodeData;
        let dom = self.dom();
        match dom.data(id) {
            NodeData::Element(e) => {
                if let Some(i) = self.custom.borrow().interface_override.get(&id) {
                    return *i;
                }
                html::element_interface(e.name())
            }
            NodeData::Document(_) => {
                // `HTMLDocument` for documents whose type is "html" (`legacy.idl`): the one
                // loaded into a window, `createHTMLDocument`, `DOMParser` and XHR's `text/html`.
                if self.xml_documents.borrow().contains(&id) {
                    InterfaceId::XMLDocument
                } else if dom.is_html_document(id) {
                    InterfaceId::HTMLDocument
                } else {
                    InterfaceId::Document
                }
            }
            NodeData::DocumentType(_) => InterfaceId::DocumentType,
            NodeData::DocumentFragment { .. } => InterfaceId::DocumentFragment,
            NodeData::ShadowRoot(_) => InterfaceId::ShadowRoot,
            NodeData::Attr(_) => InterfaceId::Attr,
            NodeData::Text(_) => InterfaceId::Text,
            NodeData::CDataSection(_) => InterfaceId::CDATASection,
            NodeData::Comment(_) => InterfaceId::Comment,
            NodeData::ProcessingInstruction { .. } => InterfaceId::ProcessingInstruction,
            NodeData::Free { .. } => unreachable!("a freed node has no wrapper"),
        }
    }

    fn object_interface(&self, h: Handle) -> InterfaceId {
        match &*self.obj(h) {
            Object::Window => InterfaceId::Window,
            Object::EventTarget => InterfaceId::EventTarget,
            Object::InputDeviceCapabilities(_) => InterfaceId::InputDeviceCapabilities,
            Object::IdleDeadline { .. } => InterfaceId::IdleDeadline,
            Object::Location { .. } => InterfaceId::Location,
            Object::Navigator => InterfaceId::Navigator,
            Object::Screen { .. } => InterfaceId::Screen,
            Object::ScreenOrientation { .. } => InterfaceId::ScreenOrientation,
            Object::VisualViewport { .. } => InterfaceId::VisualViewport,
            Object::Performance(_) => InterfaceId::Performance,
            Object::PerformanceEntry(e) => match e.entry_type {
                timeline::EntryType::Mark => InterfaceId::PerformanceMark,
                timeline::EntryType::Measure => InterfaceId::PerformanceMeasure,
                timeline::EntryType::Resource => InterfaceId::PerformanceResourceTiming,
                timeline::EntryType::Navigation => InterfaceId::PerformanceNavigationTiming,
                timeline::EntryType::Paint => InterfaceId::PerformancePaintTiming,
                timeline::EntryType::VisibilityState => InterfaceId::VisibilityStateEntry,
                timeline::EntryType::Event | timeline::EntryType::FirstInput => {
                    InterfaceId::PerformanceEventTiming
                }
            },
            Object::PerformanceTiming(_) => InterfaceId::PerformanceTiming,
            Object::NetworkInformation => InterfaceId::NetworkInformation,
            Object::NavigatorUAData => InterfaceId::NavigatorUAData,
            Object::Clipboard => InterfaceId::Clipboard,
            Object::Permissions => InterfaceId::Permissions,
            Object::PermissionStatus(_) => InterfaceId::PermissionStatus,
            Object::PluginArray { .. } => InterfaceId::PluginArray,
            Object::MimeTypeArray { .. } => InterfaceId::MimeTypeArray,
            Object::Plugin { .. } => InterfaceId::Plugin,
            Object::MimeType { .. } => InterfaceId::MimeType,
            Object::BatteryManager { .. } => InterfaceId::BatteryManager,
            Object::StorageManager { .. } => InterfaceId::StorageManager,
            Object::MediaCapabilities { .. } => InterfaceId::MediaCapabilities,
            Object::Notification(_) => InterfaceId::Notification,
            Object::CredentialsContainer => InterfaceId::CredentialsContainer,
            Object::Scheduler => InterfaceId::Scheduler,
            Object::Scheduling => InterfaceId::Scheduling,
            Object::Credential(c) => match c.kind {
                credentials::CredentialKind::Password { .. } => InterfaceId::PasswordCredential,
                credentials::CredentialKind::Federated { .. } => InterfaceId::FederatedCredential,
            },
            Object::ClipboardItem(_) => InterfaceId::ClipboardItem,
            Object::TrustedTypePolicyFactory(_) => InterfaceId::TrustedTypePolicyFactory,
            Object::TrustedTypePolicy(_) => InterfaceId::TrustedTypePolicy,
            Object::TrustedHtmlParserOptions(_) => InterfaceId::TrustedHTMLParserOptions,
            Object::ViewTransition(_) => InterfaceId::ViewTransition,
            Object::ViewTransitionTypeSet(_) => InterfaceId::ViewTransitionTypeSet,
            Object::PerformanceNavigation(_) => InterfaceId::PerformanceNavigation,
            Object::PerformanceServerTiming(_) => InterfaceId::PerformanceServerTiming,
            Object::PerformanceObserver(_) => InterfaceId::PerformanceObserver,
            Object::MessageChannel { .. } => InterfaceId::MessageChannel,
            Object::MessagePort(_) => InterfaceId::MessagePort,
            Object::BroadcastChannel(_) => InterfaceId::BroadcastChannel,
            Object::WebSocket(_) => InterfaceId::WebSocket,
            Object::IntersectionObserver(_) => InterfaceId::IntersectionObserver,
            Object::IntersectionObserverEntry(_) => InterfaceId::IntersectionObserverEntry,
            Object::ResizeObserver(_) => InterfaceId::ResizeObserver,
            Object::ResizeObserverEntry(_) => InterfaceId::ResizeObserverEntry,
            Object::ResizeObserverSize { .. } => InterfaceId::ResizeObserverSize,
            Object::Svg(o) => o.interface(),
            Object::Audio(o) => o.interface(),
            Object::PerformanceObserverEntryList(..) => InterfaceId::PerformanceObserverEntryList,
            Object::BarProp(_) => InterfaceId::BarProp,
            Object::UserActivation(_) => InterfaceId::UserActivation,
            Object::MediaDevices(_) => InterfaceId::MediaDevices,
            Object::External => InterfaceId::External,
            Object::CustomElementRegistry => InterfaceId::CustomElementRegistry,
            Object::DomImplementation { .. } => InterfaceId::DOMImplementation,
            Object::DomException { .. } => InterfaceId::DOMException,
            Object::Collection(c) => c.interface(),
            Object::TokenList { .. } => InterfaceId::DOMTokenList,
            Object::StringMap { .. } => InterfaceId::DOMStringMap,
            Object::AttrMap { .. } => InterfaceId::NamedNodeMap,
            Object::ValidityState { .. } => InterfaceId::ValidityState,
            Object::Event(e) => e.interface(),
            Object::AbortController { signal } => {
                if self.is_task_signal(*signal) {
                    InterfaceId::TaskController
                } else {
                    InterfaceId::AbortController
                }
            }
            Object::AbortSignal(s) if s.task.is_some() => InterfaceId::TaskSignal,
            Object::AbortSignal(_) => InterfaceId::AbortSignal,
            Object::MutationObserver(_) => InterfaceId::MutationObserver,
            Object::MutationRecord(_) => InterfaceId::MutationRecord,
            Object::Range(_) => InterfaceId::Range,
            Object::StaticRange(_) => InterfaceId::StaticRange,
            Object::TreeWalker(_) => InterfaceId::TreeWalker,
            Object::NodeIterator(_) => InterfaceId::NodeIterator,
            Object::DomParser { .. } => InterfaceId::DOMParser,
            Object::XmlSerializer => InterfaceId::XMLSerializer,
            Object::Sanitizer(_) => InterfaceId::Sanitizer,
            Object::TextEncoder => InterfaceId::TextEncoder,
            Object::SubtleCrypto => InterfaceId::SubtleCrypto,
            Object::CryptoKey(_) => InterfaceId::CryptoKey,
            Object::ReadableStream(_) => InterfaceId::ReadableStream,
            Object::ReadableStreamDefaultReader(_) => InterfaceId::ReadableStreamDefaultReader,
            Object::ReadableStreamBYOBReader(_) => InterfaceId::ReadableStreamBYOBReader,
            Object::ReadableStreamBYOBRequest(_) => InterfaceId::ReadableStreamBYOBRequest,
            Object::ReadableController(c) if c.byte => InterfaceId::ReadableByteStreamController,
            Object::ReadableController(_) => InterfaceId::ReadableStreamDefaultController,
            Object::WritableStream(_) => InterfaceId::WritableStream,
            Object::WritableStreamDefaultWriter(_) => InterfaceId::WritableStreamDefaultWriter,
            Object::WritableController(_) => InterfaceId::WritableStreamDefaultController,
            Object::TransformStream(_) => InterfaceId::TransformStream,
            Object::TransformController(_) => InterfaceId::TransformStreamDefaultController,
            Object::QueuingStrategy { byte: true, .. } => InterfaceId::ByteLengthQueuingStrategy,
            Object::QueuingStrategy { .. } => InterfaceId::CountQueuingStrategy,
            Object::TextDecoderStream { .. } => InterfaceId::TextDecoderStream,
            Object::TextEncoderStream { .. } => InterfaceId::TextEncoderStream,
            Object::CompressionStream { .. } => InterfaceId::CompressionStream,
            Object::DecompressionStream { .. } => InterfaceId::DecompressionStream,
            Object::TextDecoder(_) => InterfaceId::TextDecoder,
            Object::Url(_) => InterfaceId::URL,
            Object::UrlSearchParams(_) => InterfaceId::URLSearchParams,
            Object::DomRect(r) => {
                // (a viewBox's animVal is a read-only DOMRect: SVGRect, as engines expose it)
                if r.read_only && r.svg.is_none() {
                    InterfaceId::DOMRectReadOnly
                } else {
                    InterfaceId::DOMRect
                }
            }
            Object::DomRectList(_) => InterfaceId::DOMRectList,
            Object::Touch(_) => InterfaceId::Touch,
            Object::TouchList(_) => InterfaceId::TouchList,
            Object::DomPoint(p) => {
                // (read-only points of SVG lists and non-outermost `currentTranslate`s are
                // DOMPoints whose setters throw, as engines expose them)
                if p.read_only && p.svg.is_none() {
                    InterfaceId::DOMPointReadOnly
                } else {
                    InterfaceId::DOMPoint
                }
            }
            Object::DomQuad(_) => InterfaceId::DOMQuad,
            Object::DomMatrix(m) => {
                if m.read_only {
                    InterfaceId::DOMMatrixReadOnly
                } else {
                    InterfaceId::DOMMatrix
                }
            }
            Object::TrustedHtml(_) => InterfaceId::TrustedHTML,
            Object::TrustedScript(_) => InterfaceId::TrustedScript,
            Object::TrustedScriptUrl(_) => InterfaceId::TrustedScriptURL,
            Object::Headers(_) => InterfaceId::Headers,
            Object::Request(_) => InterfaceId::Request,
            Object::Response(_) => InterfaceId::Response,
            Object::Blob(b) => {
                if b.file.is_some() {
                    InterfaceId::File
                } else {
                    InterfaceId::Blob
                }
            }
            Object::FileList(_) => InterfaceId::FileList,
            Object::DataTransfer(_) => InterfaceId::DataTransfer,
            Object::DataTransferItemList(_) => InterfaceId::DataTransferItemList,
            Object::DataTransferItem(_) => InterfaceId::DataTransferItem,
            Object::FileReader(_) => InterfaceId::FileReader,
            Object::FormData(_) => InterfaceId::FormData,
            Object::Xhr(_) => InterfaceId::XMLHttpRequest,
            Object::XhrUpload => InterfaceId::XMLHttpRequestUpload,
            Object::WorkerGlobal(_) => InterfaceId::DedicatedWorkerGlobalScope,
            Object::WorkerLocation(_) => InterfaceId::WorkerLocation,
            Object::WorkerNavigator => InterfaceId::WorkerNavigator,
            Object::Worker(_) => InterfaceId::Worker,
            Object::ElementInternals(_) => InterfaceId::ElementInternals,
            Object::CustomStateSet { .. } => InterfaceId::CustomStateSet,
            Object::StyleDeclaration(cssom::Decl::FontFace(_)) => {
                InterfaceId::CSSFontFaceDescriptors
            }
            Object::StyleDeclaration(_) => InterfaceId::CSSStyleProperties,
            Object::MediaQueryList(_) => InterfaceId::MediaQueryList,
            Object::FontFaceSet { .. } => InterfaceId::FontFaceSet,
            Object::FontFace(_) => InterfaceId::FontFace,
            Object::Animation(a) => match a.css {
                Some(animation::CssOwner::Animation { .. }) => InterfaceId::CSSAnimation,
                Some(animation::CssOwner::Transition { .. }) => InterfaceId::CSSTransition,
                None => InterfaceId::Animation,
            },
            Object::KeyframeEffect(_) => InterfaceId::KeyframeEffect,
            Object::DocumentTimeline(_) => InterfaceId::DocumentTimeline,
            Object::Storage(..) => InterfaceId::Storage,
            Object::History { .. } => InterfaceId::History,
            Object::StyleSheet(_) => InterfaceId::CSSStyleSheet,
            Object::StyleSheetList { .. } => InterfaceId::StyleSheetList,
            Object::CssRuleList { .. } => InterfaceId::CSSRuleList,
            Object::CssRule { rule, .. } => Engine::rule_interface(rule),
            Object::MediaList(_) => InterfaceId::MediaList,
            Object::Canvas2D(_) => InterfaceId::CanvasRenderingContext2D,
            Object::CanvasGradient(_) => InterfaceId::CanvasGradient,
            Object::CanvasPattern(_) => InterfaceId::CanvasPattern,
            Object::Path2D(_) => InterfaceId::Path2D,
            Object::TextMetrics { .. } => InterfaceId::TextMetrics,
            Object::ImageData(_) => InterfaceId::ImageData,
            Object::Crypto => InterfaceId::Crypto,
            Object::Selection(_) => InterfaceId::Selection,
            Object::Media(m) => m.interface(),
            Object::Idb(o) => o.interface(),
            Object::StringList(..) => InterfaceId::DOMStringList,
            Object::Free => unreachable!("a freed object has no wrapper"),
        }
    }
}

/// The trusted types interfaces only carry their string.
impl snowlark_bindings::generated::TrustedHTMLImpl for Engine {
    fn stringify(&self, this: Handle) -> DomStr<'_> {
        self.trusted_str(this)
    }
    fn to_json(&self, _cx: &mut Cx, this: Handle) -> rt::Result<DomStr<'_>> {
        Ok(self.trusted_str(this))
    }
}

impl snowlark_bindings::generated::TrustedScriptImpl for Engine {
    fn stringify(&self, this: Handle) -> DomStr<'_> {
        self.trusted_str(this)
    }
    fn to_json(&self, _cx: &mut Cx, this: Handle) -> rt::Result<DomStr<'_>> {
        Ok(self.trusted_str(this))
    }
}

impl snowlark_bindings::generated::TrustedScriptURLImpl for Engine {
    fn stringify(&self, this: Handle) -> DomStr<'_> {
        self.trusted_str(this)
    }
    fn to_json(&self, _cx: &mut Cx, this: Handle) -> rt::Result<DomStr<'_>> {
        Ok(self.trusted_str(this))
    }
}

impl Engine {
    fn trusted_str(&self, this: Handle) -> DomStr<'_> {
        DomStr::Guard(Ref::map(self.objects.borrow(), |o| {
            match &o[this.get() as usize - 1] {
                Object::TrustedHtml(s) | Object::TrustedScript(s) | Object::TrustedScriptUrl(s) => {
                    s.as_str()
                }
                _ => "",
            }
        }))
    }
}

impl snowlark_bindings::generated::DOMExceptionImpl for Engine {
    fn name(&self, this: Handle) -> DomStr<'_> {
        DomStr::Guard(Ref::map(self.objects.borrow(), |o| {
            match &o[this.get() as usize - 1] {
                Object::DomException { name, .. } => name.as_str(),
                _ => "",
            }
        }))
    }
    fn message(&self, this: Handle) -> DomStr<'_> {
        DomStr::Guard(Ref::map(self.objects.borrow(), |o| {
            match &o[this.get() as usize - 1] {
                Object::DomException { message, .. } => message.as_str(),
                _ => "",
            }
        }))
    }
    fn code(&self, this: Handle) -> u16 {
        match &*self.obj(this) {
            Object::DomException { name, .. } => ExceptionName::legacy_code(name),
            _ => 0,
        }
    }
    fn constructor(&self, _cx: &mut Cx, message: &str, name: &str) -> rt::Result<Handle> {
        Ok(self.alloc(Object::DomException {
            name: name.to_owned(),
            message: message.to_owned(),
        }))
    }
}
