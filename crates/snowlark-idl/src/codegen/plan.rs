// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Planning: which interfaces and members are generated, their prototype ids, the Rust trait methods the
//! engine implements, how each attribute reflects, and what is left out (with the reason).

use super::names::*;
use crate::ast::*;
use crate::model::*;
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// The global the bindings are generated for.
pub const GLOBAL: &str = "Window";

/// A member's name, [[NeedsGet]] and [[NeedsSet]].
pub type CrossOriginMembers = &'static [(&'static str, bool, bool)];

/// HTML §7.2.1.3.2 CrossOriginProperties(O), in its order: per interface, the members with
/// [[NeedsGet]] and [[NeedsSet]] (both false: an operation).
pub const CROSS_ORIGIN_PROPERTIES: &[(&str, CrossOriginMembers)] = &[
    (
        "Window",
        &[
            ("window", true, false),
            ("self", true, false),
            ("location", true, true),
            ("close", false, false),
            ("closed", true, false),
            ("focus", false, false),
            ("blur", false, false),
            ("frames", true, false),
            ("length", true, false),
            ("top", true, false),
            ("opener", true, false),
            ("parent", true, false),
            ("postMessage", false, false),
        ],
    ),
    (
        "Location",
        &[("href", false, true), ("replace", false, false)],
    ),
];

/// The interfaces a Window or Location can be the `this` of: their members perform a security check
/// (Web IDL §3.7.6, HTML §7.2.1.3.5), also the members of Window's mixins.
pub const SECURITY_CHECKED: &[&str] = &["Window", "Location", "EventTarget"];

/// The global names whose realms the bindings serve: a Window, and a dedicated worker's global scope
/// (`[Global=(Worker,DedicatedWorker)]`). Interfaces and members exposed in any of them are generated;
/// each realm exposes the interface objects of its own globals.
pub const GLOBALS: &[&str] = &["Window", "Worker", "DedicatedWorker"];

/// Reflected attributes whose rules are in the HTML prose but not (yet) in its IDL: (interface,
/// attribute, extended attribute, default).
pub const REFLECT_PROSE: &[(&str, &str, &str, &str)] = &[
    // HTML §4.10.5.3.2: "limited to only positive numbers", default 20.
    ("HTMLInputElement", "size", "ReflectPositive", "20"),
];

/// Whether an exposure set includes one of the generated globals.
pub fn exposed_any(set: &BTreeSet<String>) -> bool {
    GLOBALS.iter().any(|g| exposed_in(set, g))
}

/// The global names of each kind of realm the bindings install into (as the generated
/// `GlobalKind::exposes` matches them): a Window, a dedicated worker.
const REALMS: &[&[&str]] = &[&["Window"], &["Worker", "DedicatedWorker"]];

/// A member's exposure set when it is narrower than its interface's in one of the generated kinds of
/// realm (Web IDL §3.3.6: a member is exposed only where both it and its interface are).
pub fn narrower_exposure(
    iface: &BTreeSet<String>,
    member: &BTreeSet<String>,
) -> Option<Vec<String>> {
    let in_realm =
        |set: &BTreeSet<String>, names: &[&str]| names.iter().any(|g| exposed_in(set, g));
    REALMS
        .iter()
        .any(|r| in_realm(iface, r) && !in_realm(member, r))
        .then(|| member.iter().cloned().collect())
}

/// The interfaces of this milestone besides every `HTMLElement` descendant (always included).
pub const SCOPE: &[&str] = &[
    "DOMException",
    "EventTarget",
    "Event",
    "AbortSignal",
    "Node",
    "Document",
    "DocumentFragment",
    "ShadowRoot",
    "Element",
    "Attr",
    "NamedNodeMap",
    "CharacterData",
    "Text",
    "CDATASection",
    "Comment",
    "ProcessingInstruction",
    "DocumentType",
    "HTMLCollection",
    "HTMLAllCollection",
    "HTMLFormControlsCollection",
    "HTMLOptionsCollection",
    "NodeList",
    "RadioNodeList",
    "DOMTokenList",
    "DOMStringMap",
    "ValidityState",
    "TrustedHTML",
    "TrustedScript",
    "TrustedScriptURL",
    "TrustedTypePolicyFactory",
    "TrustedTypePolicy",
    "TrustedHTMLParserOptions",
    // CSS Typed OM Level 1 (without the CSSColorValue family, which no engine ships).
    "CSSStyleValue",
    "StylePropertyMapReadOnly",
    "StylePropertyMap",
    "CSSUnparsedValue",
    "CSSVariableReferenceValue",
    "CSSKeywordValue",
    "CSSNumericValue",
    "CSSUnitValue",
    "CSSMathValue",
    "CSSMathSum",
    "CSSMathProduct",
    "CSSMathNegate",
    "CSSMathInvert",
    "CSSMathMin",
    "CSSMathMax",
    "CSSMathClamp",
    "CSSNumericArray",
    "CSSTransformValue",
    "CSSTransformComponent",
    "CSSTranslate",
    "CSSRotate",
    "CSSScale",
    "CSSSkew",
    "CSSSkewX",
    "CSSSkewY",
    "CSSPerspective",
    "CSSMatrixComponent",
    "CSSImageValue",
    // CSS View Transitions (the same-document API, without capture or animation).
    "ViewTransition",
    "ViewTransitionTypeSet",
    "CustomElementRegistry",
    "HTMLElement",
    // The global and what hangs off it.
    "Window",
    "Location",
    "Navigator",
    "Performance",
    "BarProp",
    // HTML §6.4.4 the UserActivation interface (`navigator.userActivation`).
    "UserActivation",
    // Media Capture and Streams: device enumeration (no capture devices, no getUserMedia).
    "MediaDevices",
    "MediaDeviceInfo",
    "InputDeviceInfo",
    // HTML §16.3 (obsolete): `window.external`, whose two methods do nothing.
    "External",
    // Network Information, User-Agent Client Hints, the async Clipboard API and Permissions.
    "NetworkInformation",
    "NavigatorUAData",
    "Clipboard",
    "ClipboardItem",
    // Clipboard API §5 the clipboard events (`copy`, `cut`, `paste`).
    "ClipboardEvent",
    "Permissions",
    // HTML: the plugin and MIME type arrays (§8.9.1.6 PDF viewing support); Battery Status.
    "PluginArray",
    "MimeTypeArray",
    "Plugin",
    "MimeType",
    "BatteryManager",
    "StorageManager",
    // Media Capabilities: decoding answered from what the media stack plays.
    "MediaCapabilities",
    // Gamepad (no gamepad source: `getGamepads()` answers empty slots).
    "Gamepad",
    "GamepadButton",
    "GamepadHapticActuator",
    "GamepadEvent",
    // The Notifications API (no notification platform: permission "denied").
    "Notification",
    // Credential Management, with the WebAuthn and FedCM credential types (no credential store,
    // no authenticator, no identity provider).
    "CredentialsContainer",
    "Credential",
    "PasswordCredential",
    "FederatedCredential",
    "PublicKeyCredential",
    "AuthenticatorResponse",
    "AuthenticatorAttestationResponse",
    "AuthenticatorAssertionResponse",
    "IdentityCredential",
    // Prioritized Task Scheduling, and isInputPending.
    "Scheduler",
    "TaskController",
    "TaskSignal",
    "TaskPriorityChangeEvent",
    "Scheduling",
    "PermissionStatus",
    // Performance Timeline and User Timing.
    "PerformanceEntry",
    "PerformanceMark",
    "PerformanceMeasure",
    "PerformanceObserver",
    "PerformanceObserverEntryList",
    // Resource Timing, Navigation Timing (Level 2 and the legacy Level 1 objects), Server Timing,
    // Paint Timing and Event Timing.
    "PerformanceResourceTiming",
    "PerformanceNavigationTiming",
    "PerformanceServerTiming",
    "PerformanceTiming",
    "PerformanceNavigation",
    "PerformancePaintTiming",
    "VisibilityStateEntry",
    "PerformanceEventTiming",
    // DOM.
    "AbortController",
    "MutationObserver",
    "MutationRecord",
    "AbstractRange",
    "Range",
    "StaticRange",
    "Selection",
    "TreeWalker",
    "NodeIterator",
    "XPathResult",
    "XPathExpression",
    "XPathEvaluator",
    "DOMImplementation",
    "XMLDocument",
    "HTMLDocument",
    "DOMParser",
    "XMLSerializer",
    "Sanitizer",
    // Events.
    "CustomEvent",
    "UIEvent",
    "FocusEvent",
    "MouseEvent",
    "WheelEvent",
    "PointerEvent",
    // Touch Events: the interfaces (the legacy `ontouch*` handlers only with a touch device).
    "Touch",
    "TouchList",
    "TouchEvent",
    "InputEvent",
    "KeyboardEvent",
    "CompositionEvent",
    "TextEvent",
    "ErrorEvent",
    "SecurityPolicyViolationEvent",
    "PromiseRejectionEvent",
    "HashChangeEvent",
    "PageTransitionEvent",
    "PopStateEvent",
    "BeforeUnloadEvent",
    "ProgressEvent",
    "SubmitEvent",
    "FormDataEvent",
    "CommandEvent",
    // HTML §6.11 drag and drop: the drag data store's script interfaces and the DND events.
    "DataTransfer",
    "DataTransferItemList",
    "DataTransferItem",
    "DragEvent",
    "ToggleEvent",
    "InputDeviceCapabilities",
    "MessageEvent",
    "MessageChannel",
    "MessagePort",
    "BroadcastChannel",
    // WebSockets.
    "WebSocket",
    "CloseEvent",
    "IdleDeadline",
    // Encoding, URL, geometry.
    "TextEncoder",
    "TextDecoder",
    "URL",
    "URLSearchParams",
    "Headers",
    "Request",
    "Response",
    "Blob",
    "File",
    "FileList",
    "FileReader",
    "FormData",
    "XMLHttpRequestEventTarget",
    "XMLHttpRequestUpload",
    "XMLHttpRequest",
    "WorkerGlobalScope",
    "DedicatedWorkerGlobalScope",
    "WorkerLocation",
    "WorkerNavigator",
    "Worker",
    "ElementInternals",
    "CustomStateSet",
    "DOMRectReadOnly",
    "DOMRect",
    "DOMRectList",
    "DOMPointReadOnly",
    "DOMPoint",
    "DOMQuad",
    "DOMMatrixReadOnly",
    "DOMMatrix",
    // CSSOM and CSSOM View.
    "CSSStyleDeclaration",
    "CSSStyleProperties",
    "StyleSheet",
    "CSSStyleSheet",
    "StyleSheetList",
    "MediaList",
    "CSSRuleList",
    "CSSRule",
    "CSSStyleRule",
    "CSSGroupingRule",
    "CSSConditionRule",
    "CSSMediaRule",
    "CSSSupportsRule",
    "CSSContainerRule",
    "CSSImportRule",
    "CSSFontFaceRule",
    "CSSFontFaceDescriptors",
    "CSSFontFeatureValuesRule",
    "CSSFontPaletteValuesRule",
    "CSSKeyframesRule",
    "CSSKeyframeRule",
    "CSSNamespaceRule",
    "CSSLayerBlockRule",
    "CSSLayerStatementRule",
    "CSSPropertyRule",
    "MediaQueryList",
    "MediaQueryListEvent",
    // Session history.
    "History",
    // Canvas 2D.
    "CanvasRenderingContext2D",
    "CanvasGradient",
    "CanvasPattern",
    "TextMetrics",
    "Path2D",
    "ImageData",
    "OffscreenCanvas",
    "OffscreenCanvasRenderingContext2D",
    "ImageBitmap",
    // WebGL 1.0.
    "WebGLRenderingContext",
    "WebGLObject",
    "WebGLBuffer",
    "WebGLFramebuffer",
    "WebGLProgram",
    "WebGLRenderbuffer",
    "WebGLShader",
    "WebGLTexture",
    "WebGLUniformLocation",
    "WebGLActiveInfo",
    "WebGLShaderPrecisionFormat",
    "WebGLContextEvent",
    // WebGL 2.0.
    "WebGL2RenderingContext",
    "WebGLQuery",
    "WebGLSampler",
    "WebGLSync",
    "WebGLTransformFeedback",
    "WebGLVertexArrayObject",
    // Web Cryptography (getRandomValues, randomUUID).
    "Crypto",
    "SubtleCrypto",
    "CryptoKey",
    // Web storage.
    "Storage",
    "StorageEvent",
    // Indexed Database API 3.0, and HTML's DOMStringList it returns names in.
    "DOMStringList",
    "IDBRequest",
    "IDBOpenDBRequest",
    "IDBVersionChangeEvent",
    "IDBFactory",
    "IDBDatabase",
    "IDBObjectStore",
    "IDBIndex",
    "IDBKeyRange",
    "IDBRecord",
    "IDBCursor",
    "IDBCursorWithValue",
    "IDBTransaction",
    // Intersection Observer.
    "IntersectionObserver",
    "IntersectionObserverEntry",
    // Streams.
    "ReadableStream",
    "ReadableStreamDefaultReader",
    "ReadableStreamBYOBReader",
    "ReadableStreamDefaultController",
    "ReadableByteStreamController",
    "ReadableStreamBYOBRequest",
    "WritableStream",
    "WritableStreamDefaultWriter",
    "WritableStreamDefaultController",
    "TransformStream",
    "TransformStreamDefaultController",
    "ByteLengthQueuingStrategy",
    "CountQueuingStrategy",
    "TextDecoderStream",
    "TextEncoderStream",
    // Compression Standard.
    "CompressionStream",
    "DecompressionStream",
    // CSSOM View: the screen and the visual viewport.
    "Screen",
    "ScreenOrientation",
    "VisualViewport",
    // Resize Observer.
    "ResizeObserver",
    "ResizeObserverEntry",
    "ResizeObserverSize",
    // SVG 2: the element interfaces, the basic data types and the animated value objects
    // (SVG 2 §4), with the SMIL animation elements (SVG Animations), clipping and masking (CSS
    // Masking §14) and the filter elements (Filter Effects §15).
    "SVGElement",
    "SVGGraphicsElement",
    "SVGGeometryElement",
    "SVGNumber",
    "SVGLength",
    "SVGAngle",
    "SVGNumberList",
    "SVGLengthList",
    "SVGStringList",
    "SVGAnimatedBoolean",
    "SVGAnimatedEnumeration",
    "SVGAnimatedInteger",
    "SVGAnimatedNumber",
    "SVGAnimatedLength",
    "SVGAnimatedAngle",
    "SVGAnimatedString",
    "SVGAnimatedRect",
    "SVGAnimatedNumberList",
    "SVGAnimatedLengthList",
    "SVGUnitTypes",
    "SVGSVGElement",
    "SVGGElement",
    "SVGDefsElement",
    "SVGDescElement",
    "SVGMetadataElement",
    "SVGTitleElement",
    "SVGSymbolElement",
    "SVGUseElement",
    "SVGSwitchElement",
    "SVGStyleElement",
    "SVGTransform",
    "SVGTransformList",
    "SVGAnimatedTransformList",
    "SVGPreserveAspectRatio",
    "SVGAnimatedPreserveAspectRatio",
    "SVGPathElement",
    "SVGRectElement",
    "SVGCircleElement",
    "SVGEllipseElement",
    "SVGLineElement",
    "SVGPointList",
    "SVGPolylineElement",
    "SVGPolygonElement",
    "SVGTextContentElement",
    "SVGTextPositioningElement",
    "SVGTextElement",
    "SVGTSpanElement",
    "SVGTextPathElement",
    "SVGImageElement",
    "SVGForeignObjectElement",
    "SVGMarkerElement",
    "SVGGradientElement",
    "SVGLinearGradientElement",
    "SVGRadialGradientElement",
    "SVGStopElement",
    "SVGPatternElement",
    "SVGScriptElement",
    "SVGAElement",
    "SVGViewElement",
    "SVGAnimationElement",
    "SVGAnimateElement",
    "SVGSetElement",
    "SVGAnimateMotionElement",
    "SVGMPathElement",
    "SVGAnimateTransformElement",
    "SVGClipPathElement",
    "SVGMaskElement",
    "SVGFilterElement",
    "SVGFEBlendElement",
    "SVGFEColorMatrixElement",
    "SVGFEComponentTransferElement",
    "SVGComponentTransferFunctionElement",
    "SVGFEFuncRElement",
    "SVGFEFuncGElement",
    "SVGFEFuncBElement",
    "SVGFEFuncAElement",
    "SVGFECompositeElement",
    "SVGFEConvolveMatrixElement",
    "SVGFEDiffuseLightingElement",
    "SVGFEDistantLightElement",
    "SVGFEPointLightElement",
    "SVGFESpotLightElement",
    "SVGFEDisplacementMapElement",
    "SVGFEDropShadowElement",
    "SVGFEFloodElement",
    "SVGFEGaussianBlurElement",
    "SVGFEImageElement",
    "SVGFEMergeElement",
    "SVGFEMergeNodeElement",
    "SVGFEMorphologyElement",
    "SVGFEOffsetElement",
    "SVGFESpecularLightingElement",
    "SVGFETileElement",
    "SVGFETurbulenceElement",
    // CSS Font Loading.
    "FontFace",
    "FontFaceSet",
    "FontFaceSetLoadEvent",
    // HTML §4.8.11 media elements: errors, time ranges, tracks.
    "MediaError",
    "TimeRanges",
    "AudioTrackList",
    "AudioTrack",
    "VideoTrackList",
    "VideoTrack",
    "TextTrackList",
    "TextTrack",
    "TextTrackCueList",
    "TextTrackCue",
    "VTTCue",
    "TrackEvent",
    // Media Playback Quality.
    "VideoPlaybackQuality",
    // Media Source Extensions.
    "MediaSource",
    "SourceBuffer",
    "SourceBufferList",
    // Encrypted Media Extensions (the Clear Key key system).
    "MediaKeySystemAccess",
    "MediaKeys",
    "MediaKeySession",
    "MediaKeyStatusMap",
    "MediaKeyMessageEvent",
    "MediaEncryptedEvent",
    // Web Animations, and the CSS Animations and Transitions objects and events.
    "AnimationTimeline",
    "DocumentTimeline",
    "Animation",
    "CSSAnimation",
    "CSSTransition",
    "AnimationEffect",
    "KeyframeEffect",
    "AnimationPlaybackEvent",
    "AnimationEvent",
    "TransitionEvent",
    // Web Audio API 1.1: the rendering core, the real-time and the offline context.
    "BaseAudioContext",
    "AudioContext",
    "OfflineAudioContext",
    "OfflineAudioCompletionEvent",
    "AudioBuffer",
    "AudioNode",
    "AudioParam",
    "AudioDestinationNode",
    "AudioScheduledSourceNode",
    "AudioBufferSourceNode",
    "ConstantSourceNode",
    "GainNode",
    "OscillatorNode",
    "PeriodicWave",
    "DynamicsCompressorNode",
    "AnalyserNode",
    "BiquadFilterNode",
    "ChannelMergerNode",
    "ChannelSplitterNode",
    "ConvolverNode",
    "DelayNode",
    "IIRFilterNode",
    "PannerNode",
    "AudioListener",
    "StereoPannerNode",
    "WaveShaperNode",
];

/// Members of later levels that are in the IDL but that no shipping engine exposes (Chrome does not):
/// left out, so feature detection does not find a half-built proposal. (interface or dictionary,
/// member)
pub const UNSHIPPED: &[(&str, &str)] = &[
    // Web Audio 1.1's configurable render quantum and the offline context's `close()`.
    ("BaseAudioContext", "renderQuantumSize"),
    ("OfflineAudioContext", "close"),
    ("OfflineAudioContextOptions", "renderSizeHint"),
    ("AudioContextOptions", "renderSizeHint"),
    // Web Animations 2 group effects and timeline-driven playback.
    ("AnimationEffect", "previousSibling"),
    ("AnimationEffect", "nextSibling"),
    ("AnimationEffect", "before"),
    ("AnimationEffect", "after"),
    ("AnimationEffect", "replace"),
    ("AnimationEffect", "remove"),
    ("AnimationTimeline", "play"),
    // Web Animations 2 iteration composite and per-effect playback rate.
    ("KeyframeEffect", "iterationComposite"),
    ("KeyframeEffectOptions", "iterationComposite"),
    ("EffectTiming", "playbackRate"),
    ("OptionalEffectTiming", "playbackRate"),
    // The SVG Paths draft's path data API (Chrome and the svgwg's SVG 2 do not have it).
    // (svg-paths.idl repeats SVGGeometryElement's members on SVGPathElement; SVG 2 and Chrome
    // have them on SVGGeometryElement only)
    ("SVGPathElement", "pathLength"),
    ("SVGPathElement", "getTotalLength"),
    ("SVGPathElement", "getPointAtLength"),
    // Media Capture drafts Chrome does not ship: Audio Output's `selectAudioOutput` (Firefox only),
    // viewport capture, capture handle actions, and getUserMedia semantics.
    ("MediaDevices", "selectAudioOutput"),
    ("MediaDevices", "getViewportMedia"),
    ("MediaDevices", "setSupportedCaptureActions"),
    ("MediaDevices", "oncaptureaction"),
    ("MediaDevices", "defaultSemantics"),
    // Gamepad: multi-touch surfaces, and the extensions' hand, pose and haptic actuator list.
    ("Gamepad", "touches"),
    ("Gamepad", "hand"),
    ("Gamepad", "hapticActuators"),
    ("Gamepad", "pose"),
    ("GamepadHapticActuator", "pulse"),
];

/// Dictionaries an engine algorithm converts from an `any` value itself (IndexedDB §5.12: `getAll()`'s
/// `queryOrOptions` is an `IDBGetAllOptions` when it is not a key range): they get a public
/// `from_value`.
pub const ENGINE_CONVERTED_DICTIONARIES: &[&str] = &["IDBGetAllOptions", "WebGLContextAttributes"];

/// Members of shipped interfaces that the engine does not implement yet, with the reason: left
/// out rather than generated as members that would pretend. (interface or dictionary, member,
/// why)
pub const NOT_YET: &[(&str, &str, &str)] = &[
    // Web Audio §1.2 output device selection: the engine has no audio output devices to choose
    // from yet, so a context renders to the default output.
    (
        "AudioContext",
        "sinkId",
        "audio output device selection is not implemented",
    ),
    (
        "AudioContext",
        "setSinkId",
        "audio output device selection is not implemented",
    ),
    (
        "AudioContext",
        "onsinkchange",
        "audio output device selection is not implemented",
    ),
    (
        "AudioContextOptions",
        "sinkId",
        "audio output device selection is not implemented",
    ),
    // WebGL: what needs WebXR, and sized drawing buffer formats (later stages of WebGL).
    (
        "WebGLRenderingContext",
        "makeXRCompatible",
        "WebXR is not implemented",
    ),
    (
        "WebGLRenderingContext",
        "drawingBufferStorage",
        "sized drawing buffer formats are not implemented",
    ),
    (
        "WebGL2RenderingContext",
        "makeXRCompatible",
        "WebXR is not implemented",
    ),
    (
        "WebGL2RenderingContext",
        "drawingBufferStorage",
        "sized drawing buffer formats are not implemented",
    ),
];

/// Why a member is left out, when it is in [`UNSHIPPED`] or [`NOT_YET`].
pub fn left_out(owner: &str, member: &str) -> Option<&'static str> {
    if UNSHIPPED.iter().any(|&(o, m)| o == owner && m == member) {
        return Some("not shipped (a later level's proposal)");
    }
    NOT_YET
        .iter()
        .find(|&&(o, m, _)| o == owner && m == member)
        .map(|&(_, _, why)| why)
}

/// The object types SVG DOM attributes reflect content attributes through (SVG 2 §4.3.4): an
/// interface's or mixin's readonly attribute of one of these types is a [`Getter::SvgReflect`].
pub const SVG_REFLECTED: &[&str] = &[
    "SVGAnimatedAngle",
    "SVGAnimatedBoolean",
    "SVGAnimatedEnumeration",
    "SVGAnimatedInteger",
    "SVGAnimatedLength",
    "SVGAnimatedLengthList",
    "SVGAnimatedNumber",
    "SVGAnimatedNumberList",
    "SVGAnimatedPreserveAspectRatio",
    "SVGAnimatedRect",
    "SVGAnimatedString",
    "SVGAnimatedTransformList",
    "SVGPointList",
    "SVGStringList",
];

/// Interfaces whose indexed getter is also an operation that throws an IndexSizeError for an index
/// out of range (SVG 2 §4.5.4 `getItem`), where the indexed property is merely absent.
pub const INDEX_SIZE_GETTERS: &[&str] = &[
    "SVGNumberList",
    "SVGLengthList",
    "SVGPointList",
    "SVGTransformList",
    "SVGStringList",
];

/// The namespaces of this milestone (Web IDL §3.13 namespace objects on the global).
pub const NAMESPACES: &[&str] = &["console", "CSS"];

/// The IDL types whose attributes are event handler IDL attributes (HTML §8.1.8.1): they all behave the
/// same, so the generator routes them to `rt::EventHandlers` by event type instead of one trait method
/// pair each.
pub const EVENT_HANDLER_TYPES: &[&str] = &[
    "EventHandler",
    "OnErrorEventHandler",
    "OnBeforeUnloadEventHandler",
];

/// Interfaces whose objects HTML's prose gives an [[IsHTMLDDA]] internal slot and a custom [[Call]]
/// (HTML §2.7.2.1 `HTMLAllCollection`: calling one is `item(nameOrIndex)`). Their instances are
/// "undetectable" and callable.
pub const HTML_DDA: &[&str] = &["HTMLAllCollection"];

/// Interfaces whose objects are "code like" (TC39 dynamic code brand checks, HostGetCodeForEval;
/// Trusted Types: a TrustedScript compiles as its data): V8 lets them through `eval()` and the
/// `Function` constructor to the string compilation hook, and tells it when a `Function`'s source
/// was made only of them (HTML's EnsureCSPDoesNotBlockStringCompilation, all arguments TrustedScript).
pub const CODE_LIKE: &[&str] = &["TrustedScript"];

/// Stringifiers the specs define as throwing (Geometry §6.4: a non-finite element is an
/// InvalidStateError); their trait methods take a `Cx` and return `Result`.
const THROWING_STRINGIFIERS: &[&str] = &["DOMMatrixReadOnly"];

/// The interfaces whose asynchronous iterable declaration has asynchronous iterator return steps
/// (prose, not IDL: Web IDL §3.7.10.2).
const ASYNC_ITERATOR_RETURN: &[&str] = &["ReadableStream"];

/// A value asynchronously iterable declaration: the value type, the declaration's arguments, and the
/// trait methods (initialization steps, next iteration result, return steps when defined).
#[derive(Clone, Debug)]
pub struct AsyncIterPlan {
    pub value: Type,
    pub args: Vec<Argument>,
    pub init: String,
    pub next: String,
    pub ret: Option<String>,
}

/// Attribute getters the specs define as throwing; their trait methods return `Result`.
const THROWING_GETTERS: &[(&str, &str)] = &[
    ("Document", "cookie"),
    // HTML §7.2.2.2: compares its container's origin with the current settings object's.
    ("Window", "frameElement"),
    // HTML §12.2.2: a document with an opaque origin has no storage.
    ("Window", "localStorage"),
    ("Window", "sessionStorage"),
    ("Document", "domain"),
    ("HTMLInputElement", "valueAsDate"),
    ("HTMLInputElement", "valueAsNumber"),
    ("HTMLInputElement", "selectionStart"),
    ("HTMLInputElement", "selectionEnd"),
    ("HTMLInputElement", "selectionDirection"),
    // Fetch §5.3: the body's stream is made on first access (a V8 object, so with a context).
    ("Request", "body"),
    ("Response", "body"),
    // IndexedDB §4.1: an "InvalidStateError" while the request is pending.
    ("IDBRequest", "error"),
    // Streams §5.4.3: throws once the writer released its lock.
    ("WritableStreamDefaultWriter", "desiredSize"),
    // HTML §4.10.5.3.8: the pattern is a JavaScript regular expression, run in a context (they
    // never throw).
    ("ValidityState", "patternMismatch"),
    ("ValidityState", "valid"),
    // HTML §7.2.5: a "SecurityError" when the associated Document is not fully active.
    ("History", "length"),
    ("History", "scrollRestoration"),
    // DOM §8.4: a TypeError when the result is not of the getter's type.
    ("XPathResult", "numberValue"),
    ("XPathResult", "stringValue"),
    ("XPathResult", "booleanValue"),
    ("XPathResult", "singleNodeValue"),
    ("XPathResult", "snapshotLength"),
];

#[derive(Clone, Debug)]
pub enum ReflectRule {
    String {
        content: String,
        nullable: bool,
        null_to_empty: bool,
    },
    Url {
        content: String,
    },
    Boolean {
        content: String,
    },
    Long {
        content: String,
        default: i64,
        non_negative: bool,
    },
    Unsigned {
        content: String,
        kind: &'static str,
        default: u32,
        range: Option<(u32, u32)>,
    },
    Double {
        content: String,
        positive: bool,
        default: String,
    },
    TokenList {
        content: String,
    },
    Enumerated {
        content: String,
        keywords: Vec<(String, String)>,
        missing: Option<String>,
        invalid: Option<String>,
        nullable: bool,
    },
}

#[derive(Clone, Debug)]
pub enum Getter {
    Trait(String),
    Reflect(ReflectRule),
    /// An event handler IDL attribute for the event type given.
    EventHandler(String),
    /// A `CSSStyleProperties` attribute of the CSS property given (CSSOM §6.6.1: its getter is
    /// `getPropertyValue(property)`).
    CssProperty(String),
    /// An `ObservableArray<T>` attribute (Web IDL §3.10): the getter returns the attribute's
    /// observable array exotic object, the setter replaces the backing list's contents.
    ObservableArray(ObservablePlan),
    /// An SVG DOM attribute that reflects a content attribute through an animated value or list
    /// object (SVG 2 §4.3.4): every one routes to `SVGElementImpl::svg_reflect` with its owner
    /// and name, and the object is the same one each time.
    SvgReflect,
}

/// The trait methods behind an `ObservableArray<T>` attribute: the engine keeps the backing list
/// and runs the attribute's "set an indexed value" and "delete an indexed value" steps.
#[derive(Clone, Debug)]
pub struct ObservablePlan {
    /// `T`.
    pub element: Type,
    /// The backing list's size.
    pub length: String,
    /// An item of the backing list.
    pub item: String,
    /// "Set an indexed value" (the attribute's own steps; storing is `store`).
    pub set_indexed_value: String,
    /// "Delete an indexed value" (the attribute's own steps; removing is `store`).
    pub delete_indexed_value: String,
    /// Writes the backing list: the value at an index (appended at its size), or `None`: the last
    /// item removed.
    pub store: String,
}

#[derive(Clone, Debug)]
pub enum Setter {
    None,
    Trait(String),
    Reflect(ReflectRule),
    PutForwards(String),
    Replaceable,
    EventHandler(String),
    /// The `CSSStyleProperties` attribute setter: `setProperty(property, value)`.
    CssProperty(String),
    /// An `ObservableArray<T>` attribute's setter (Web IDL §3.7.6): the backing list emptied, then
    /// each value of the sequence set at its index.
    ObservableArray,
}

/// The primitive result of a getter that can have a Fast API variant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FastRet {
    Bool,
    I32,
    U32,
    F64,
}

#[derive(Clone, Debug)]
pub struct AttrPlan {
    pub name: String,
    pub ty: Type,
    pub owner: String,
    pub is_static: bool,
    pub getter: Getter,
    pub setter: Setter,
    /// The getter's trait method takes a `Cx` and returns `Result`.
    pub getter_fallible: bool,
    pub fast: Option<FastRet>,
    pub unforgeable: bool,
    /// `[CachedAccessor]` (ours, after Blink): V8 reads the value from a private of the holder, which
    /// the engine keeps current (`rt::set_cached_attribute`).
    pub cached: bool,
    pub same_object: bool,
    pub ce_reactions: bool,
    pub secure: bool,
    /// The global names the member is exposed in, when that is narrower than its interface's
    /// (`[Exposed=Window] readonly attribute Document? responseXML` on an interface exposed in workers
    /// too): the template installs it only in realms of those globals.
    pub exposure: Option<Vec<String>>,
    /// `[UaGate=Chromium]`: the member exists only where the user agent in force is of that family
    /// (the template installs it by `InstallOptions::ua_allows`).
    pub ua_gate: Option<String>,
    /// `[TouchDevice]` (ours; Touch Events "conditionally exposing legacy touch event APIs"): the
    /// member exists only where the user agent has a touch device (`InstallOptions::touch_events`).
    pub touch_device: bool,
    pub stringifier: bool,
    /// `[LegacyLenientThis]` (Web IDL §3.3.6): a `this` of the wrong kind gives `undefined` and ignores
    /// the set, so the accessor has no signature.
    pub lenient_this: bool,
    pub idl: String,
}

#[derive(Clone, Debug)]
pub struct OverloadPlan {
    pub args: Vec<Argument>,
    pub ret: Type,
    pub method: String,
    pub idl: String,
}

#[derive(Clone, Debug)]
pub struct OpPlan {
    pub name: String,
    pub owner: String,
    pub is_static: bool,
    pub overloads: Vec<OverloadPlan>,
    pub ce_reactions: bool,
    pub unforgeable: bool,
    pub secure: bool,
    /// As [`AttrPlan::exposure`].
    pub exposure: Option<Vec<String>>,
    pub promise: bool,
}

#[derive(Clone, Debug)]
pub struct SpecialPlan {
    /// The trait method (the named operation's, or `indexed_getter` etc. for unnamed ones).
    pub method: String,
    pub owner: String,
    pub key: Type,
    pub value: Type,
    pub ce_reactions: bool,
}

#[derive(Clone, Debug)]
pub enum Stringifier {
    Attribute(String),
    /// The owner, its trait method, and whether the method can throw.
    Method(String, String, bool),
}

#[derive(Clone, Debug)]
pub struct IfacePlan {
    pub name: String,
    pub file: String,
    pub lo: u16,
    pub hi: u16,
    pub parent: Option<String>,
    pub attrs: Vec<AttrPlan>,
    pub ops: Vec<OpPlan>,
    pub consts: Vec<Const>,
    /// The identifiers of its `[Unscopable]` regular attributes and operations, its mixins' included
    /// (Web IDL §3.7.3, `@@unscopables`), in member order.
    pub unscopables: Vec<String>,
    pub ctor: Option<OpPlan>,
    pub html_constructor: bool,
    pub factories: Vec<OpPlan>,
    pub indexed_getter: Option<SpecialPlan>,
    pub indexed_setter: Option<SpecialPlan>,
    pub named_getter: Option<SpecialPlan>,
    pub named_setter: Option<SpecialPlan>,
    pub named_deleter: Option<SpecialPlan>,
    pub override_builtins: bool,
    pub unenumerable_named: bool,
    pub stringifier: Option<Stringifier>,
    pub value_iterable: bool,
    /// `iterable<K, V>`: the key and value types and the trait method returning the pair at an index.
    pub pair_iterable: Option<(Type, Type, String)>,
    /// `async_iterable<V>(args)`: a value asynchronously iterable declaration (Web IDL §3.7.10).
    pub async_iterable: Option<AsyncIterPlan>,
    /// `setlike<T>` (Web IDL §3.7.12): the value type, whether it is read-only, and the trait methods
    /// (at, size, has, add, delete, clear; the last three absent when read-only).
    pub setlike: Option<SetlikePlan>,
    /// `[Global]`: members live on the global object itself (Web IDL §3.7, "[Global]").
    pub global: bool,
    /// A namespace (Web IDL §3.13): a plain object with operations, no prototype id.
    pub namespace: bool,
    /// Owners (this interface and mixins) whose traits the members use.
    pub uses: BTreeSet<String>,
    /// The global names the interface is exposed in.
    pub exposure: BTreeSet<String>,
    /// `[Default] object toJSON()` (Web IDL §3.7.7.1 "default toJSON steps"): the identifiers of the
    /// attributes of the interface's inheritance stack whose values it collects, in order.
    pub default_json: Option<Vec<String>>,
}

/// A `setlike<T>` declaration's trait methods.
#[derive(Clone, Debug)]
pub struct SetlikePlan {
    pub value: Type,
    pub readonly: bool,
    pub at: String,
    pub size: String,
    pub has: String,
    pub add: Option<String>,
    pub delete: Option<String>,
    pub clear: Option<String>,
}

/// A callback interface with constants: it gets a legacy callback interface object (Web IDL §3.10).
#[derive(Clone, Debug)]
pub struct CallbackObjectPlan {
    pub name: String,
    pub consts: Vec<Const>,
}

/// A trait method the engine implements.
#[derive(Clone, Debug)]
pub struct MethodSig {
    pub name: String,
    pub doc: String,
    pub cx: bool,
    pub this: bool,
    pub params: Vec<(String, String)>,
    pub ret: String,
}

#[derive(Clone, Debug)]
pub struct TraitPlan {
    pub owner: String,
    pub trait_name: String,
    pub file: String,
    pub is_mixin: bool,
    pub methods: Vec<MethodSig>,
    used_names: BTreeSet<String>,
    /// For mixins, which are planned at their first inclusion: member key → trait method, so later
    /// inclusions call the same methods.
    keyed: BTreeMap<String, String>,
}

impl TraitPlan {
    fn unique(&mut self, base: &str) -> String {
        let mut n = base.to_string();
        let mut i = 2;
        while self.used_names.contains(&n) {
            n = format!("{base}_{i}");
            i += 1;
        }
        self.used_names.insert(n.clone());
        n
    }
}

/// A union type's Rust enum.
#[derive(Clone, Debug)]
pub struct UnionPlan {
    pub rust: String,
    pub members: Vec<Type>,
}

pub struct Plan<'m> {
    pub model: &'m Model,
    pub ifaces: Vec<IfacePlan>,
    pub namespaces: Vec<IfacePlan>,
    pub callback_objects: Vec<CallbackObjectPlan>,
    pub ids: BTreeMap<String, (u16, u16)>,
    pub traits: BTreeMap<String, TraitPlan>,
    pub dictionaries: BTreeSet<String>,
    pub enums: BTreeSet<String>,
    pub callbacks: BTreeSet<String>,
    pub callback_interfaces: BTreeSet<String>,
    pub unions: BTreeMap<String, UnionPlan>,
    pub omitted: Vec<String>,
}

/// The enumerated attribute table (`data/enumerated.txt`).
pub struct Enumerated {
    pub content: String,
    pub keywords: Vec<(String, String)>,
    pub missing: Option<String>,
    pub invalid: Option<String>,
}

pub fn parse_enumerated(src: &str) -> HashMap<(String, String), Enumerated> {
    let mut out = HashMap::new();
    for line in src.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let parts: Vec<&str> = line.split('|').map(str::trim).collect();
        assert_eq!(parts.len(), 4, "bad enumerated line: {line}");
        let mut head = parts[0].split_whitespace();
        let target = head.next().unwrap();
        let content = head.next().unwrap().to_string();
        let (iface, attr) = target.split_once('.').unwrap();
        let keywords = parts[1]
            .split_whitespace()
            .map(|k| match k.split_once('=') {
                Some((a, b)) => (a.to_string(), b.to_string()),
                None => (k.to_string(), k.to_string()),
            })
            .collect();
        let default = |s: &str| match s {
            "null" => None,
            "empty" => Some(String::new()),
            k => Some(k.to_string()),
        };
        out.insert(
            (iface.to_string(), attr.to_string()),
            Enumerated {
                content,
                keywords,
                missing: default(parts[2]),
                invalid: default(parts[3]),
            },
        );
    }
    out
}

/// The idl text of a member, for doc comments.
fn idl_of(m: &Member) -> String {
    fn ext(e: &[ExtAttr]) -> String {
        if e.is_empty() {
            return String::new();
        }
        let items: Vec<String> = e
            .iter()
            .map(|a| match &a.value {
                ExtValue::None => a.name.clone(),
                ExtValue::Token(t) => format!("{}={t}", a.name),
                ExtValue::Wildcard => format!("{}=*", a.name),
                ExtValue::List(l) => format!("{}=({})", a.name, l.join(", ")),
                ExtValue::Args(_) => format!("{}(...)", a.name),
                ExtValue::NamedArgs(n, _) => format!("{}={n}(...)", a.name),
            })
            .collect();
        format!("[{}] ", items.join(", "))
    }
    let args = |args: &[Argument]| {
        args.iter()
            .map(|a| {
                format!(
                    "{}{}{}{} {}{}",
                    if a.optional { "optional " } else { "" },
                    ext(&a.ty.ext),
                    a.ty.idl(),
                    if a.variadic { "..." } else { "" },
                    a.name,
                    match &a.default {
                        Some(d) => format!(" = {}", literal_idl(d)),
                        None => String::new(),
                    }
                )
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    match m {
        Member::Attribute(a) => format!(
            "{}{}{}{}attribute {} {};",
            ext(&a.ext),
            if a.is_static { "static " } else { "" },
            if a.stringifier { "stringifier " } else { "" },
            if a.readonly { "readonly " } else { "" },
            a.ty.idl(),
            a.name
        ),
        Member::Operation(o) => format!(
            "{}{}{}{} {}({});",
            ext(&o.ext),
            if o.is_static { "static " } else { "" },
            match o.special {
                Some(Special::Getter) => "getter ",
                Some(Special::Setter) => "setter ",
                Some(Special::Deleter) => "deleter ",
                None => "",
            },
            o.ret.idl(),
            o.name.clone().unwrap_or_default(),
            args(&o.args)
        ),
        Member::Constructor(a, e) => format!("{}constructor({});", ext(e), args(a)),
        other => member_name(other),
    }
}

pub fn literal_idl(l: &Literal) -> String {
    match l {
        Literal::Bool(b) => b.to_string(),
        Literal::Integer(s) | Literal::Decimal(s) => s.clone(),
        Literal::Infinity => "Infinity".into(),
        Literal::NegInfinity => "-Infinity".into(),
        Literal::NaN => "NaN".into(),
        Literal::String(s) => format!("\"{s}\""),
        Literal::Null => "null".into(),
        Literal::Undefined => "undefined".into(),
        Literal::EmptySequence => "[]".into(),
        Literal::EmptyDictionary => "{}".into(),
    }
}

impl<'m> Plan<'m> {
    pub fn new(model: &'m Model, enumerated: &HashMap<(String, String), Enumerated>) -> Plan<'m> {
        let mut p = Plan {
            model,
            ifaces: Vec::new(),
            namespaces: Vec::new(),
            callback_objects: Vec::new(),
            ids: BTreeMap::new(),
            traits: BTreeMap::new(),
            dictionaries: BTreeSet::new(),
            enums: BTreeSet::new(),
            callbacks: BTreeSet::new(),
            callback_interfaces: BTreeSet::new(),
            unions: BTreeMap::new(),
            omitted: Vec::new(),
        };
        let set = p.interface_set();
        p.assign_ids(&set);
        let order: Vec<String> = {
            let mut v: Vec<(u16, String)> =
                p.ids.iter().map(|(n, (lo, _))| (*lo, n.clone())).collect();
            v.sort();
            v.into_iter().map(|(_, n)| n).collect()
        };
        for name in order {
            let plan = p.plan_interface(&name, enumerated);
            p.ifaces.push(plan);
        }
        for name in NAMESPACES {
            let i = model
                .interfaces
                .get(*name)
                .unwrap_or_else(|| panic!("namespace {name} is not in the IDL"));
            assert_eq!(
                i.kind,
                InterfaceKind::Namespace,
                "{name} is not a namespace"
            );
            let plan = p.plan_interface(name, enumerated);
            p.namespaces.push(plan);
        }
        for (name, i) in &model.interfaces {
            if i.kind != InterfaceKind::CallbackInterface || !i.exposed_in(GLOBAL) {
                continue;
            }
            let consts: Vec<Const> = i
                .members
                .iter()
                .filter_map(|m| match &m.member {
                    Member::Const(c) => Some(c.clone()),
                    _ => None,
                })
                .collect();
            if !consts.is_empty() {
                p.callback_objects.push(CallbackObjectPlan {
                    name: name.clone(),
                    consts,
                });
            }
        }
        p
    }

    fn interface_set(&self) -> BTreeSet<String> {
        let m = self.model;
        let mut set: BTreeSet<String> = SCOPE.iter().map(|s| s.to_string()).collect();
        for (name, i) in &m.interfaces {
            if i.kind == InterfaceKind::Interface
                && i.exposed_in(GLOBAL)
                && m.inherits_from(name, "HTMLElement")
            {
                set.insert(name.clone());
            }
        }
        for n in &set {
            let i = m
                .interfaces
                .get(n)
                .unwrap_or_else(|| panic!("scope interface {n} is not in the IDL"));
            assert!(
                exposed_any(&i.exposed),
                "{n} is not exposed in a generated global"
            );
            if let Some(parent) = &i.inherits {
                assert!(
                    set.contains(parent),
                    "{n} inherits {parent}, which is not generated"
                );
            }
        }
        set
    }

    /// Prototype ids in depth-first order of the inheritance forest (siblings by name), from 1.
    fn assign_ids(&mut self, set: &BTreeSet<String>) {
        let mut children: BTreeMap<Option<String>, Vec<String>> = BTreeMap::new();
        for n in set {
            let parent = self.model.interfaces[n].inherits.clone();
            children.entry(parent).or_default().push(n.clone());
        }
        fn visit(
            name: &str,
            children: &BTreeMap<Option<String>, Vec<String>>,
            next: &mut u16,
            ids: &mut BTreeMap<String, (u16, u16)>,
        ) {
            let lo = *next;
            *next += 1;
            if let Some(kids) = children.get(&Some(name.to_string())) {
                for k in kids {
                    visit(k, children, next, ids);
                }
            }
            ids.insert(name.to_string(), (lo, *next));
        }
        let mut next = 1u16;
        let roots = children.get(&None).cloned().unwrap_or_default();
        for r in roots {
            visit(&r, &children, &mut next, &mut self.ids);
        }
    }

    // ---- type support ------------------------------------------------------------------------------

    /// Why a type cannot be generated yet, or `None` when it can.
    /// A nullable interface type whose interface is not generated: no value of it can exist, so it is
    /// always null (a required dictionary member of it is written as null).
    pub fn always_null(&self, ty: &Type) -> bool {
        let ty = self.model.resolve(ty);
        ty.nullable
            && matches!(&ty.kind, TypeKind::Named(n)
                if matches!(self.model.kind_of(n), NamedKind::Interface) && !self.ids.contains_key(n))
    }

    pub fn unsupported(&self, ty: &Type) -> Option<String> {
        self.unsupported_rec(ty, &mut BTreeSet::new())
    }

    fn unsupported_rec(&self, ty: &Type, seen: &mut BTreeSet<String>) -> Option<String> {
        let ty = self.model.resolve(ty);
        match &ty.kind {
            TypeKind::Prim(Prim::BigInt) => Some("bigint is not supported yet".into()),
            TypeKind::Prim(_) => None,
            TypeKind::Named(n) => match self.model.kind_of(n) {
                NamedKind::Interface => {
                    if self.ids.contains_key(n) {
                        None
                    } else {
                        Some(format!("interface {n} is not generated"))
                    }
                }
                NamedKind::Enum | NamedKind::Buffer => None,
                // Optional members of types not generated are left out of the dictionary; a required
                // one makes the dictionary unsupported, unless it can only ever be null.
                NamedKind::Dictionary => {
                    if !seen.insert(n.clone()) {
                        return None;
                    }
                    for d in self.model.dictionary_chain(n) {
                        for dm in &d.members {
                            if !dm.required || self.always_null(&dm.ty) {
                                continue;
                            }
                            if let Some(r) = self.unsupported_rec(&dm.ty, seen) {
                                return Some(format!("dictionary {n}: {r}"));
                            }
                        }
                    }
                    None
                }
                NamedKind::Callback => {
                    if !seen.insert(n.clone()) {
                        return None;
                    }
                    let c = &self.model.callbacks[n];
                    if let Some(r) = self.unsupported_rec(&c.ret, seen) {
                        return Some(format!("callback {n}: {r}"));
                    }
                    for a in &c.args {
                        if let Some(r) = self.unsupported_rec(&a.ty, seen) {
                            return Some(format!("callback {n}: {r}"));
                        }
                    }
                    None
                }
                NamedKind::CallbackInterface => {
                    if !seen.insert(n.clone()) {
                        return None;
                    }
                    let ops = self.callback_interface_ops(n);
                    if ops.len() != 1 {
                        return Some(format!(
                            "callback interface {n} with {} operations",
                            ops.len()
                        ));
                    }
                    let o = ops[0];
                    for t in std::iter::once(&o.ret).chain(o.args.iter().map(|a| &a.ty)) {
                        if let Some(r) = self.unsupported_rec(t, seen) {
                            return Some(format!("callback interface {n}: {r}"));
                        }
                    }
                    None
                }
                k => Some(format!("{n} is a {k:?}")),
            },
            TypeKind::Sequence(t) | TypeKind::FrozenArray(t) => self.unsupported_rec(t, seen),
            TypeKind::Record(_, t) => self.unsupported_rec(t, seen),
            TypeKind::Promise(t) => {
                if t.is_undefined() || t.as_prim() == Some(Prim::Any) {
                    None
                } else {
                    self.unsupported_rec(t, seen)
                }
            }
            TypeKind::ObservableArray(_) => Some("ObservableArray is not supported yet".into()),
            TypeKind::AsyncSequence(t) => self.unsupported_rec(t, seen),
            // A union whose members are partly not generated keeps the others (see `prune`).
            TypeKind::Union(ts) => {
                let mut first = None;
                for t in ts {
                    let r = self.unsupported_rec(t, seen)?;
                    first.get_or_insert(r);
                }
                first
            }
        }
    }

    /// The type with typedefs expanded and the union members that are not generated left out (a union
    /// left with one member becomes that member). Every emitted conversion and Rust type goes through
    /// this, so a partly supported union is generated over what Snowlark has.
    pub fn prune(&self, ty: &Type) -> Type {
        let ty = self.model.resolve(ty);
        let p = |t: &Type| Box::new(self.prune(t));
        let kind = match &ty.kind {
            TypeKind::Union(ts) => {
                let kept: Vec<Type> = ts
                    .iter()
                    .filter(|t| self.unsupported(t).is_none())
                    .map(|t| self.prune(t))
                    .collect();
                if kept.len() == 1 && kept.len() != ts.len() {
                    let mut t = kept.into_iter().next().unwrap();
                    t.nullable |= ty.nullable;
                    t.ext.extend(ty.ext.iter().cloned());
                    return t;
                }
                if kept.is_empty() {
                    TypeKind::Union(ts.clone())
                } else {
                    TypeKind::Union(kept)
                }
            }
            TypeKind::Sequence(t) => TypeKind::Sequence(p(t)),
            TypeKind::FrozenArray(t) => TypeKind::FrozenArray(p(t)),
            TypeKind::Record(k, v) => TypeKind::Record(*k, p(v)),
            TypeKind::Promise(t) => TypeKind::Promise(p(t)),
            other => other.clone(),
        };
        Type {
            kind,
            nullable: ty.nullable,
            ext: ty.ext.clone(),
        }
    }

    pub fn callback_interface_ops(&self, name: &str) -> Vec<&Operation> {
        self.model.interfaces[name]
            .members
            .iter()
            .filter_map(|m| match &m.member {
                Member::Operation(o) => Some(o),
                _ => None,
            })
            .collect()
    }

    /// Records the dictionaries, enums, unions and callbacks a supported type uses.
    pub fn collect(&mut self, ty: &Type) {
        let ty = self.prune(ty);
        match &ty.kind {
            TypeKind::Named(n) => match self.model.kind_of(n) {
                NamedKind::Enum => {
                    self.enums.insert(n.clone());
                }
                NamedKind::Dictionary => {
                    if self.dictionaries.insert(n.clone()) {
                        let types: Vec<Type> = self
                            .dictionary_members(n)
                            .into_iter()
                            .map(|m| m.ty)
                            .collect();
                        for t in types {
                            self.collect(&t);
                        }
                    }
                }
                NamedKind::Callback => {
                    if self.callbacks.insert(n.clone()) {
                        let c = self.model.callbacks[n].clone();
                        self.collect(&c.ret);
                        for a in &c.args {
                            self.collect(&a.ty);
                        }
                    }
                }
                NamedKind::CallbackInterface if self.callback_interfaces.insert(n.clone()) => {
                    let ops: Vec<Operation> = self
                        .callback_interface_ops(n)
                        .into_iter()
                        .cloned()
                        .collect();
                    for o in ops {
                        self.collect(&o.ret);
                        for a in &o.args {
                            self.collect(&a.ty);
                        }
                    }
                }
                _ => {}
            },
            TypeKind::Sequence(t)
            | TypeKind::FrozenArray(t)
            | TypeKind::Record(_, t)
            | TypeKind::Promise(t) => self.collect(t),
            TypeKind::Union(ts) => {
                let name = self.union_name(&ty);
                for t in ts {
                    self.collect(t);
                }
                self.unions.entry(name.clone()).or_insert(UnionPlan {
                    rust: name,
                    members: ts.clone(),
                });
            }
            _ => {}
        }
    }

    /// The Rust name of a (resolved) union type: its member names joined by `Or`.
    pub fn union_name(&self, ty: &Type) -> String {
        let TypeKind::Union(ts) = &ty.kind else {
            unreachable!()
        };
        ts.iter()
            .map(|t| self.type_label(t))
            .collect::<Vec<_>>()
            .join("Or")
    }

    /// The variant name of a union member.
    pub fn variant_label(&self, t: &Type) -> String {
        self.type_label(&Type {
            ext: Vec::new(),
            nullable: false,
            ..t.clone()
        })
    }

    /// A short CamelCase label for a type, for union and variant names.
    pub fn type_label(&self, t: &Type) -> String {
        let base = match &t.kind {
            TypeKind::Prim(p) => match p {
                Prim::DOMString | Prim::USVString | Prim::ByteString => "String".to_string(),
                Prim::UnrestrictedDouble => "UnrestrictedDouble".into(),
                Prim::UnrestrictedFloat => "UnrestrictedFloat".into(),
                Prim::UnsignedLong => "UnsignedLong".into(),
                Prim::UnsignedShort => "UnsignedShort".into(),
                Prim::LongLong => "LongLong".into(),
                Prim::UnsignedLongLong => "UnsignedLongLong".into(),
                p => camel(p.idl_name()),
            },
            TypeKind::Named(n) => n.clone(),
            TypeKind::Sequence(t) => format!("{}Sequence", self.type_label(t)),
            TypeKind::FrozenArray(t) => format!("{}FrozenArray", self.type_label(t)),
            TypeKind::Record(_, t) => format!("{}Record", self.type_label(t)),
            TypeKind::Promise(t) => format!("{}Promise", self.type_label(t)),
            TypeKind::Union(_) => self.union_name(t),
            TypeKind::ObservableArray(t) | TypeKind::AsyncSequence(t) => self.type_label(t),
        };
        let mut prefix = String::new();
        for (attr, label) in [
            ("LegacyNullToEmptyString", "NullEmpty"),
            ("Clamp", "Clamp"),
            ("EnforceRange", "EnforceRange"),
        ] {
            if t.has_ext(attr) {
                prefix.push_str(label);
            }
        }
        if t.nullable {
            format!("Nullable{prefix}{base}")
        } else {
            format!("{prefix}{base}")
        }
    }

    // ---- Rust types -------------------------------------------------------------------------------

    /// The Rust type of an argument, dictionary member or callback argument. `owned` for values inside
    /// containers (strings as `String`).
    pub fn rust_arg(&self, ty: &Type, owned: bool) -> String {
        let ty = self.prune(ty);
        let base = match &ty.kind {
            TypeKind::Prim(p) => match p {
                Prim::DOMString | Prim::USVString | Prim::ByteString => {
                    if owned {
                        "String".into()
                    } else {
                        "&str".into()
                    }
                }
                Prim::Any => "JsValue".into(),
                Prim::Object | Prim::Symbol => {
                    if *p == Prim::Object {
                        "JsObject".into()
                    } else {
                        "JsValue".into()
                    }
                }
                p => prim_rust(*p).into(),
            },
            TypeKind::Named(n) => match self.model.kind_of(n) {
                NamedKind::Interface => "Handle".into(),
                NamedKind::Buffer => "JsObject".into(),
                _ => n.clone(),
            },
            TypeKind::Sequence(t) | TypeKind::FrozenArray(t) => {
                format!("Vec<{}>", self.rust_arg(t, true))
            }
            TypeKind::Record(_, t) => format!("Record<{}>", self.rust_arg(t, true)),
            TypeKind::Promise(_) => "JsObject".into(),
            TypeKind::AsyncSequence(_) => "rt::AsyncSequence".into(),
            TypeKind::Union(_) => self.union_name(&Type {
                nullable: false,
                ..ty.clone()
            }),
            _ => "JsValue".into(),
        };
        if ty.nullable && !matches!(ty.kind, TypeKind::Prim(Prim::Any)) {
            format!("Option<{base}>")
        } else {
            base
        }
    }

    /// The Rust type of a result.
    pub fn rust_ret(&self, ty: &Type) -> String {
        let ty = self.prune(ty);
        let base = match &ty.kind {
            TypeKind::Prim(Prim::Undefined) => "()".into(),
            TypeKind::Prim(p) if p.is_string() => "DomStr<'_>".into(),
            TypeKind::Promise(_) => "JsPromise".into(),
            TypeKind::Union(_) => self.union_name(&Type {
                nullable: false,
                ..ty.clone()
            }),
            _ => {
                return self.rust_arg(&ty, true);
            }
        };
        if ty.nullable {
            format!("Option<{base}>")
        } else {
            base
        }
    }

    /// Whether an argument or result needs the execution context (JS values, callbacks).
    pub fn needs_cx(&self, ty: &Type) -> bool {
        let ty = self.prune(ty);
        match &ty.kind {
            TypeKind::Prim(Prim::Any | Prim::Object | Prim::Symbol) => true,
            TypeKind::Promise(_) | TypeKind::AsyncSequence(_) => true,
            TypeKind::Named(n) => matches!(
                self.model.kind_of(n),
                NamedKind::Callback | NamedKind::CallbackInterface | NamedKind::Buffer
            ),
            TypeKind::Sequence(t) | TypeKind::FrozenArray(t) | TypeKind::Record(_, t) => {
                self.needs_cx(t)
            }
            TypeKind::Union(ts) => ts.iter().any(|t| self.needs_cx(t)),
            _ => false,
        }
    }

    fn fast_ret(&self, ty: &Type) -> Option<FastRet> {
        let ty = self.prune(ty);
        if ty.nullable {
            return None;
        }
        match ty.as_prim()? {
            Prim::Boolean => Some(FastRet::Bool),
            Prim::Byte | Prim::Short | Prim::Long => Some(FastRet::I32),
            Prim::Octet | Prim::UnsignedShort | Prim::UnsignedLong => Some(FastRet::U32),
            Prim::Float | Prim::UnrestrictedFloat | Prim::Double | Prim::UnrestrictedDouble => {
                Some(FastRet::F64)
            }
            _ => None,
        }
    }

    // ---- members -----------------------------------------------------------------------------------

    fn trait_for(&mut self, owner: &str, is_mixin: bool) -> &mut TraitPlan {
        self.traits
            .entry(owner.to_string())
            .or_insert_with(|| TraitPlan {
                owner: owner.to_string(),
                trait_name: format!("{}Impl", upper_first(owner)),
                file: snake(owner).trim_end_matches('_').to_string(),
                is_mixin,
                methods: Vec::new(),
                used_names: BTreeSet::new(),
                keyed: BTreeMap::new(),
            })
    }

    fn add_method(
        &mut self,
        owner: &str,
        is_mixin: bool,
        base: &str,
        mut sig: MethodSig,
    ) -> String {
        let t = self.trait_for(owner, is_mixin);
        let name = t.unique(base);
        sig.name = name.clone();
        t.methods.push(sig);
        name
    }

    fn omit(&mut self, iface: &str, member: &str, why: &str) {
        self.omitted.push(format!("{iface}.{member}: {why}"));
    }

    fn reflect_rule(
        &self,
        iface: &str,
        a: &Attribute,
        enumerated: &HashMap<(String, String), Enumerated>,
    ) -> Option<(ReflectRule, bool)> {
        let ty = self.prune(&a.ty);
        if let Some(e) = enumerated.get(&(iface.to_string(), a.name.clone())) {
            return Some((
                ReflectRule::Enumerated {
                    content: e.content.clone(),
                    keywords: e.keywords.clone(),
                    missing: e.missing.clone(),
                    invalid: e.invalid.clone(),
                    nullable: ty.nullable,
                },
                false,
            ));
        }
        // Reflection the HTML prose defines but its IDL does not annotate yet.
        let mut patched;
        let ext = match REFLECT_PROSE
            .iter()
            .find(|(i, n, _, _)| *i == iface && *n == a.name)
        {
            Some((_, _, kind, default)) => {
                patched = a.ext.clone();
                patched.retain(|x| !x.name.starts_with("Reflect"));
                patched.push(crate::ast::ExtAttr {
                    name: kind.to_string(),
                    value: crate::ast::ExtValue::None,
                });
                patched.push(crate::ast::ExtAttr {
                    name: "ReflectDefault".into(),
                    value: crate::ast::ExtValue::Token(default.to_string()),
                });
                &patched
            }
            None => &a.ext,
        };
        let setter_only = has_ext(ext, "ReflectSetter");
        let reflect = ext.iter().find(|x| {
            matches!(
                x.name.as_str(),
                "Reflect"
                    | "ReflectURL"
                    | "ReflectNonNegative"
                    | "ReflectPositive"
                    | "ReflectPositiveWithFallback"
                    | "ReflectSetter"
            )
        })?;
        let content = ext
            .iter()
            .filter(|x| x.name.starts_with("Reflect"))
            .find_map(|x| x.token().filter(|_| x.name != "ReflectDefault"))
            .map(str::to_string)
            .unwrap_or_else(|| a.name.to_ascii_lowercase());
        let default = ext
            .iter()
            .find(|x| x.name == "ReflectDefault")
            .and_then(|x| x.token());
        let range = ext.iter().find(|x| x.name == "ReflectRange").map(|x| {
            let t = x.tokens();
            (
                parse_integer(t[0]).unwrap() as u32,
                parse_integer(t[1]).unwrap() as u32,
            )
        });
        let rule = match &ty.kind {
            TypeKind::Prim(Prim::Boolean) => ReflectRule::Boolean { content },
            TypeKind::Prim(p) if p.is_string() => {
                if reflect.name == "ReflectURL" {
                    ReflectRule::Url { content }
                } else {
                    ReflectRule::String {
                        content,
                        nullable: ty.nullable,
                        null_to_empty: ty.has_ext("LegacyNullToEmptyString"),
                    }
                }
            }
            TypeKind::Prim(Prim::Long) => ReflectRule::Long {
                content,
                non_negative: reflect.name == "ReflectNonNegative",
                default: default.and_then(parse_integer).map(|d| d as i64).unwrap_or(
                    if reflect.name == "ReflectNonNegative" {
                        -1
                    } else {
                        0
                    },
                ),
            },
            TypeKind::Prim(Prim::UnsignedLong) => {
                let kind = match reflect.name.as_str() {
                    "ReflectPositive" => "Positive",
                    "ReflectPositiveWithFallback" => "PositiveWithFallback",
                    _ => "Plain",
                };
                ReflectRule::Unsigned {
                    content,
                    kind,
                    default: default
                        .and_then(parse_integer)
                        .map(|d| d as u32)
                        .unwrap_or(if kind == "Plain" { 0 } else { 1 }),
                    range,
                }
            }
            TypeKind::Prim(Prim::Double | Prim::UnrestrictedDouble) => ReflectRule::Double {
                content,
                positive: reflect.name == "ReflectPositive",
                default: default.map(str::to_string).unwrap_or_else(|| "0.0".into()),
            },
            TypeKind::Named(n) if n == "DOMTokenList" => ReflectRule::TokenList { content },
            // Element and FrozenArray<Element> reflection keep explicitly set elements: engine work.
            _ => return None,
        };
        Some((rule, setter_only))
    }

    fn plan_interface(
        &mut self,
        name: &str,
        enumerated: &HashMap<(String, String), Enumerated>,
    ) -> IfacePlan {
        let model = self.model;
        let iface = &model.interfaces[name];
        let namespace = iface.kind == InterfaceKind::Namespace;
        let (lo, hi) = self.ids.get(name).copied().unwrap_or((0, 0));
        let mut plan = IfacePlan {
            name: name.to_string(),
            file: snake(name).trim_end_matches('_').to_string(),
            lo,
            hi,
            parent: iface.inherits.clone(),
            attrs: Vec::new(),
            ops: Vec::new(),
            consts: Vec::new(),
            unscopables: Vec::new(),
            ctor: None,
            html_constructor: false,
            factories: Vec::new(),
            indexed_getter: None,
            indexed_setter: None,
            named_getter: None,
            named_setter: None,
            named_deleter: None,
            override_builtins: iface.has_ext("LegacyOverrideBuiltIns"),
            unenumerable_named: iface.has_ext("LegacyUnenumerableNamedProperties"),
            stringifier: None,
            value_iterable: false,
            pair_iterable: None,
            async_iterable: None,
            setlike: None,
            global: iface.has_ext("Global"),
            namespace,
            uses: BTreeSet::new(),
            exposure: iface.exposed.clone(),
            default_json: None,
        };
        self.trait_for(name, false);
        plan.uses.insert(name.to_string());

        // Group operations by owner, name and staticness, in member order.
        let mut op_groups: Vec<(String, String, bool, Vec<&IMember>)> = Vec::new();
        let mut ctors: Vec<&IMember> = Vec::new();
        for im in &iface.members {
            if !exposed_any(&im.exposed) || left_out(name, &member_name(&im.member)).is_some() {
                continue;
            }
            let owner = im.mixin.clone().unwrap_or_else(|| name.to_string());
            match &im.member {
                Member::Operation(o) if o.special.is_none() && o.name.is_some() => {
                    let n = o.name.clone().unwrap();
                    match op_groups
                        .iter_mut()
                        .find(|g| g.0 == owner && g.1 == n && g.2 == o.is_static)
                    {
                        Some(g) => g.3.push(im),
                        None => op_groups.push((owner, n, o.is_static, vec![im])),
                    }
                }
                Member::Constructor(..) => ctors.push(im),
                _ => {}
            }
        }

        for im in &iface.members {
            if !exposed_any(&im.exposed) {
                continue;
            }
            if let Some(why) = left_out(name, &member_name(&im.member)) {
                self.omit(name, &member_name(&im.member), why);
                continue;
            }
            let owner = im.mixin.clone().unwrap_or_else(|| name.to_string());
            let is_mixin = im.mixin.is_some();
            if ext(im.member.ext(), "Unscopable").is_some() {
                let n = member_name(&im.member);
                if !n.is_empty() && !plan.unscopables.contains(&n) {
                    plan.unscopables.push(n);
                }
            }
            match &im.member {
                Member::Const(c) => plan.consts.push(c.clone()),
                Member::Attribute(a) => {
                    if let Some(mut ap) =
                        self.plan_attribute(name, &owner, is_mixin, im, a, enumerated)
                    {
                        ap.exposure = narrower_exposure(&iface.exposed, &im.exposed);
                        ap.ua_gate = ext(im.member.ext(), "UaGate")
                            .and_then(|e| e.tokens().first().map(|t| (*t).to_owned()));
                        ap.touch_device = ext(im.member.ext(), "TouchDevice").is_some();
                        plan.uses.insert(owner.clone());
                        if a.stringifier {
                            plan.stringifier = Some(Stringifier::Attribute(a.name.clone()));
                        }
                        plan.attrs.push(ap);
                    }
                }
                Member::Operation(o) if o.special.is_some() => {
                    self.plan_special(name, &owner, is_mixin, o, &mut plan);
                    plan.uses.insert(owner.clone());
                }
                Member::Operation(o) if o.name.is_none() && o.stringifier => {
                    let throws = THROWING_STRINGIFIERS.contains(&owner.as_str());
                    let m = self.add_method(
                        &owner,
                        is_mixin,
                        "stringify",
                        MethodSig {
                            name: String::new(),
                            doc: idl_of(&im.member),
                            cx: throws,
                            this: true,
                            params: vec![],
                            ret: if throws {
                                "Result<DomStr<'_>>".into()
                            } else {
                                "DomStr<'_>".into()
                            },
                        },
                    );
                    plan.stringifier = Some(Stringifier::Method(owner.clone(), m, throws));
                }
                Member::Stringifier(_) => {
                    let throws = THROWING_STRINGIFIERS.contains(&owner.as_str());
                    let m = self.add_method(
                        &owner,
                        is_mixin,
                        "stringify",
                        MethodSig {
                            name: String::new(),
                            doc: "stringifier;".into(),
                            cx: throws,
                            this: true,
                            params: vec![],
                            ret: if throws {
                                "Result<DomStr<'_>>".into()
                            } else {
                                "DomStr<'_>".into()
                            },
                        },
                    );
                    plan.stringifier = Some(Stringifier::Method(owner.clone(), m, throws));
                    plan.uses.insert(owner.clone());
                }
                Member::Iterable {
                    key: None,
                    is_async: false,
                    ..
                } => plan.value_iterable = true,
                Member::Iterable {
                    key: Some(k),
                    value,
                    is_async: false,
                    ..
                } => {
                    let k = self.prune(k);
                    let v = self.prune(value);
                    if let Some(why) = self.unsupported(&k).or_else(|| self.unsupported(&v)) {
                        self.omit(name, "(iterable)", &why);
                    } else {
                        self.collect(&k);
                        self.collect(&v);
                        let ret = format!(
                            "Option<({}, {})>",
                            self.rust_arg(&k, true),
                            self.rust_arg(&v, true)
                        );
                        let m = self.add_method(
                            name,
                            false,
                            "pair_at",
                            MethodSig {
                                name: String::new(),
                                doc: format!(
                                    "iterable<{}, {}>;\n\nThe value pair to iterate over at 'index' (Web IDL §3.7.10), or `None` past the end.",
                                    k.idl(),
                                    v.idl()
                                ),
                                cx: false,
                                this: true,
                                params: vec![("index".into(), "u32".into())],
                                ret,
                            },
                        );
                        plan.pair_iterable = Some((k, v, m));
                        plan.uses.insert(name.to_string());
                    }
                }
                Member::Iterable {
                    key: None,
                    value,
                    is_async: true,
                    args,
                    ..
                } => {
                    let v = self.prune(value);
                    let why = self.unsupported(&v).or_else(|| {
                        args.iter()
                            .find_map(|a| self.unsupported(&a.ty))
                            .or_else(|| {
                                args.iter()
                                    .any(|a| !a.optional)
                                    .then(|| "a required async iterator argument".to_string())
                            })
                    });
                    if let Some(why) = why {
                        self.omit(name, "(async_iterable)", &why);
                    } else {
                        self.collect(&v);
                        let mut params = Vec::new();
                        for (i, a) in args.iter().enumerate() {
                            self.collect(&a.ty);
                            let t = self.rust_arg(&a.ty, false);
                            let t = if a.default.is_some() || t.starts_with("Option<") {
                                t
                            } else {
                                format!("Option<{t}>")
                            };
                            params.push((format!("arg{i}"), t));
                        }
                        let init = self.add_method(
                            name,
                            false,
                            "async_iterator_init",
                            MethodSig {
                                name: String::new(),
                                doc: format!(
                                    "async_iterable<{}>;\n\nThe asynchronous iterator initialization steps (Web IDL §3.7.10.2): the engine's state for the new iterator.",
                                    v.idl()
                                ),
                                cx: true,
                                this: true,
                                params,
                                ret: "Result<Handle>".into(),
                            },
                        );
                        let next = self.add_method(
                            name,
                            false,
                            "async_iterator_next",
                            MethodSig {
                                name: String::new(),
                                doc: "Get the next iteration result: a promise for the next value, or for `rt::end_of_iteration`.".into(),
                                cx: true,
                                this: true,
                                params: vec![("state".into(), "Handle".into())],
                                ret: "Result<JsPromise>".into(),
                            },
                        );
                        let ret = ASYNC_ITERATOR_RETURN.contains(&name).then(|| {
                            self.add_method(
                                name,
                                false,
                                "async_iterator_return",
                                MethodSig {
                                    name: String::new(),
                                    doc: "The asynchronous iterator return steps: a promise."
                                        .into(),
                                    cx: true,
                                    this: true,
                                    params: vec![
                                        ("state".into(), "Handle".into()),
                                        ("value".into(), "JsValue".into()),
                                    ],
                                    ret: "Result<JsPromise>".into(),
                                },
                            )
                        });
                        plan.async_iterable = Some(AsyncIterPlan {
                            value: v,
                            args: args.clone(),
                            init,
                            next,
                            ret,
                        });
                        plan.uses.insert(name.to_string());
                    }
                }
                Member::Iterable { .. } => self.omit(
                    name,
                    "(iterable)",
                    "pair async iterators are not supported yet",
                ),
                Member::Maplike { .. } => {
                    self.omit(name, "(maplike)", "maplike is not supported yet")
                }
                Member::Setlike {
                    value, readonly, ..
                } => {
                    let v = self.prune(value);
                    let is_string = matches!(&v.kind, TypeKind::Prim(p) if p.is_string());
                    let is_interface = matches!(&v.kind, TypeKind::Named(n)
                        if self.model.kind_of(n) == NamedKind::Interface);
                    if !is_string && !is_interface {
                        self.omit(
                            name,
                            "(setlike)",
                            "setlike of a type that is neither a string nor an interface is not supported yet",
                        );
                    } else {
                        let sig = |doc: &str,
                                   cx: bool,
                                   params: Vec<(String, String)>,
                                   ret: &str| MethodSig {
                            name: String::new(),
                            doc: doc.to_string(),
                            cx,
                            this: true,
                            params,
                            ret: ret.to_string(),
                        };
                        let at = self.add_method(
                            name,
                            false,
                            "setlike_at",
                            sig("setlike: the value at 'index' in insertion order, or `None` past the end.", false, vec![("index".into(), "u32".into())], if is_interface { "Option<Handle>" } else { "Option<String>" }),
                        );
                        let size = self.add_method(
                            name,
                            false,
                            "setlike_size",
                            sig("setlike: `size`.", false, vec![], "u32"),
                        );
                        let has = self.add_method(
                            name,
                            false,
                            "setlike_has",
                            sig(
                                "setlike: `has(value)`.",
                                false,
                                vec![(
                                    "value".into(),
                                    if is_interface { "Handle" } else { "&str" }.into(),
                                )],
                                "bool",
                            ),
                        );
                        // (an interface's setlike gets no generated mutators: the interface declares
                        // `add`, `delete` and `clear` itself)
                        let (add, delete, clear) = if *readonly || is_interface {
                            (None, None, None)
                        } else {
                            (
                                Some(self.add_method(
                                    name,
                                    false,
                                    "setlike_add",
                                    sig(
                                        "setlike: `add(value)`.",
                                        true,
                                        vec![("value".into(), "&str".into())],
                                        "Result<()>",
                                    ),
                                )),
                                Some(self.add_method(
                                    name,
                                    false,
                                    "setlike_delete",
                                    sig(
                                        "setlike: `delete(value)`: whether it was there.",
                                        true,
                                        vec![("value".into(), "&str".into())],
                                        "Result<bool>",
                                    ),
                                )),
                                Some(self.add_method(
                                    name,
                                    false,
                                    "setlike_clear",
                                    sig("setlike: `clear()`.", true, vec![], "Result<()>"),
                                )),
                            )
                        };
                        plan.setlike = Some(SetlikePlan {
                            value: v,
                            readonly: *readonly || is_interface,
                            at,
                            size,
                            has,
                            add,
                            delete,
                            clear,
                        });
                        plan.uses.insert(name.to_string());
                    }
                }
                _ => {}
            }
        }
        if plan.value_iterable && plan.indexed_getter.is_none() {
            self.omit(
                name,
                "(iterable)",
                "value iterator without an indexed getter",
            );
            plan.value_iterable = false;
        }

        for (owner, op_name, is_static, members) in op_groups {
            if op_name == "toJSON"
                && !is_static
                && members.iter().any(|m| has_ext(m.member.ext(), "Default"))
            {
                plan.default_json = Some(self.default_json_attributes(name));
                continue;
            }
            let is_mixin = owner != name;
            // A namespace's operations have no `this`, like static operations.
            let is_static = is_static || namespace;
            if let Some(mut op) =
                self.plan_operation(name, &owner, is_mixin, &op_name, is_static, &members)
            {
                let mut exposed = BTreeSet::new();
                for m in &members {
                    exposed.extend(m.exposed.iter().cloned());
                }
                op.exposure = narrower_exposure(&iface.exposed, &exposed);
                plan.uses.insert(owner.clone());
                if op.overloads.iter().any(|o| o.args.is_empty())
                    && op_name == "toString"
                    && plan.stringifier.is_some()
                {
                    continue;
                }
                plan.ops.push(op);
            }
        }

        if has_ext(&iface.ext, "HTMLConstructor")
            || ctors
                .iter()
                .any(|c| has_ext(c.member.ext(), "HTMLConstructor"))
        {
            plan.html_constructor = true;
        } else if !ctors.is_empty() {
            plan.ctor = self.plan_operation(name, name, false, "constructor", true, &ctors);
        }

        // [LegacyFactoryFunction=Image(unsigned long width, unsigned long height)]
        for x in iface
            .ext
            .iter()
            .filter(|x| x.name == "LegacyFactoryFunction")
        {
            let (fname, args) = match &x.value {
                ExtValue::NamedArgs(n, a) => (n.clone(), a.clone()),
                ExtValue::Token(n) => (n.clone(), Vec::new()),
                _ => continue,
            };
            let fake = IMember {
                member: Member::Constructor(args, Vec::new()),
                mixin: None,
                exposed: iface.exposed.clone(),
                container_ext: Vec::new(),
            };
            if let Some(mut op) = self.plan_operation(
                name,
                name,
                false,
                &format!("factory{fname}"),
                true,
                &[&fake],
            ) {
                op.name = fname;
                plan.factories.push(op);
            }
        }
        plan
    }

    /// Web IDL "collect attribute values of an inheritance stack" as far as it is static: for each
    /// interface of `name`'s inheritance stack (the root first) that declares a `[Default]` toJSON
    /// operation, its regular attributes of a JSON type, in member order.
    fn default_json_attributes(&self, name: &str) -> Vec<String> {
        let mut stack = vec![name.to_string()];
        while let Some(parent) = self.model.interfaces[stack.last().unwrap()]
            .inherits
            .clone()
        {
            stack.push(parent);
        }
        let mut out: Vec<String> = Vec::new();
        for i in stack.iter().rev() {
            let iface = &self.model.interfaces[i];
            let has_default = iface.members.iter().any(|im| {
                matches!(&im.member, Member::Operation(o) if o.name.as_deref() == Some("toJSON") && !o.is_static)
                    && has_ext(im.member.ext(), "Default")
            });
            if !has_default {
                continue;
            }
            for im in &iface.members {
                let Member::Attribute(a) = &im.member else {
                    continue;
                };
                if a.is_static || !exposed_any(&im.exposed) {
                    continue;
                }
                if self.unsupported(&a.ty).is_some() || !self.is_json_type(&a.ty) {
                    continue;
                }
                if !out.contains(&a.name) {
                    out.push(a.name.clone());
                }
            }
        }
        out
    }

    /// Web IDL §2.13.32 "JSON types".
    fn is_json_type(&self, ty: &Type) -> bool {
        let ty = self.model.resolve(ty);
        match &ty.kind {
            TypeKind::Prim(p) => !matches!(p, Prim::Any | Prim::Undefined | Prim::Symbol),
            TypeKind::Named(n) => match self.model.kind_of(n) {
                NamedKind::Enum => true,
                NamedKind::Dictionary => self
                    .model
                    .dictionary_chain(n)
                    .iter()
                    .all(|d| d.members.iter().all(|m| self.is_json_type(&m.ty))),
                // An interface type with a toJSON operation declared on itself or an ancestor.
                NamedKind::Interface => {
                    let mut cur = Some(n.clone());
                    while let Some(c) = cur {
                        let Some(i) = self.model.interfaces.get(&c) else {
                            return false;
                        };
                        if i.members.iter().any(|im| {
                            matches!(&im.member, Member::Operation(o) if o.name.as_deref() == Some("toJSON") && !o.is_static)
                        }) {
                            return true;
                        }
                        cur = i.inherits.clone();
                    }
                    false
                }
                _ => false,
            },
            TypeKind::Sequence(t) | TypeKind::FrozenArray(t) => self.is_json_type(t),
            TypeKind::Record(_, t) => self.is_json_type(t),
            TypeKind::Union(ts) => ts.iter().all(|t| self.is_json_type(t)),
            _ => false,
        }
    }

    fn plan_attribute(
        &mut self,
        iface: &str,
        owner: &str,
        is_mixin: bool,
        im: &IMember,
        a: &Attribute,
        enumerated: &HashMap<(String, String), Enumerated>,
    ) -> Option<AttrPlan> {
        let ty = self.prune(&a.ty);
        // CSSOM §6.6.1: the attributes of every supported CSS property (synthesized by the model
        // loader from `data/css-properties.txt`) all route to one getter and one setter by property.
        // So do those of CSS Fonts 5 `CSSFontFaceDescriptors`, by the descriptor each stands for: its
        // name with a dash before each upper-case letter, lower-cased (CSSOM "IDL attribute to CSS
        // property").
        let descriptor = (iface == "CSSFontFaceDescriptors").then(|| {
            let mut out = String::new();
            for c in a.name.chars() {
                if c.is_ascii_uppercase() {
                    out.push('-');
                    out.push(c.to_ascii_lowercase());
                } else {
                    out.push(c);
                }
            }
            out
        });
        if let Some(property) = ext(&a.ext, "SnowlarkCssProperty")
            .and_then(|x| x.token())
            .map(str::to_string)
            .or(descriptor)
        {
            if !self
                .trait_for(owner, is_mixin)
                .methods
                .iter()
                .any(|m| m.name == "css_property")
            {
                let ret = self.rust_ret(&ty);
                self.add_method(
                    owner,
                    is_mixin,
                    "css_property",
                    MethodSig {
                        name: String::new(),
                        doc: "/// The getter of every CSS property attribute (CSSOM §6.6.1 `getPropertyValue(property)`).".into(),
                        cx: false,
                        this: true,
                        params: vec![("property".into(), "&'static str".into())],
                        ret,
                    },
                );
                let arg = self.rust_arg(&ty, false);
                self.add_method(
                    owner,
                    is_mixin,
                    "set_css_property",
                    MethodSig {
                        name: String::new(),
                        doc: "/// The setter of every CSS property attribute (CSSOM §6.6.1 `setProperty(property, value)`).".into(),
                        cx: true,
                        this: true,
                        params: vec![("property".into(), "&'static str".into()), ("value".into(), arg)],
                        ret: "Result<()>".into(),
                    },
                );
            }
            return Some(AttrPlan {
                name: a.name.clone(),
                ty,
                owner: owner.to_string(),
                is_static: false,
                getter: Getter::CssProperty(property.clone()),
                setter: Setter::CssProperty(property),
                getter_fallible: false,
                fast: None,
                unforgeable: false,
                cached: false,
                same_object: false,
                ce_reactions: true,
                secure: false,
                exposure: None,
                ua_gate: None,
                touch_device: false,
                stringifier: false,
                lenient_this: false,
                idl: idl_of(&im.member),
            });
        }
        if a.readonly
            && !a.is_static
            && owner.starts_with("SVG")
            && matches!(&ty.kind, TypeKind::Named(n) if SVG_REFLECTED.contains(&n.as_str()))
        {
            self.collect(&ty);
            // (the owner's file holds the accessor callbacks)
            self.trait_for(owner, is_mixin);
            let base = self.trait_for("SVGElement", false);
            if !base.methods.iter().any(|m| m.name == "svg_reflect") {
                self.add_method(
                    "SVGElement",
                    false,
                    "svg_reflect",
                    MethodSig {
                        name: String::new(),
                        doc: "/// The object of every SVG DOM attribute that reflects a content attribute through an
    /// animated value or list object (SVG 2 §4.3.4), by the attribute's owner and name.".into(),
                        cx: false,
                        this: true,
                        params: vec![
                            ("owner".into(), "&'static str".into()),
                            ("member".into(), "&'static str".into()),
                        ],
                        ret: "Handle".into(),
                    },
                );
            }
            return Some(AttrPlan {
                name: a.name.clone(),
                ty,
                owner: owner.to_string(),
                is_static: false,
                getter: Getter::SvgReflect,
                setter: Setter::None,
                getter_fallible: false,
                fast: None,
                unforgeable: false,
                cached: false,
                same_object: true,
                ce_reactions: false,
                secure: im.secure_context(),
                exposure: None,
                ua_gate: None,
                touch_device: false,
                stringifier: false,
                lenient_this: false,
                idl: idl_of(&im.member),
            });
        }
        if matches!(&a.ty.kind, TypeKind::Named(n) if EVENT_HANDLER_TYPES.contains(&n.as_str())) {
            let event = a.name.strip_prefix("on").unwrap_or(&a.name).to_string();
            // The owner's file holds the accessor callbacks, also when no trait method is planned.
            self.trait_for(owner, is_mixin);
            return Some(AttrPlan {
                name: a.name.clone(),
                ty,
                owner: owner.to_string(),
                is_static: false,
                getter: Getter::EventHandler(event.clone()),
                setter: Setter::EventHandler(event),
                getter_fallible: true,
                fast: None,
                unforgeable: false,
                cached: false,
                same_object: false,
                ce_reactions: false,
                secure: im.secure_context(),
                exposure: None,
                ua_gate: None,
                touch_device: false,
                stringifier: false,
                lenient_this: has_ext(&a.ext, "LegacyLenientThis"),
                idl: idl_of(&im.member),
            });
        }
        if let TypeKind::ObservableArray(element) = &ty.kind {
            let element = self.prune(element);
            let interface = matches!(&element.kind, TypeKind::Named(n)
                if self.model.kind_of(n) == NamedKind::Interface)
                && !element.nullable;
            if !interface {
                self.omit(
                    iface,
                    &a.name,
                    "ObservableArray of a type other than an interface is not supported yet",
                );
                return None;
            }
            return Some(self.plan_observable_array(owner, is_mixin, im, a, ty.clone(), element));
        }
        if let Some(why) = self.unsupported(&ty) {
            self.omit(iface, &a.name, &why);
            return None;
        }
        // A mixin member is planned once; later inclusions reuse its trait method names.
        let already = is_mixin && self.mixin_planned(owner, &format!("get:{}", a.name));
        self.collect(&ty);
        let doc = idl_of(&im.member);
        let reflect_owner = if is_mixin { iface } else { owner };
        let reflect = self.reflect_rule(reflect_owner, a, enumerated);
        let put_forwards = ext(&a.ext, "PutForwards")
            .and_then(|x| x.token())
            .map(str::to_string);
        let replaceable = has_ext(&a.ext, "Replaceable");
        let fallible = THROWING_GETTERS.contains(&(iface, a.name.as_str())) || self.needs_cx(&ty);
        let snake_name = snake(&a.name);
        let base = snake_name.trim_end_matches('_');
        let (getter, setter) = match &reflect {
            Some((rule, false)) => (
                Getter::Reflect(rule.clone()),
                if a.readonly && put_forwards.is_none() {
                    Setter::None
                } else if let Some(f) = &put_forwards {
                    Setter::PutForwards(f.clone())
                } else {
                    Setter::Reflect(rule.clone())
                },
            ),
            _ => {
                let gname = if already {
                    self.mixin_method(owner, &format!("get:{}", a.name))
                } else {
                    let m = self.add_method(
                        owner,
                        is_mixin,
                        &if a.is_static {
                            format!("static_{base}")
                        } else {
                            snake_name.clone()
                        },
                        MethodSig {
                            name: String::new(),
                            doc: doc.clone(),
                            cx: fallible,
                            this: !a.is_static,
                            params: vec![],
                            ret: if fallible {
                                format!("Result<{}>", self.rust_ret(&ty))
                            } else {
                                self.rust_ret(&ty)
                            },
                        },
                    );
                    self.remember_mixin(owner, is_mixin, &format!("get:{}", a.name), &m);
                    m
                };
                let setter = if let Some((rule, true)) = &reflect {
                    Setter::Reflect(rule.clone())
                } else if let Some(f) = &put_forwards {
                    Setter::PutForwards(f.clone())
                } else if replaceable {
                    Setter::Replaceable
                } else if a.readonly {
                    Setter::None
                } else {
                    let sname = if already {
                        self.mixin_method(owner, &format!("set:{}", a.name))
                    } else {
                        let m = self.add_method(
                            owner,
                            is_mixin,
                            &format!("set_{base}"),
                            MethodSig {
                                name: String::new(),
                                doc: doc.clone(),
                                cx: true,
                                this: !a.is_static,
                                params: vec![("value".into(), self.rust_arg(&ty, false))],
                                ret: "Result<()>".into(),
                            },
                        );
                        self.remember_mixin(owner, is_mixin, &format!("set:{}", a.name), &m);
                        m
                    };
                    Setter::Trait(sname)
                };
                (Getter::Trait(gname), setter)
            }
        };
        let fast = match &getter {
            Getter::Trait(_) if !a.is_static && !fallible => self.fast_ret(&ty),
            _ => None,
        };
        Some(AttrPlan {
            name: a.name.clone(),
            ty,
            owner: owner.to_string(),
            is_static: a.is_static,
            getter,
            setter,
            getter_fallible: fallible,
            fast,
            unforgeable: has_ext(&a.ext, "LegacyUnforgeable"),
            cached: has_ext(&a.ext, "CachedAccessor"),
            same_object: has_ext(&a.ext, "SameObject"),
            ce_reactions: has_ext(&a.ext, "CEReactions"),
            secure: im.secure_context(),
            exposure: None,
            ua_gate: None,
            touch_device: false,
            stringifier: a.stringifier,
            lenient_this: has_ext(&a.ext, "LegacyLenientThis"),
            idl: doc,
        })
    }

    /// An `ObservableArray<T>` attribute of interface type `T` (Web IDL §3.10).
    fn plan_observable_array(
        &mut self,
        owner: &str,
        is_mixin: bool,
        im: &IMember,
        a: &Attribute,
        ty: Type,
        element: Type,
    ) -> AttrPlan {
        self.collect(&element);
        let snake_name = snake(&a.name);
        let base = snake_name.trim_end_matches('_').to_string();
        let ret = self.rust_ret(&element);
        let arg = self.rust_arg(&element, false);
        let doc = idl_of(&im.member);
        let sig = |cx: bool, params: Vec<(String, String)>, ret: String| MethodSig {
            name: String::new(),
            doc: String::new(),
            cx,
            this: true,
            params,
            ret,
        };
        let methods: [(&str, &str, MethodSig); 5] = [
            ("length", "The size of the backing list.", sig(false, vec![], "u32".into())),
            (
                "item",
                "The backing list's item at `index` (below its size).",
                sig(false, vec![("index".into(), "u32".into())], ret),
            ),
            (
                "set_indexed_value",
                "The attribute's \"set an indexed value\" steps for `value` at `index`, before it is stored.",
                sig(
                    true,
                    vec![("value".into(), arg.clone()), ("index".into(), "u32".into())],
                    "Result<()>".into(),
                ),
            ),
            (
                "delete_indexed_value",
                "The attribute's \"delete an indexed value\" steps for `value` at `index`, before it is removed.",
                sig(
                    true,
                    vec![("value".into(), arg.clone()), ("index".into(), "u32".into())],
                    "Result<()>".into(),
                ),
            ),
            (
                "store",
                "Writes the backing list: `value` at `index` (appended at its size), or with `None` the last item removed.",
                sig(
                    false,
                    vec![
                        ("index".into(), "u32".into()),
                        ("value".into(), format!("Option<{arg}>")),
                    ],
                    "()".into(),
                ),
            ),
        ];
        let mut names = Vec::new();
        for (suffix, what, mut sig) in methods {
            let key = format!("observable-{suffix}:{}", a.name);
            let name = if is_mixin && self.mixin_planned(owner, &key) {
                self.mixin_method(owner, &key)
            } else {
                sig.doc = format!("{doc}\n{what} (Web IDL §3.10 observable array)");
                let m = self.add_method(owner, is_mixin, &format!("{base}_{suffix}"), sig);
                self.remember_mixin(owner, is_mixin, &key, &m);
                m
            };
            names.push(name);
        }
        let plan = ObservablePlan {
            element,
            length: names[0].clone(),
            item: names[1].clone(),
            set_indexed_value: names[2].clone(),
            delete_indexed_value: names[3].clone(),
            store: names[4].clone(),
        };
        AttrPlan {
            name: a.name.clone(),
            ty,
            owner: owner.to_string(),
            is_static: false,
            getter: Getter::ObservableArray(plan),
            setter: if a.readonly {
                Setter::None
            } else {
                Setter::ObservableArray
            },
            getter_fallible: false,
            fast: None,
            unforgeable: has_ext(&a.ext, "LegacyUnforgeable"),
            cached: false,
            // (the getter returns the same object every time: Web IDL §3.7.6)
            same_object: true,
            ce_reactions: has_ext(&a.ext, "CEReactions"),
            exposure: None,
            ua_gate: None,
            touch_device: false,
            secure: im.secure_context(),
            stringifier: false,
            lenient_this: false,
            idl: doc,
        }
    }

    fn mixin_planned(&self, owner: &str, key: &str) -> bool {
        self.traits
            .get(owner)
            .is_some_and(|t| t.keyed.contains_key(key))
    }

    fn mixin_method(&self, owner: &str, key: &str) -> String {
        self.traits[owner]
            .keyed
            .get(key)
            .cloned()
            .unwrap_or_else(|| panic!("mixin method {owner} {key} not planned"))
    }

    fn remember_mixin(&mut self, owner: &str, is_mixin: bool, key: &str, method: &str) {
        if is_mixin {
            let t = self.traits.get_mut(owner).unwrap();
            t.keyed.insert(key.to_string(), method.to_string());
        }
    }

    fn plan_operation(
        &mut self,
        iface: &str,
        owner: &str,
        is_mixin: bool,
        op_name: &str,
        is_static: bool,
        members: &[&IMember],
    ) -> Option<OpPlan> {
        let is_ctor = op_name == "constructor" || op_name.starts_with("factory");
        let mut overloads = Vec::new();
        let already = is_mixin && self.mixin_planned(owner, &format!("op:{op_name}#0"));
        let mut kept = Vec::new();
        for im in members {
            let (args, ret, ext): (Vec<Argument>, Type, &[ExtAttr]) = match &im.member {
                Member::Operation(o) => (o.args.clone(), o.ret.clone(), &o.ext),
                Member::Constructor(a, e) => (a.clone(), Type::prim(Prim::Undefined), e),
                _ => unreachable!(),
            };
            let args: Vec<Argument> = args
                .into_iter()
                .map(|mut a| {
                    a.ty = self.prune(&a.ty);
                    a
                })
                .collect();
            let ret = self.prune(&ret);
            let mut why = None;
            for t in std::iter::once(&ret).chain(args.iter().map(|a| &a.ty)) {
                if let Some(w) = self.unsupported(t) {
                    why = Some(w);
                    break;
                }
            }
            if let Some(w) = why {
                self.omit(
                    iface,
                    &format!(
                        "{op_name}({})",
                        args.iter()
                            .map(|a| a.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                    &w,
                );
                continue;
            }
            kept.push((args, ret, ext.to_vec(), idl_of(&im.member)));
        }
        if kept.is_empty() {
            return None;
        }
        let promise = kept
            .iter()
            .any(|(_, r, _, _)| matches!(r.kind, TypeKind::Promise(_)));
        let ce = kept.iter().any(|(_, _, e, _)| has_ext(e, "CEReactions"));
        let unforgeable = kept
            .iter()
            .any(|(_, _, e, _)| has_ext(e, "LegacyUnforgeable"));
        let secure = members.iter().any(|m| m.secure_context());
        let multiple = kept.len() > 1;
        for (i, (args, ret, _, doc)) in kept.into_iter().enumerate() {
            for a in &args {
                self.collect(&a.ty);
            }
            self.collect(&ret);
            let in_namespace = self
                .model
                .interfaces
                .get(iface)
                .is_some_and(|i| i.kind == InterfaceKind::Namespace);
            let base = if is_ctor || in_namespace {
                snake(op_name)
            } else if is_static {
                format!("static_{}", snake(op_name).trim_end_matches('_'))
            } else {
                snake(op_name)
            };
            let base = if multiple {
                let suffix: Vec<String> = args
                    .iter()
                    .map(|a| snake(&a.name).trim_end_matches('_').to_string())
                    .collect();
                if suffix.is_empty() {
                    format!("{}_0", base.trim_end_matches('_'))
                } else {
                    format!("{}_{}", base.trim_end_matches('_'), suffix.join("_"))
                }
            } else {
                base
            };
            let key = format!("op:{op_name}#{i}");
            let method = if already {
                self.mixin_method(owner, &key)
            } else {
                let params = args
                    .iter()
                    .map(|a| {
                        let pname = snake(&a.name);
                        let t = if a.variadic {
                            format!("Vec<{}>", self.rust_arg(&a.ty, true))
                        } else if a.optional && a.default.is_none() {
                            let inner = self.rust_arg(&a.ty, false);
                            if inner.starts_with("Option<") {
                                inner
                            } else {
                                format!("Option<{inner}>")
                            }
                        } else {
                            self.rust_arg(&a.ty, false)
                        };
                        (pname, t)
                    })
                    .collect();
                let ret_rust = if is_ctor {
                    "Result<Handle>".to_string()
                } else {
                    format!("Result<{}>", self.rust_ret(&ret))
                };
                let m = self.add_method(
                    owner,
                    is_mixin,
                    &base,
                    MethodSig {
                        name: String::new(),
                        doc: doc.clone(),
                        cx: true,
                        this: !is_static,
                        params,
                        ret: ret_rust,
                    },
                );
                self.remember_mixin(owner, is_mixin, &key, &m);
                m
            };
            overloads.push(OverloadPlan {
                args,
                ret,
                method,
                idl: doc,
            });
        }
        Some(OpPlan {
            name: op_name.to_string(),
            owner: owner.to_string(),
            is_static,
            overloads,
            ce_reactions: ce,
            unforgeable,
            secure,
            exposure: None,
            promise,
        })
    }

    fn plan_special(
        &mut self,
        iface: &str,
        owner: &str,
        is_mixin: bool,
        o: &Operation,
        plan: &mut IfacePlan,
    ) {
        let special = o.special.unwrap();
        let args: Vec<Type> = o.args.iter().map(|a| self.prune(&a.ty)).collect();
        let ret = self.prune(&o.ret);
        for t in args.iter().chain(std::iter::once(&ret)) {
            if let Some(w) = self.unsupported(t) {
                self.omit(iface, "(special operation)", &w);
                return;
            }
            self.collect(t);
        }
        let indexed = args[0].as_prim() == Some(Prim::UnsignedLong);
        let kind = match (special, indexed) {
            (Special::Getter, true) => "indexed_getter",
            (Special::Getter, false) => "named_getter",
            (Special::Setter, true) => "indexed_setter",
            (Special::Setter, false) => "named_setter",
            (Special::Deleter, _) => "named_deleter",
        };
        let key = format!("special:{kind}");
        let already = is_mixin && self.mixin_planned(owner, &key);
        let method = if already {
            self.mixin_method(owner, &key)
        } else {
            let base = o
                .name
                .as_ref()
                .map(|n| snake(n))
                .unwrap_or_else(|| kind.to_string());
            let key_param = if indexed {
                ("index".to_string(), "u32".to_string())
            } else {
                ("name".to_string(), "&str".to_string())
            };
            let value_ty = Type {
                nullable: false,
                ..ret.clone()
            };
            let sig = match special {
                Special::Getter => MethodSig {
                    name: String::new(),
                    doc: format!(
                        "{}\n\nReturns `None` when the {} is not a supported property {}.",
                        idl_of(&Member::Operation(o.clone())),
                        if indexed { "index" } else { "name" },
                        if indexed { "index" } else { "name" }
                    ),
                    cx: false,
                    this: true,
                    params: vec![key_param],
                    // An `object` getter returns a platform object (Window's and Document's named
                    // properties): the engine names it by its handle.
                    ret: if value_ty.as_prim() == Some(Prim::Object) {
                        "Option<Handle>".into()
                    } else {
                        format!("Option<{}>", self.rust_ret(&value_ty))
                    },
                },
                Special::Setter => MethodSig {
                    name: String::new(),
                    doc: idl_of(&Member::Operation(o.clone())),
                    cx: true,
                    this: true,
                    params: vec![key_param, ("value".into(), self.rust_arg(&args[1], false))],
                    ret: "Result<()>".into(),
                },
                Special::Deleter => MethodSig {
                    name: String::new(),
                    doc: idl_of(&Member::Operation(o.clone())),
                    cx: true,
                    this: true,
                    params: vec![key_param],
                    ret: "Result<()>".into(),
                },
            };
            let m = self.add_method(owner, is_mixin, &base, sig);
            if !indexed && special == Special::Getter {
                self.add_method(
                    owner,
                    is_mixin,
                    "supported_property_names",
                    MethodSig {
                        name: String::new(),
                        doc: "The supported property names, in order (for enumeration).".into(),
                        cx: false,
                        this: true,
                        params: vec![],
                        ret: "Vec<String>".into(),
                    },
                );
            }
            self.remember_mixin(owner, is_mixin, &key, &m);
            m
        };
        let sp = SpecialPlan {
            method: method.clone(),
            owner: owner.to_string(),
            key: args[0].clone(),
            value: if special == Special::Setter {
                args[1].clone()
            } else {
                ret.clone()
            },
            ce_reactions: has_ext(&o.ext, "CEReactions"),
        };
        // A named getter that is also a regular operation (`item`, `namedItem`) is callable too.
        if let Some(n) = &o.name {
            if special == Special::Getter {
                plan.ops.push(OpPlan {
                    name: n.clone(),
                    owner: owner.to_string(),
                    is_static: false,
                    overloads: vec![OverloadPlan {
                        args: o
                            .args
                            .iter()
                            .map(|a| {
                                let mut a = a.clone();
                                a.ty = self.prune(&a.ty);
                                a
                            })
                            .collect(),
                        ret: ret.clone(),
                        method: format!("#getter:{method}"),
                        idl: idl_of(&Member::Operation(o.clone())),
                    }],
                    ce_reactions: false,
                    unforgeable: false,
                    secure: false,
                    exposure: None,
                    promise: false,
                });
            } else {
                plan.ops.push(OpPlan {
                    name: n.clone(),
                    owner: owner.to_string(),
                    is_static: false,
                    overloads: vec![OverloadPlan {
                        args: o
                            .args
                            .iter()
                            .map(|a| {
                                let mut a = a.clone();
                                a.ty = self.prune(&a.ty);
                                a
                            })
                            .collect(),
                        ret: ret.clone(),
                        method: method.clone(),
                        idl: idl_of(&Member::Operation(o.clone())),
                    }],
                    ce_reactions: has_ext(&o.ext, "CEReactions"),
                    unforgeable: false,
                    secure: false,
                    exposure: None,
                    promise: false,
                });
            }
        }
        let slot = match kind {
            "indexed_getter" => &mut plan.indexed_getter,
            "named_getter" => &mut plan.named_getter,
            "indexed_setter" => &mut plan.indexed_setter,
            "named_setter" => &mut plan.named_setter,
            _ => &mut plan.named_deleter,
        };
        *slot = Some(sp);
    }
}

/// `console` → `Console`, for trait names of namespaces.
fn upper_first(name: &str) -> String {
    let mut c = name.chars();
    match c.next() {
        Some(f) => f.to_ascii_uppercase().to_string() + c.as_str(),
        None => String::new(),
    }
}

pub fn prim_rust(p: Prim) -> &'static str {
    match p {
        Prim::Boolean => "bool",
        Prim::Byte => "i8",
        Prim::Octet => "u8",
        Prim::Short => "i16",
        Prim::UnsignedShort => "u16",
        Prim::Long => "i32",
        Prim::UnsignedLong => "u32",
        Prim::LongLong => "i64",
        Prim::UnsignedLongLong => "u64",
        Prim::Float | Prim::UnrestrictedFloat => "f32",
        Prim::Double | Prim::UnrestrictedDouble => "f64",
        Prim::Undefined => "()",
        _ => "JsValue",
    }
}
