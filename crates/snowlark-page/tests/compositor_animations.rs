// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! CSS animations a compositor runs (`docs/compositor-animations.md`, S2): once a frame painted for
//! the compositor carries them, the page asks no rendering update for them but the one at their next
//! change (Blink's `DocumentTimeline::ScheduleNextService`), and between rendering updates script
//! still reads their values, times and events at the time it sees (Blink's `AnimationClock`, which
//! moves once per task outside rendering updates).

use std::time::{Duration, Instant};

use snowlark_net::{NetConfig, NetThread};
use snowlark_page::{Page, PageOptions, RenderSetup};
use snowlark_paint::Point;

fn eval(page: &mut Page, js: &str) -> String {
    match page.eval(js, "test.js") {
        Ok(v) => v,
        Err(e) => panic!("{js}: threw {e}"),
    }
}

fn num(page: &mut Page, js: &str) -> f64 {
    let v = eval(page, js);
    v.parse().unwrap_or_else(|_| panic!("{js}: {v}"))
}

/// How long until the event loop wants to wake (`None`: never by itself).
fn wake_in(page: &Page) -> Option<Duration> {
    page.engine()
        .next_wakeup()
        .map(|t| t.saturating_duration_since(Instant::now()))
}

#[test]
fn compositor_animations_ask_for_their_changes_and_script_still_sees_the_time() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        viewport: (800, 600),
        render: Some(RenderSetup::new().unwrap()),
        ..Default::default()
    });
    page.load_html(
        "<!doctype html><style>
           div { position: absolute; width: 40px; height: 30px; background: teal }
           @keyframes spin { to { transform: rotate(1turn) } }
           @keyframes fade { from { opacity: 0 } to { opacity: .5 } }
           #a { left: 10px; animation: spin 1s linear infinite }
           #b { left: 60px; animation: fade 2s linear 3 }
         </style><div id=a></div><div id=b></div>",
        &url::Url::parse("http://example.test/").unwrap(),
    );
    assert!(page.run_until_settled(Duration::from_secs(10)), "settles");
    page.update_the_rendering();
    // (before a frame for a compositor, the page services every frame)
    assert!(wake_in(&page).is_some_and(|w| w < Duration::from_millis(20)));
    page.engine().paint(Point::ZERO, 1.0).unwrap();
    assert_eq!(
        page.engine()
            .paint(Point::ZERO, 1.0)
            .unwrap()
            .animations
            .len(),
        2
    );
    // The next update the page wants is #b's end, 6 s on (not the next frame).
    let wait = wake_in(&page).expect("wakes for #b's end");
    assert!(wait > Duration::from_secs(4), "{wait:?}");

    // No rendering update runs; script in a later task reads the time that has passed.
    let t0 = num(&mut page, "document.timeline.currentTime");
    std::thread::sleep(Duration::from_millis(150));
    let read = eval(
        &mut page,
        "const b = document.getElementById('b');
         const t = document.timeline.currentTime;
         const anim = b.getAnimations()[0];
         const o = getComputedStyle(b).opacity;
         // (it does not change within a task)
         const s = performance.now(); while (performance.now() - s < 30) {}
         [t, anim.startTime, anim.currentTime, o, document.timeline.currentTime === t].join(' ')",
    );
    let v: Vec<&str> = read.split(' ').collect();
    let t: f64 = v[0].parse().unwrap();
    let start: f64 = v[1].parse().unwrap();
    let current: f64 = v[2].parse().unwrap();
    let opacity: f64 = v[3].parse().unwrap();
    assert!(t - t0 >= 100.0, "the timeline moved: {t0} -> {t}");
    assert!((current - (t - start)).abs() < 1e-6, "{read}");
    let want = 0.5 * (current % 2000.0) / 2000.0;
    assert!(
        (opacity - want).abs() < 0.005,
        "getComputedStyle at the time script sees: {opacity}, want {want} ({read})"
    );
    assert_eq!(v[4], "true", "{read}");
    // (the `CSSAnimation` object script made asks for no frames either)
    let wait = wake_in(&page).expect("wakes for #b's end");
    assert!(wait > Duration::from_secs(4), "{wait:?}");

    // A listener for `animationiteration` asks for the update at #b's next iteration (2 s on).
    eval(
        &mut page,
        "document.getElementById('b').addEventListener('animationiteration', () => {})",
    );
    let wait = wake_in(&page).expect("wakes for #b's next iteration");
    let to_next = 2000.0 - current % 2000.0;
    assert!(
        wait > Duration::from_millis(10) && wait.as_secs_f64() * 1000.0 < to_next,
        "{wait:?}, {to_next} ms from the read"
    );
}

/// The first rendering update's transition events and animation frame callbacks run before its
/// restyle; script in them still reads styles at the frame's time (CSS Transitions 1 §3: a
/// completed transition no longer applies).
#[test]
fn script_in_the_first_update_reads_transitions_at_the_frame_time() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        viewport: (800, 600),
        render: Some(RenderSetup::new().unwrap()),
        ..Default::default()
    });
    page.load_html(
        "<!doctype html><div id=a style='transition: width 0.02s; width: 0px'></div>
         <div id=b style='transition: width 100s linear; width: 0px'></div>",
        &url::Url::parse("http://example.test/").unwrap(),
    );
    eval(
        &mut page,
        "window.LOG = [];
         const a = document.getElementById('a'), b = document.getElementById('b');
         getComputedStyle(a).width; getComputedStyle(b).width;
         a.style.width = '100px'; b.style.width = '100px';
         getComputedStyle(a).width; getComputedStyle(b).width;
         a.addEventListener('transitionend', () => LOG.push('end ' + getComputedStyle(a).width));
         requestAnimationFrame(() => LOG.push('raf ' + getComputedStyle(b).width));",
    );
    // (#a's 20 ms are over before the first rendering update)
    std::thread::sleep(Duration::from_millis(100));
    page.update_the_rendering();
    let log = eval(&mut page, "LOG.join(', ')");
    assert!(log.contains("end 100px"), "#a completed: {log}");
    let b: f64 = log
        .split("raf ")
        .nth(1)
        .and_then(|s| s.split("px").next())
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("{log}"));
    assert!(b > 0.0, "#b runs: {log}");
}

/// DOM's move (`moveBefore`) keeps the element connected, so its transition goes on through the next
/// rendering update, which moves the timeline before it restyles the moved element.
#[test]
fn a_moved_element_keeps_its_transition() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        viewport: (800, 600),
        render: Some(RenderSetup::new().unwrap()),
        ..Default::default()
    });
    page.load_html(
        "<!doctype html><style>#item { position: absolute; left: 0; width: 10px; height: 10px;
           transition: left 10s linear }</style>
         <section id=old><div id=item></div></section><section id=new></section>",
        &url::Url::parse("http://example.test/").unwrap(),
    );
    assert!(page.run_until_settled(Duration::from_secs(10)), "settles");
    page.update_the_rendering();
    eval(
        &mut page,
        "const item = document.getElementById('item');
         item.style.left = '400px'; getComputedStyle(item).left;",
    );
    std::thread::sleep(Duration::from_millis(50));
    page.update_the_rendering();
    eval(
        &mut page,
        "document.getElementById('new').moveBefore(document.getElementById('item'), null)",
    );
    std::thread::sleep(Duration::from_millis(50));
    page.update_the_rendering();
    let read = eval(
        &mut page,
        "const it = document.getElementById('item');
         it.getAnimations().length + ' ' + parseFloat(getComputedStyle(it).left)",
    );
    let v: Vec<&str> = read.split(' ').collect();
    assert_eq!(v[0], "1", "the transition runs: {read}");
    let left: f64 = v[1].parse().unwrap();
    assert!(left > 0.0 && left < 399.0, "{read}");
    // Insertion is no move: removed and inserted again, the element loses its transition (CSS
    // Transitions 1 §3).
    eval(
        &mut page,
        "document.getElementById('old').append(document.getElementById('item'))",
    );
    std::thread::sleep(Duration::from_millis(50));
    page.update_the_rendering();
    let read = eval(
        &mut page,
        "const el = document.getElementById('item');
         el.getAnimations().length + ' ' + getComputedStyle(el).left",
    );
    assert_eq!(read, "0 400px");
}

/// An animation that finishes at a time script sees between rendering updates (the clock moves per
/// task) still gets the update that runs its finished state: its promise resolves.
#[test]
fn an_animation_finished_between_updates_still_resolves_its_promise() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        viewport: (800, 600),
        render: Some(RenderSetup::new().unwrap()),
        ..Default::default()
    });
    page.load_html(
        "<!doctype html><div id=d>x</div>",
        &url::Url::parse("http://example.test/").unwrap(),
    );
    assert!(page.run_until_settled(Duration::from_secs(10)), "settles");
    eval(
        &mut page,
        "window.log = []; const div = document.getElementById('d');
         const a = div.animate({ opacity: 1 }, { duration: 1, fill: 'forwards' });
         a.finished.then(() => log.push('finished ' + a.playState));",
    );
    let deadline = Instant::now() + Duration::from_millis(300);
    while Instant::now() < deadline {
        page.run_until(deadline, |_| false);
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(eval(&mut page, "log.join(',')"), "finished finished");
}

/// A seek of a paused CSS animation from an event handler (a microtask of the rendering update that
/// dispatched it) still gets its events: the scan that found them is not the update's own, so the
/// next update is due, and the loop counts it as work (WPT `css-animations/event-order.tentative`).
#[test]
fn events_of_a_seek_in_an_event_handler_still_fire() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        viewport: (800, 600),
        render: Some(RenderSetup::new().unwrap()),
        ..Default::default()
    });
    page.load_html(
        "<!doctype html><style>@keyframes anim { from { margin-left: 0px } to { margin-left: 100px } }</style><body>",
        &url::Url::parse("http://example.test/").unwrap(),
    );
    assert!(page.run_until_settled(Duration::from_secs(10)), "settles");
    eval(
        &mut page,
        "window.log = []; window.mk = () => { const d = document.createElement('div'); d.style.animation = 'anim 100s 2 paused'; document.body.append(d);
           for (const n of ['start','iteration','end']) d['onanimation'+n] = e => log.push(e.type + ' ' + e.elapsedTime);
           const w = {}; for (const n of ['animationstart','animationiteration','animationend']) d.addEventListener(n, () => { log.push('w ' + n); });
           return d.getAnimations()[0]; };
         const once = (d, n) => new Promise(r => d.addEventListener(n, r, { once: true }));
         (async () => {
           const a1 = mk(), a2 = mk();
           const [d1, d2] = [a1.effect.target, a2.effect.target];
           await Promise.all([once(d1, 'animationstart'), once(d2, 'animationstart')]);
           log.push('|');
           a1.currentTime = 100000; a2.currentTime = 100000;
           await Promise.all([once(d1, 'animationiteration'), once(d2, 'animationiteration')]);
           log.push('|');
           a1.finish(); a2.finish();
           await Promise.all([once(d1, 'animationend'), once(d2, 'animationend')]);
           window.done = 'yes';
         })();",
    );
    let deadline = Instant::now() + Duration::from_millis(2000);
    let done = page.run_until(deadline, |p| {
        p.global_string("done").as_deref() == Some("yes")
    });
    let log = eval(&mut page, "log.join(',')");
    assert!(done, "all the events arrive: {log}");
    assert_eq!(
        log,
        [
            "animationstart 0,w animationstart,animationstart 0,w animationstart",
            "animationiteration 100,w animationiteration,animationiteration 100,w animationiteration",
            "animationend 200,w animationend,animationend 200,w animationend",
        ]
        .join(",|,")
    );
}

/// S2, script animations: an `element.animate()` animation a compositor can run goes with the frame
/// (Blink's `Animation::PreCommit` starts it there), and the page then asks only for the update at
/// its next change (`Animation::TimeToEffectChange`); script reads between rendering updates see
/// its values at the time they see. A paused one leaves at the next frame, and one the compositor
/// cannot run (an end delay: `ConvertTimingForCompositor`) keeps the page ticking.
#[test]
fn script_animations_run_on_the_compositor() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        viewport: (800, 600),
        render: Some(RenderSetup::new().unwrap()),
        ..Default::default()
    });
    page.load_html(
        "<!doctype html><style>
           div { position: absolute; width: 40px; height: 30px; background: teal }
         </style><div id=a style='left: 10px'></div><div id=b style='left: 60px'></div>
         <div id=c style='left: 110px'></div>",
        &url::Url::parse("http://example.test/").unwrap(),
    );
    assert!(page.run_until_settled(Duration::from_secs(10)), "settles");
    eval(
        &mut page,
        "window.a1 = document.getElementById('a').animate(
           { transform: ['rotate(0)', 'rotate(1turn)'] }, { duration: 1000, iterations: Infinity });
         window.b1 = document.getElementById('b').animate(
           { opacity: [0, .5] }, { duration: 3000, delay: 1000 });
         b1.playbackRate = 2;",
    );
    // (the pending play tasks run at the next rendering update)
    page.update_the_rendering();
    page.engine().paint(Point::ZERO, 1.0).unwrap();
    assert_eq!(
        page.engine()
            .paint(Point::ZERO, 1.0)
            .unwrap()
            .animations
            .len(),
        2
    );
    // The next update the page wants is #b's start, 1000 / 2 ms on (not the next frame), then its
    // end, (1000 + 3000) / 2 ms on.
    let wait = wake_in(&page).expect("wakes for #b's start");
    assert!(
        wait > Duration::from_millis(250) && wait <= Duration::from_millis(500),
        "{wait:?}"
    );
    std::thread::sleep(wait + Duration::from_millis(20));
    page.update_the_rendering();
    let list = page.engine().paint(Point::ZERO, 1.0).unwrap();
    assert_eq!(list.animations.len(), 2);
    let wait = wake_in(&page).expect("wakes for #b's end");
    assert!(wait > Duration::from_millis(1200), "{wait:?}");

    // No rendering update runs; script in a later task reads the value at the time it sees.
    std::thread::sleep(Duration::from_millis(150));
    let read = eval(
        &mut page,
        "[b1.currentTime, getComputedStyle(document.getElementById('b')).opacity].join(' ')",
    );
    let v: Vec<f64> = read.split(' ').map(|x| x.parse().unwrap()).collect();
    assert!(v[0] > 1250.0, "{read}");
    let want = 0.5 * (v[0] - 1000.0) / 3000.0;
    assert!(
        (v[1] - want).abs() < 0.002,
        "getComputedStyle at the time script sees: {}, want {want}",
        v[1]
    );

    // Paused, it leaves the compositor at the next frame (the page paints it).
    eval(&mut page, "b1.pause()");
    page.update_the_rendering();
    let list = page.engine().paint(Point::ZERO, 1.0).unwrap();
    assert_eq!(list.animations.len(), 1, "{:?}", list.animations);

    // One with an end delay stays with the page, which keeps rendering every frame.
    eval(
        &mut page,
        "document.getElementById('c').animate(
           { transform: ['scale(1)', 'scale(2)'] }, { duration: 1000, iterations: 3, endDelay: 50 });",
    );
    page.update_the_rendering();
    let list = page.engine().paint(Point::ZERO, 1.0).unwrap();
    assert_eq!(list.animations.len(), 1, "{:?}", list.animations);
    assert!(wake_in(&page).is_some_and(|w| w < Duration::from_millis(20)));
}

/// Web Animations 1 §5.7 (side effects of animation): an element whose animation of
/// `opacity` or `transform` is current or in effect acts as if `will-change` named the property,
/// whatever value it shows, as Blink's `ComputedStyle::HasCurrentOpacityAnimation` and
/// `HasCurrentTransformRelatedAnimation` make it a stacking context and, for a transform, a
/// containing block. As WPT's `side-effects-of-animations-*` reftests set them up: a positioned
/// box covers the target, which is above it only as a stacking context (`elementFromPoint`).
#[test]
fn current_animations_act_as_will_change() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        viewport: (800, 600),
        render: Some(RenderSetup::new().unwrap()),
        ..Default::default()
    });
    page.load_html(
        "<!doctype html><style>
           body { margin: 0 }
           .case { position: relative; width: 100px; height: 20px; margin-bottom: 10px }
           .cover { position: absolute; top: 0; left: 0; width: 100%; height: 100% }
           .target { width: 100%; height: 100% }
           .fixed { position: fixed; top: 3px; left: 0; width: 5px; height: 5px }
           #t.go { opacity: .5; transition: opacity 1s 100s }
         </style>
         <div class=case><div class=cover></div><div class=target id=ob></div></div>
         <div class=case><div class=cover></div><div class=target id=oa></div></div>
         <div class=case><div class=cover></div><div class=target id=tb><div class=fixed id=f></div></div></div>
         <div class=case><div class=cover></div><div class=target id=n></div></div>
         <div class=case><div class=cover></div><div class=target id=t></div></div>",
        &url::Url::parse("http://example.test/").unwrap(),
    );
    assert!(page.run_until_settled(Duration::from_secs(10)), "settles");
    eval(
        &mut page,
        "const opts = (fill) => ({ duration: 1e6, delay: 1e6, endDelay: 1e6,
                                    easing: 'steps(1, jump-both)', fill });
         const on = (id, kf, fill, time) => {
           const a = document.getElementById(id).animate([kf, kf], opts(fill));
           a.currentTime = time;
         };
         // current: in the before phase, playing forwards (no value yet)
         on('ob', { opacity: 1 }, 'none', 0.5e6);
         // in effect at 1, the value that makes no stacking context by itself
         on('oa', { opacity: 1 }, 'none', 1.5e6);
         // a transform in the before phase: a containing block for the fixed child too
         on('tb', { transform: 'translateX(0px)' }, 'none', 0.5e6);
         // neither: past its end without a fill
         on('n', { opacity: 1 }, 'none', 2.5e6);
         // a CSS transition from 1 in its delay
         document.getElementById('t').classList.add('go');",
    );
    page.update_the_rendering();
    let top = eval(
        &mut page,
        "['ob', 'oa', 'tb', 'n', 't'].map(id => {
           const r = document.getElementById(id).getBoundingClientRect();
           const e = document.elementFromPoint(r.left + 50, r.top + 10);
           return e.id || e.className;
         }).join(' ')",
    );
    assert_eq!(top, "ob oa tb cover t");
    // (the fixed box is placed in #tb, 60 px down, not in the viewport)
    let fixed = num(
        &mut page,
        "document.getElementById('f').getBoundingClientRect().top",
    );
    assert_eq!(fixed, 63.0);
    // Once the animation is gone, so is the stacking context.
    eval(
        &mut page,
        "document.getElementById('ob').getAnimations()[0].cancel()",
    );
    page.update_the_rendering();
    let top = eval(
        &mut page,
        "const r = document.getElementById('ob').getBoundingClientRect();
         document.elementFromPoint(r.left + 50, r.top + 10).className",
    );
    assert_eq!(top, "cover");
}

/// One frame: a rendering update, then the paint that goes to the compositor (its animations).
fn frame(page: &mut Page) -> Vec<snowlark_paint::layer_animation::LayerAnimation> {
    page.update_the_rendering();
    page.engine().paint(Point::ZERO, 1.0).unwrap().animations
}

/// Whether the compositor, sampling `anims` at the time script sees, shows the opacity the page
/// computes for `#id` then (`Err`: what differs).
fn compositor_agrees(
    page: &mut Page,
    anims: &[snowlark_paint::layer_animation::LayerAnimation],
    id: &str,
) -> Result<(), String> {
    use snowlark_paint::layer_animation::Sample;
    let read = eval(
        page,
        &format!(
            "[document.timeline.currentTime,
              getComputedStyle(document.getElementById('{id}')).opacity].join(' ')"
        ),
    );
    let v: Vec<f64> = read.split(' ').map(|x| x.parse().unwrap()).collect();
    if anims.len() != 1 {
        return Err(format!("#{id}: on the compositor: {anims:?}"));
    }
    match anims[0].sample(v[0] / 1000.0) {
        Some(Sample::Opacity(o)) if (o as f64 - v[1]).abs() < 2e-3 => Ok(()),
        s => Err(format!(
            "#{id} at {} ms: the compositor shows {s:?}, the page {}",
            v[0], v[1]
        )),
    }
}

/// S3 (`docs/compositor-animations.md`): a change script or style makes to an animation a
/// compositor runs asks for the next rendering update, whose frame carries the animation as the
/// page now runs it, or without it when the page paints it from then on (Blink's
/// `Animation::SetCompositorPending`, which adds it to `PendingAnimations` and schedules a frame,
/// and `Animation::PreCommit`, which cancels and restarts it on the compositor). Without the update,
/// the compositor kept showing the animation as it was before the change.
#[test]
fn changes_to_compositor_animations_reach_the_next_frame() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        viewport: (800, 600),
        render: Some(RenderSetup::new().unwrap()),
        ..Default::default()
    });
    page.load_html(
        "<!doctype html><style>
           div { position: absolute; width: 40px; height: 30px; background: teal }
           @keyframes fade { from { opacity: 0 } to { opacity: 1 } }
         </style><body>",
        &url::Url::parse("http://example.test/").unwrap(),
    );
    assert!(page.run_until_settled(Duration::from_secs(10)), "settles");
    const LOOP: &str = "{ duration: 1000, iterations: Infinity }";
    const FIVE: &str = "{ duration: 1000, iterations: 5 }";
    // (how the animation starts: a script one, or a CSS one; the change; where the compositor
    // shows it after, `None`: it is gone from the compositor)
    let cases: &[(&str, &str, Option<&str>)] = &[
        (LOOP, "a.currentTime = 250", Some("x")),
        (LOOP, "a.currentTime = 1700", Some("x")),
        (LOOP, "a.playbackRate = 3", Some("x")),
        (LOOP, "a.playbackRate = -2", None),
        (FIVE, "a.playbackRate = -2", Some("x")),
        (
            LOOP,
            "a.startTime = document.timeline.currentTime - 600",
            Some("x"),
        ),
        (LOOP, "a.updatePlaybackRate(.5)", Some("x")),
        (FIVE, "a.reverse()", Some("x")),
        (LOOP, "a.pause()", None),
        (LOOP, "a.cancel()", None),
        (FIVE, "a.finish()", None),
        (LOOP, "a.effect.updateTiming({ duration: 400 })", Some("x")),
        (LOOP, "a.effect.updateTiming({ delay: -300 })", Some("x")),
        (LOOP, "a.effect.updateTiming({ endDelay: 10 })", None),
        (
            LOOP,
            "a.effect.setKeyframes({ opacity: [1, .2] })",
            Some("x"),
        ),
        (
            LOOP,
            "a.effect.target = document.getElementById('y' + i)",
            Some("y"),
        ),
        (
            LOOP,
            "a.effect = new KeyframeEffect(a.effect.target, { opacity: [.3, .6] }, 700)",
            Some("x"),
        ),
        (LOOP, "a.effect = null", None),
        (LOOP, "a.timeline = null", None),
        ("css", "x.style.animationPlayState = 'paused'", None),
        ("css", "x.style.animationDuration = '400ms'", Some("x")),
        ("css", "x.style.animationDelay = '-300ms'", Some("x")),
        ("css", "x.style.animation = 'none'", None),
        ("css", "a.currentTime = 250", None),
        ("css", "a.cancel()", None),
        // (CSS Cascade 5 §6.1: an important declaration beats animations)
        (
            LOOP,
            "x.style.setProperty('opacity', '.5', 'important')",
            None,
        ),
        (
            "css",
            "x.style.setProperty('opacity', '.5', 'important')",
            None,
        ),
        // (a transition sits above important declarations: it stays)
        (
            "transition",
            "x.style.setProperty('opacity', '.2', 'important')",
            Some("x"),
        ),
    ];
    let mut failed = Vec::new();
    for (i, &(start, change, after)) in cases.iter().enumerate() {
        let what = format!("{start} then `{change}`");
        let make = if start == "css" {
            "x.style.animation = 'fade 1s linear infinite'; window.a = x.getAnimations()[0];"
                .to_string()
        } else if start == "transition" {
            "x.style.transition = 'opacity 100s linear'; getComputedStyle(x).opacity;
             x.style.opacity = '.2'; window.a = x.getAnimations()[0];"
                .to_string()
        } else {
            format!("window.a = x.animate({{ opacity: [0, 1] }}, {start});")
        };
        eval(
            &mut page,
            &format!(
                "window.i = {i};
                 document.body.insertAdjacentHTML('beforeend',
                   '<div id=x{i} style=\"left: 10px\"></div><div id=y{i} style=\"left: 60px\"></div>');
                 window.x = document.getElementById('x{i}'); {make}"
            ),
        );
        frame(&mut page);
        let anims = frame(&mut page);
        compositor_agrees(&mut page, &anims, &format!("x{i}")).unwrap();
        assert!(
            wake_in(&page).is_none_or(|w| w > Duration::from_millis(100)),
            "{what}: the page ticks for an animation the compositor runs"
        );
        std::thread::sleep(Duration::from_millis(40));
        eval(&mut page, change);
        if !wake_in(&page).is_some_and(|w| w < Duration::from_millis(20)) {
            failed.push(format!(
                "{what}: no rendering update is due after the change"
            ));
        }
        let anims = frame(&mut page);
        let shown = match after {
            Some(id) => compositor_agrees(&mut page, &anims, &format!("{id}{i}")),
            None if anims.is_empty() => Ok(()),
            None => Err(format!("still on the compositor: {anims:?}")),
        };
        if let Err(e) = shown {
            failed.push(format!("{what}: {e}"));
        }
        // (a looping one the compositor still runs asks for no update after that frame; a style
        // change that retimed a CSS animation first has the event scan it made due, S2)
        if start != LOOP && start != FIVE && after.is_some() {
            frame(&mut page);
        }
        if after.is_some()
            && (start == LOOP || start == "css")
            && !wake_in(&page).is_none_or(|w| w > Duration::from_millis(100))
        {
            failed.push(format!("{what}: the page ticks after the change"));
        }
        // (clean up: the next case starts with no animation)
        eval(
            &mut page,
            "for (const e of document.getAnimations()) e.cancel(); x.remove();
             document.getElementById('y' + i).remove();",
        );
        let anims = frame(&mut page);
        assert!(anims.is_empty(), "{what}: left {anims:?}");
    }
    assert!(failed.is_empty(), "\n{}", failed.join("\n"));
}

/// Web Animations 1 §4.5.3: setting a `CSSAnimation`'s effect replaces what it applies. The CSS
/// keyframes stop applying and the new effect applies (`null`: nothing); the animation's own
/// effect set back brings them back. An opacity animation a compositor could run is replaced too.
#[test]
fn replacing_the_effect_of_a_css_animation_replaces_its_values() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        viewport: (800, 600),
        render: Some(RenderSetup::new().unwrap()),
        ..Default::default()
    });
    page.load_html(
        "<!doctype html><style>
           @keyframes k { from, to { width: 10px } }
           @keyframes f { from, to { opacity: .2 } }
           #t { width: 100px; height: 10px; animation: k 100s }
           #o { width: 10px; height: 10px; background: teal; animation: f 100s }
         </style><div id=t></div><div id=o></div>",
        &url::Url::parse("http://example.test/").unwrap(),
    );
    assert!(page.run_until_settled(Duration::from_secs(10)), "settles");
    page.update_the_rendering();
    page.engine().paint(Point::ZERO, 1.0).unwrap();
    let width = |page: &mut Page, js: &str| {
        eval(
            page,
            &format!("(() => {{ const t = document.getElementById('t'); {js}; return getComputedStyle(t).width }})()"),
        )
    };
    assert_eq!(
        width(
            &mut page,
            "window.a = t.getAnimations()[0]; window.own = a.effect"
        ),
        "10px"
    );
    let replaced = "a.effect = new KeyframeEffect(t, [{ width: '50px' }, { width: '50px' }], \
                    { duration: 100000 })";
    assert_eq!(width(&mut page, replaced), "50px");
    page.update_the_rendering();
    assert_eq!(width(&mut page, ""), "50px", "after a rendering update");
    assert_eq!(width(&mut page, "a.effect = null"), "100px");
    assert_eq!(width(&mut page, "a.effect = own"), "10px");
    assert_eq!(
        eval(
            &mut page,
            "var o = document.getElementById('o'); var b = o.getAnimations()[0];
             b.effect = new KeyframeEffect(o, [{ opacity: .7 }, { opacity: .7 }],
                                           { duration: 100000 });
             getComputedStyle(o).opacity",
        ),
        "0.7"
    );
}
