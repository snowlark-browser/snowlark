# Service Workers: a staged plan

Status: **planned, not started** (2026-11-13). Specs: W3C Service Workers
(Nightly, the "Service Worker" and "Cache" parts), Fetch ("handle fetch" hook, request `service-workers mode`),
HTML (environment settings objects, reserved clients, `execution ready`), Storage (storage keys).

## Why

`navigator.serviceWorker` is undefined in Snowlark. Sites test for it and log or branch (a dashboard
logs "Service workers are not supported in this browser"; `tools/census/report.md` ranks
`Navigator.prototype.serviceWorker` 18th by corpus use, 10 of 25 sites). Offline-capable apps, push, and many
PWAs need it. `caches` (the Cache API) is also missing and is used from windows as well as from workers.

## What exists today

- `idl/service-workers.idl` has every interface (`ServiceWorkerContainer`, `ServiceWorkerRegistration`,
  `ServiceWorker`, `ServiceWorkerGlobalScope`, `Clients`, `Client`, `WindowClient`, `ExtendableEvent`,
  `InstallEvent`, `FetchEvent`, `ExtendableMessageEvent`, `NavigationPreloadManager`, `Cache`,
  `CacheStorage`). None is in the generator's list (`crates/snowlark-idl/src/codegen/plan.rs`), so nothing is
  bound.
- Dedicated workers (`crates/snowlark-page/src/worker.rs`): a thread per worker with its own isolate, engine and
  event loop, structured-serialization messages. The worker global scope machinery is reused.
- `snowlark-net` already knows `Destination::ServiceWorker` (CSP `worker-src`), the HTTP cache, and navigation
  that begins in the browser (`snowlark-net/src/navigation.rs`, process isolation M5).
- Secure contexts (`isSecureContext`, `snowlark-net/src/policy.rs`): every interface here is `[SecureContext]`.
- Storage services behind traits in the browser half: Web Storage (`WebStorageHost`), IndexedDB (`IdbService`),
  broadcast. Those are the pattern to follow.

## Where each part lives (`docs/process-isolation.md`)

A service worker outlives the tab that registered it, serves every tab of its storage key, and sits in front of
their fetches. So, as section 14 of that plan already ruled:

| Part | Lives in | Notes |
|---|---|---|
| Registration map, job queues, script and imported-script storage, version state | **Browser half**, new crate `snowlark-sw`, keyed by storage key, persisted in the profile | Never an `Arc` hub in `TabContext`; renderers reach it through a message-shaped trait (`ServiceWorkerHost`) |
| Cache API storage (`caches`) | **Browser half**, `snowlark-sw` (or its own module there), keyed by storage key | Renderers reach it through a `CacheStorageBackend` trait; bodies stream as `ResponseStream`s |
| Running a worker (`ServiceWorkerGlobalScope`) | A worker thread in a **renderer locked to the worker's site** | The browser picks the process: an existing renderer of that site, else a new one (or the spare). With `--process-model=single`, a thread of the browser's process |
| `navigator.serviceWorker`, `ServiceWorkerRegistration`, `ServiceWorker` objects | The client's renderer (page crates), thin objects over browser state | State changes arrive as messages (`updatefound`, `statechange`, `controllerchange`) |
| Navigation interception | **Browser half**, in the navigation path before the network | It decides which registration controls the new client |
| Subresource interception | The **controlled client's net backend**: requests go over a pipe to the controller worker, falling back to the network | The browser hands the client that pipe at commit and on `controllerchange`; it does not proxy every request |
| Security decisions (scope checks, `Service-Worker-Allowed`, script MIME, same-origin script URL, storage key) | `snowlark-sw` / `snowlark-net`, never the page crates | |

Headless modes (`--wpt`, `--eval`, `--render`) run the browser half in process (a local implementation, as
`LocalIdb` does), so WPT exercises the same code.

## Stages

Each stage merges on its own with WPT counts before and after (`--no-reftests`, and `--https`: see stage 0).
Sizes: S under a day, M about a day, L several days.

### Stage 0: harness (S)

- WPT: almost every test in `service-workers/` is a `.https.` file. Today's runner lists them only with
  `--https`, and skips `.serviceworker.` variants of `.any.js` tests (`run.py` `tests_in`). Make the area's
  before/after runs `--https`, and once stage 2 lands, generate the `.serviceworker.html` variants (the test
  runs inside a registered service worker: `fetch/`, `streams/`, `encoding/`, `IndexedDB/`, `url/` ... all
  gain coverage).
- A gate baseline that includes the `--https` service-workers run, so later stages are gated on it.

### Stage 1: the Cache API (M to L)

`caches` on `Window` and `WorkerGlobalScope` (secure contexts), `CacheStorage` (`open`, `has`, `delete`,
`keys`, `match`) and `Cache` (`match`, `matchAll`, `add`, `addAll`, `put`, `delete`, `keys`), with the spec's
request-response list, `Vary` matching, `ignoreSearch`/`ignoreMethod`/`ignoreVary`, the batch cache operations
algorithm (all-or-nothing), opaque responses kept opaque, and storage per storage key on disk in the profile.
Independent of service workers: pages use it directly, and stage 2 needs it for the tests' setup.
WPT: `service-workers/cache-storage/` (window and dedicated worker scopes now; the service worker scope after
stage 2).

### Stage 2: registration, lifecycle, the worker global scope (L)

- `navigator.serviceWorker` (`ServiceWorkerContainer`: `register`, `getRegistration(s)`, `ready`, `controller`,
  `startMessages`, `oncontrollerchange`, `onmessage`), `ServiceWorkerRegistration` (`installing`, `waiting`,
  `active`, `scope`, `updateViaCache`, `update()`, `unregister()`, `onupdatefound`), `ServiceWorker` (`state`,
  `scriptURL`, `postMessage`, `onstatechange`), and the same objects inside workers (`WorkerNavigator`).
- The job queue per scope (Register, Update, Unregister jobs; the spec's "Schedule Job", "Run Job",
  "Finish Job"), the Update algorithm (fetch with the `Service-Worker: script` header and the script MIME check,
  `Service-Worker-Allowed`, byte comparison including imported scripts, `updateViaCache`), Install and Activate
  with `ExtendableEvent.waitUntil`, `skipWaiting()`, the waiting worker's promotion when no client uses the
  active one, `Clients.claim()`.
- `ServiceWorkerGlobalScope` on a worker thread (reusing `worker.rs`): `clients`, `registration`,
  `serviceWorker`, `skipWaiting`, `importScripts` (only during install, from the stored script map afterwards),
  the event queue, and termination when idle (the spec leaves the timing to the user agent: 30 s idle and a
  5 min cap for one event). Classic scripts first; module scripts (`type: 'module'`) after.
- The registry persists in the profile and survives restart; "Clear site data" clears it.
- Ends the console message. WPT: `service-workers/service-worker/` registration, update, unregister, lifecycle
  and `ServiceWorkerGlobalScope` files.

### Stage 3: fetch interception for navigations (L)

In the browser's navigation path (`snowlark-net/src/navigation.rs` and the shell's navigation code): "Handle
Fetch" for a non-subresource request. Match the longest scope for the URL within the storage key, start the
active worker if it is not running, dispatch `FetchEvent` (`request`, `clientId`, `resultingClientId`,
`preloadResponse`, `handled`, `respondWith`), stream the response back as the navigation's response, or fall
back to the network when the worker does not respond, throws, or the request's service-workers mode is
`none`. The reserved client becomes controlled at commit (`navigator.serviceWorker.controller`). Navigation
preload (`NavigationPreloadManager`). Soft update after a navigation (and the 24 h rule).
WPT: `fetch-*`, `navigation-*`, `controller-*`, `claim-*` files.

### Stage 4: fetch interception for subresources (M to L)

A controlled client's requests (fetch, XHR, images, scripts, styles, workers' fetches; not those with
service-workers mode `none`) go to the controller. The client's net backend gets a pipe to the controller worker
from the browser (at commit and on `controllerchange`) and sends each request there; a fallback goes to the
network as today. Dedicated workers inherit their owner's controller. `Response` from `respondWith` keeps its
type (opaque, cors, basic) and `url` list for the request's mode checks (the spec's "handle fetch" result).
WPT: the rest of `service-workers/service-worker/` (`fetch-request-*`, `fetch-event-*`, `fetch-cors-*`,
`opaque-response-*`, `resource-timing-*`).

### Stage 5: clients and messaging (M)

`Clients.get`, `matchAll` (with `includeUncontrolled` and `type`), `openWindow`, `WindowClient.focus` and
`navigate`, `Client.postMessage` and the container's `message` events (with `startMessages`),
`ExtendableMessageEvent`. Uses the browser's tab list, so it lives in the browser half.

### Not queued

Push, Notifications from a worker, Background Sync, Background Fetch, Payment handlers, the static routing API
(`InstallEvent.addRoutes`), `SharedWorker`. Each depends on stage 2.

## WPT areas and counts

Counted 2026-11-13 on `dist` (main at b5456dbf), `--no-reftests --https` (run name `sw-baseline-b5456dbf-https`;
without `--https` the runner finds 1 file):

| Area | Files | Completed | Subtests passed |
|---|---:|---:|---:|
| `service-workers` (top-level files) | 2 | 0 | 74 / 230 |
| `service-workers/cache-storage` | 28 | 11 | 0 / 313 |
| `service-workers/service-worker` | 271 | 253 | 70 / 1,570 |
| **All** | 301 | 264 | **144 / 2,113** (6.8%) |

Most failures read `navigator.serviceWorker` (`register`, `getRegistration`) or `caches` as undefined; the
passing ones are mostly IDL checks of interfaces that exist and tests that need no worker.

The `.serviceworker.` variants of other areas are not counted (the runner skips them, stage 0).

## Order and risks

- Stage 1 can start alone. Stages 2 to 4 are in order. Stage 5 after 2.
- The registry and cache in the browser half must go through the trait from the first commit, with
  `SNOWLARK_IPC=loopback` exercising it: a shortcut through a shared `Arc` is what process isolation section 14
  warned against.
- The navigation interceptor touches the M5 navigation path; coordinate with any open work there.
- A worker that never answers must not hang a navigation: every dispatch has a timeout and a network fallback.
