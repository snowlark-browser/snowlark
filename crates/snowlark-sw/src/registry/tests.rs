// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use super::*;

const KEY: &str = "https://a.test";
const PAGE: ConnId = 1;

fn url(s: &str) -> Url {
    Url::parse(s).unwrap()
}

fn client() -> snowlark_net::Client {
    snowlark_net::Client::for_url(&url("https://a.test/page.html"))
}

fn registry() -> Registry {
    let mut r = Registry::new();
    r.connect(PAGE, KEY.into(), true);
    r
}

fn register(r: &mut Registry, call: CallId, script: &str, scope: &str) {
    r.call(
        PAGE,
        call,
        Call::Register {
            client: client(),
            script_url: url(script),
            scope: url(scope),
            update_via_cache: UpdateViaCache::Imports,
            worker_type: WorkerType::Classic,
        },
        Instant::now(),
    );
}

fn response(
    script: &str,
    mime: &str,
    body: &str,
    headers: &[(&str, &str)],
) -> snowlark_net::Response {
    let mut head = snowlark_net::ResponseHead::new(200, "OK");
    head.url_list = vec![url(script)];
    head.headers.append("Content-Type", mime);
    for (n, v) in headers {
        head.headers.append(n, v);
    }
    snowlark_net::Response {
        head,
        body: Some(body.as_bytes().to_vec().into()),
    }
}

/// The job id of the one fetch among `effects`, and its request.
fn fetch_of(effects: &[Effect]) -> (u64, snowlark_net::Request) {
    let mut found = effects.iter().filter_map(|e| match e {
        Effect::Fetch { job, request } => Some((*job, (**request).clone())),
        _ => None,
    });
    let f = found.next().expect("a fetch");
    assert!(found.next().is_none());
    f
}

fn notices(effects: &[Effect]) -> Vec<Notice> {
    effects
        .iter()
        .filter_map(|e| match e {
            Effect::Notify(PAGE, n) => Some(n.clone()),
            _ => None,
        })
        .collect()
}

fn answer(effects: &[Effect], call: CallId) -> Option<Answer> {
    notices(effects).into_iter().find_map(|n| match n {
        Notice::Answer { call: c, answer } if c == call => Some(answer),
        _ => None,
    })
}

fn started(effects: &[Effect]) -> Option<VersionId> {
    notices(effects).into_iter().find_map(|n| match n {
        Notice::Start { version, .. } => Some(version.id),
        _ => None,
    })
}

/// Registers and runs the worker to `activated`; returns the version.
fn install_and_activate(r: &mut Registry, script: &str, scope: &str, body: &str) -> VersionId {
    let now = Instant::now();
    register(r, 1, script, scope);
    let (job, _) = fetch_of(&r.take_effects());
    r.fetched(job, Ok(response(script, "text/javascript", body, &[])), now);
    let v = started(&r.take_effects()).expect("started");
    r.call(
        PAGE,
        2,
        Call::Evaluated {
            version: v,
            ok: true,
        },
        now,
    );
    r.take_effects();
    r.call(
        PAGE,
        3,
        Call::EventDone {
            version: v,
            event: Lifecycle::Install,
            ok: true,
        },
        now,
    );
    r.take_effects();
    r.call(
        PAGE,
        4,
        Call::EventDone {
            version: v,
            event: Lifecycle::Activate,
            ok: true,
        },
        now,
    );
    r.take_effects();
    v
}

#[test]
fn a_registration_installs_and_activates_in_the_spec_order() {
    let mut r = registry();
    let now = Instant::now();
    register(&mut r, 1, "https://a.test/sw/w.js", "https://a.test/sw/");
    let (job, request) = fetch_of(&r.take_effects());
    assert_eq!(
        request.destination,
        snowlark_net::Destination::ServiceWorker
    );
    assert_eq!(request.mode, snowlark_net::Mode::SameOrigin);
    assert_eq!(
        request.headers.get("Service-Worker").as_deref(),
        Some("script")
    );
    r.fetched(
        job,
        Ok(response(
            "https://a.test/sw/w.js",
            "text/javascript",
            "1",
            &[],
        )),
        now,
    );
    let v = started(&r.take_effects()).expect("the worker starts for evaluation");

    r.call(
        PAGE,
        2,
        Call::Evaluated {
            version: v,
            ok: true,
        },
        now,
    );
    let e = r.take_effects();
    let order: Vec<String> = notices(&e)
        .iter()
        .map(|n| match n {
            Notice::Slot { slot, version, .. } => format!("{slot:?}={}", version.is_some()),
            Notice::State { state, .. } => state.as_str().to_owned(),
            Notice::Answer { .. } => "answer".into(),
            Notice::UpdateFound { .. } => "updatefound".into(),
            Notice::Dispatch { event, .. } => format!("{event:?}"),
            other => format!("{other:?}"),
        })
        .collect();
    assert_eq!(
        order,
        [
            "Installing=true",
            "installing",
            "answer",
            "updatefound",
            "Install"
        ]
    );
    match answer(&e, 1) {
        Some(Answer::Registration(Ok(info))) => {
            assert_eq!(info.scope.as_str(), "https://a.test/sw/");
            assert_eq!(
                info.installing.map(|v| v.state),
                Some(WorkerState::Installing)
            );
        }
        other => panic!("{other:?}"),
    }

    r.call(
        PAGE,
        3,
        Call::EventDone {
            version: v,
            event: Lifecycle::Install,
            ok: true,
        },
        now,
    );
    let e = r.take_effects();
    let states: Vec<WorkerState> = notices(&e)
        .iter()
        .filter_map(|n| match n {
            Notice::State { state, .. } => Some(*state),
            _ => None,
        })
        .collect();
    assert_eq!(states, [WorkerState::Installed, WorkerState::Activating]);
    assert!(notices(&e).iter().any(|n| matches!(
        n,
        Notice::Dispatch {
            event: Lifecycle::Activate,
            ..
        }
    )));

    r.call(
        PAGE,
        4,
        Call::EventDone {
            version: v,
            event: Lifecycle::Activate,
            ok: true,
        },
        now,
    );
    let e = r.take_effects();
    assert!(notices(&e).iter().any(|n| matches!(
        n,
        Notice::State {
            state: WorkerState::Activated,
            ..
        }
    )));
    assert!(e
        .iter()
        .any(|e| matches!(e, Effect::Persist(k) if k == KEY)));
    assert_eq!(r.stored(KEY).len(), 1);
}

#[test]
fn the_script_must_be_javascript() {
    let mut r = registry();
    register(&mut r, 1, "https://a.test/w.js", "https://a.test/");
    let (job, _) = fetch_of(&r.take_effects());
    r.fetched(
        job,
        Ok(response("https://a.test/w.js", "text/plain", "1", &[])),
        Instant::now(),
    );
    match answer(&r.take_effects(), 1) {
        Some(Answer::Registration(Err(e))) => assert_eq!(e.name, "SecurityError"),
        other => panic!("{other:?}"),
    }
    r.call(PAGE, 2, Call::GetRegistrations, Instant::now());
    assert!(matches!(answer(&r.take_effects(), 2), Some(Answer::Lookups(v)) if v.is_empty()));
}

#[test]
fn the_scope_must_be_under_the_script_unless_the_server_allows_it() {
    let mut r = registry();
    let now = Instant::now();
    register(&mut r, 1, "https://a.test/sw/w.js", "https://a.test/");
    let (job, _) = fetch_of(&r.take_effects());
    r.fetched(
        job,
        Ok(response(
            "https://a.test/sw/w.js",
            "text/javascript",
            "1",
            &[],
        )),
        now,
    );
    match answer(&r.take_effects(), 1) {
        Some(Answer::Registration(Err(e))) => assert_eq!(e.name, "SecurityError"),
        other => panic!("{other:?}"),
    }
    register(&mut r, 2, "https://a.test/sw/w.js", "https://a.test/");
    let (job, _) = fetch_of(&r.take_effects());
    let allowed = [("Service-Worker-Allowed", "/")];
    r.fetched(
        job,
        Ok(response(
            "https://a.test/sw/w.js",
            "text/javascript",
            "1",
            &allowed,
        )),
        now,
    );
    assert!(started(&r.take_effects()).is_some());
}

#[test]
fn origins_and_schemes_are_checked_before_any_fetch() {
    let mut r = registry();
    register(&mut r, 1, "https://b.test/w.js", "https://a.test/");
    let e = r.take_effects();
    assert!(!e.iter().any(|e| matches!(e, Effect::Fetch { .. })));
    match answer(&e, 1) {
        Some(Answer::Registration(Err(e))) => assert_eq!(e.name, "SecurityError"),
        other => panic!("{other:?}"),
    }
    register(&mut r, 2, "https://a.test/w.js", "https://a.test/a%2fb/");
    match answer(&r.take_effects(), 2) {
        Some(Answer::Registration(Err(e))) => assert_eq!(e.name, "TypeError"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_script_that_throws_rejects_and_leaves_no_registration() {
    let mut r = registry();
    let now = Instant::now();
    register(&mut r, 1, "https://a.test/w.js", "https://a.test/");
    let (job, _) = fetch_of(&r.take_effects());
    r.fetched(
        job,
        Ok(response(
            "https://a.test/w.js",
            "text/javascript",
            "throw 1",
            &[],
        )),
        now,
    );
    let v = started(&r.take_effects()).unwrap();
    r.call(
        PAGE,
        2,
        Call::Evaluated {
            version: v,
            ok: false,
        },
        now,
    );
    match answer(&r.take_effects(), 1) {
        Some(Answer::Registration(Err(e))) => assert_eq!(e.name, "TypeError"),
        other => panic!("{other:?}"),
    }
    r.call(
        PAGE,
        3,
        Call::GetRegistration {
            client_url: url("https://a.test/x"),
        },
        now,
    );
    assert!(matches!(
        answer(&r.take_effects(), 3),
        Some(Answer::Lookup(None))
    ));
}

#[test]
fn registering_the_same_script_again_fetches_nothing() {
    let mut r = registry();
    install_and_activate(&mut r, "https://a.test/w.js", "https://a.test/", "1");
    register(&mut r, 9, "https://a.test/w.js", "https://a.test/");
    let e = r.take_effects();
    assert!(!e.iter().any(|e| matches!(e, Effect::Fetch { .. })));
    assert!(matches!(answer(&e, 9), Some(Answer::Registration(Ok(_)))));
}

#[test]
fn an_update_with_the_same_bytes_installs_nothing_and_new_bytes_replace_the_worker() {
    let mut r = registry();
    let now = Instant::now();
    let v1 = install_and_activate(&mut r, "https://a.test/w.js", "https://a.test/", "1");
    let rid = r.match_registration(KEY, &url("https://a.test/x")).unwrap();
    r.call(
        PAGE,
        10,
        Call::Update {
            client: client(),
            registration: rid,
        },
        now,
    );
    let (job, _) = fetch_of(&r.take_effects());
    r.fetched(
        job,
        Ok(response("https://a.test/w.js", "text/javascript", "1", &[])),
        now,
    );
    let e = r.take_effects();
    assert!(started(&e).is_none());
    assert!(matches!(answer(&e, 10), Some(Answer::Registration(Ok(_)))));

    r.call(
        PAGE,
        11,
        Call::Update {
            client: client(),
            registration: rid,
        },
        now,
    );
    let (job, _) = fetch_of(&r.take_effects());
    r.fetched(
        job,
        Ok(response("https://a.test/w.js", "text/javascript", "2", &[])),
        now,
    );
    let v2 = started(&r.take_effects()).unwrap();
    assert_ne!(v1, v2);
    r.call(
        PAGE,
        12,
        Call::Evaluated {
            version: v2,
            ok: true,
        },
        now,
    );
    r.take_effects();
    r.call(
        PAGE,
        13,
        Call::EventDone {
            version: v2,
            event: Lifecycle::Install,
            ok: true,
        },
        now,
    );
    let e = r.take_effects();
    // (the old active worker is made redundant when the new one activates)
    assert!(notices(&e).iter().any(
        |n| matches!(n, Notice::State { version, state: WorkerState::Redundant } if *version == v1)
    ));
    assert!(notices(&e).iter().any(
        |n| matches!(n, Notice::State { version, state: WorkerState::Activating } if *version == v2)
    ));
}

#[test]
fn unregister_clears_the_registration_and_makes_its_workers_redundant() {
    let mut r = registry();
    let now = Instant::now();
    let v = install_and_activate(&mut r, "https://a.test/w.js", "https://a.test/", "1");
    let rid = r.match_registration(KEY, &url("https://a.test/")).unwrap();
    r.call(
        PAGE,
        20,
        Call::Unregister {
            client: client(),
            registration: rid,
        },
        now,
    );
    let e = r.take_effects();
    assert!(matches!(
        answer(&e, 20),
        Some(Answer::Unregistered(Ok(true)))
    ));
    assert!(notices(&e).iter().any(
        |n| matches!(n, Notice::State { version, state: WorkerState::Redundant } if *version == v)
    ));
    assert!(r.stored(KEY).is_empty());
    r.call(
        PAGE,
        21,
        Call::Unregister {
            client: client(),
            registration: rid,
        },
        now,
    );
    assert!(matches!(
        answer(&r.take_effects(), 21),
        Some(Answer::Unregistered(Ok(false)))
    ));
}

#[test]
fn ready_waits_for_an_active_worker_and_the_longest_scope_matches() {
    let mut r = registry();
    let now = Instant::now();
    r.call(
        PAGE,
        30,
        Call::Ready {
            client_url: url("https://a.test/a/b/page"),
        },
        now,
    );
    assert!(answer(&r.take_effects(), 30).is_none());
    install_and_activate(&mut r, "https://a.test/w.js", "https://a.test/", "1");
    install_and_activate(&mut r, "https://a.test/a/w.js", "https://a.test/a/", "1");
    let rid = r
        .match_registration(KEY, &url("https://a.test/a/b/page"))
        .unwrap();
    r.call(
        PAGE,
        31,
        Call::GetRegistration {
            client_url: url("https://a.test/a/b/page"),
        },
        now,
    );
    match answer(&r.take_effects(), 31) {
        Some(Answer::Lookup(Some(info))) => {
            assert_eq!(info.id, rid);
            assert_eq!(info.scope.as_str(), "https://a.test/a/");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn an_install_that_never_settles_fails_after_its_timeout() {
    let mut r = registry();
    let now = Instant::now();
    register(&mut r, 1, "https://a.test/w.js", "https://a.test/");
    let (job, _) = fetch_of(&r.take_effects());
    r.fetched(
        job,
        Ok(response("https://a.test/w.js", "text/javascript", "1", &[])),
        now,
    );
    let v = started(&r.take_effects()).unwrap();
    r.call(
        PAGE,
        2,
        Call::Evaluated {
            version: v,
            ok: true,
        },
        now,
    );
    r.take_effects();
    assert!(r.tick(now).is_some());
    r.tick(now + EVENT_TIMEOUT + Duration::from_secs(1));
    let e = r.take_effects();
    assert!(notices(&e).iter().any(|n| matches!(
        n,
        Notice::State {
            state: WorkerState::Redundant,
            ..
        }
    )));
    r.call(PAGE, 3, Call::GetRegistrations, now);
    assert!(matches!(answer(&r.take_effects(), 3), Some(Answer::Lookups(v)) if v.is_empty()));
}

#[test]
fn stored_registrations_come_back_active() {
    let mut r = registry();
    install_and_activate(&mut r, "https://a.test/w.js", "https://a.test/", "1");
    let stored = r.stored(KEY);
    let mut again = registry();
    again.restore(KEY, stored, Instant::now());
    again.call(
        PAGE,
        1,
        Call::GetRegistration {
            client_url: url("https://a.test/"),
        },
        Instant::now(),
    );
    match answer(&again.take_effects(), 1) {
        Some(Answer::Lookup(Some(info))) => {
            assert_eq!(info.active.map(|v| v.state), Some(WorkerState::Activated))
        }
        other => panic!("{other:?}"),
    }
}

fn add_client(r: &mut Registry, client: ClientId, at: &str, secure: bool) {
    r.call(
        PAGE,
        0,
        Call::Client {
            client,
            url: url(at),
            secure,
        },
        Instant::now(),
    );
}

/// The clients `effects` tell of a new controller, with the controller's version.
fn controllers(effects: &[Effect]) -> Vec<(ClientId, Option<VersionId>)> {
    notices(effects)
        .into_iter()
        .filter_map(|n| match n {
            Notice::Controller { client, version } => Some((client, version.map(|v| v.id))),
            _ => None,
        })
        .collect()
}

/// Registers under `/sw/`, adds the in-scope client 1, and claims it; returns the version.
fn claimed(r: &mut Registry) -> VersionId {
    add_client(r, 1, "https://a.test/sw/page.html", true);
    let v = install_and_activate(r, "https://a.test/sw/w.js", "https://a.test/sw/", "1");
    r.call(PAGE, 30, Call::Claim { version: v }, Instant::now());
    let e = r.take_effects();
    assert!(matches!(answer(&e, 30), Some(Answer::Claimed(Ok(())))));
    assert_eq!(controllers(&e), vec![(1, Some(v))]);
    v
}

#[test]
fn claim_controls_the_secure_clients_its_scope_matches() {
    let mut r = registry();
    // (out of scope, not a secure context, and in scope)
    add_client(&mut r, 2, "https://a.test/other.html", true);
    add_client(&mut r, 3, "https://a.test/sw/insecure.html", false);
    let v = claimed(&mut r);
    // (claiming again changes nothing: the client already has it)
    r.call(PAGE, 31, Call::Claim { version: v }, Instant::now());
    let e = r.take_effects();
    assert!(matches!(answer(&e, 31), Some(Answer::Claimed(Ok(())))));
    assert!(controllers(&e).is_empty());
}

#[test]
fn only_the_active_worker_may_claim() {
    let mut r = registry();
    let now = Instant::now();
    add_client(&mut r, 1, "https://a.test/page.html", true);
    register(&mut r, 1, "https://a.test/w.js", "https://a.test/");
    let (job, _) = fetch_of(&r.take_effects());
    r.fetched(
        job,
        Ok(response("https://a.test/w.js", "text/javascript", "1", &[])),
        now,
    );
    let v = started(&r.take_effects()).unwrap();
    r.call(PAGE, 2, Call::Claim { version: v }, now);
    let e = r.take_effects();
    match answer(&e, 2) {
        Some(Answer::Claimed(Err(err))) => assert_eq!(err.name, "InvalidStateError"),
        other => panic!("{other:?}"),
    }
    assert!(controllers(&e).is_empty());
}

#[test]
fn an_unregistered_registration_stays_until_its_last_client_goes() {
    let mut r = registry();
    let now = Instant::now();
    let v = claimed(&mut r);
    let rid = r
        .match_registration(KEY, &url("https://a.test/sw/"))
        .unwrap();
    r.call(
        PAGE,
        40,
        Call::Unregister {
            client: client(),
            registration: rid,
        },
        now,
    );
    let e = r.take_effects();
    assert!(matches!(
        answer(&e, 40),
        Some(Answer::Unregistered(Ok(true)))
    ));
    // (out of the map at once, but its worker still controls the client)
    assert!(r
        .match_registration(KEY, &url("https://a.test/sw/"))
        .is_none());
    assert!(!notices(&e).iter().any(|n| matches!(
        n,
        Notice::State {
            state: WorkerState::Redundant,
            ..
        }
    )));
    r.call(PAGE, 0, Call::ClientGone { client: 1 }, now);
    let e = r.take_effects();
    assert!(notices(&e).iter().any(
        |n| matches!(n, Notice::State { version, state: WorkerState::Redundant } if *version == v)
    ));
}

#[test]
fn a_waiting_worker_waits_while_a_client_uses_the_active_one() {
    let mut r = registry();
    let now = Instant::now();
    let v1 = claimed(&mut r);
    let rid = r
        .match_registration(KEY, &url("https://a.test/sw/"))
        .unwrap();
    r.call(
        PAGE,
        50,
        Call::Update {
            client: client(),
            registration: rid,
        },
        now,
    );
    let (job, _) = fetch_of(&r.take_effects());
    r.fetched(
        job,
        Ok(response(
            "https://a.test/sw/w.js",
            "text/javascript",
            "2",
            &[],
        )),
        now,
    );
    let v2 = started(&r.take_effects()).unwrap();
    r.call(
        PAGE,
        51,
        Call::Evaluated {
            version: v2,
            ok: true,
        },
        now,
    );
    r.take_effects();
    r.call(
        PAGE,
        52,
        Call::EventDone {
            version: v2,
            event: Lifecycle::Install,
            ok: true,
        },
        now,
    );
    let e = r.take_effects();
    assert!(notices(&e).iter().any(
        |n| matches!(n, Notice::State { version, state: WorkerState::Installed } if *version == v2)
    ));
    assert!(!notices(&e).iter().any(|n| matches!(
        n,
        Notice::State {
            state: WorkerState::Activating,
            ..
        }
    )));
    // (the client goes: the waiting worker activates; skipWaiting would not have waited)
    r.call(PAGE, 0, Call::ClientGone { client: 1 }, now);
    let e = r.take_effects();
    assert!(notices(&e).iter().any(
        |n| matches!(n, Notice::State { version, state: WorkerState::Redundant } if *version == v1)
    ));
    assert!(notices(&e).iter().any(
        |n| matches!(n, Notice::State { version, state: WorkerState::Activating } if *version == v2)
    ));
}

#[test]
fn skip_waiting_activates_and_the_clients_follow_the_new_worker() {
    let mut r = registry();
    let now = Instant::now();
    claimed(&mut r);
    let rid = r
        .match_registration(KEY, &url("https://a.test/sw/"))
        .unwrap();
    r.call(
        PAGE,
        60,
        Call::Update {
            client: client(),
            registration: rid,
        },
        now,
    );
    let (job, _) = fetch_of(&r.take_effects());
    r.fetched(
        job,
        Ok(response(
            "https://a.test/sw/w.js",
            "text/javascript",
            "2",
            &[],
        )),
        now,
    );
    let v2 = started(&r.take_effects()).unwrap();
    r.call(
        PAGE,
        61,
        Call::Evaluated {
            version: v2,
            ok: true,
        },
        now,
    );
    r.call(PAGE, 62, Call::SkipWaiting { version: v2 }, now);
    r.take_effects();
    r.call(
        PAGE,
        63,
        Call::EventDone {
            version: v2,
            event: Lifecycle::Install,
            ok: true,
        },
        now,
    );
    let e = r.take_effects();
    assert_eq!(controllers(&e), vec![(1, Some(v2))]);
}
