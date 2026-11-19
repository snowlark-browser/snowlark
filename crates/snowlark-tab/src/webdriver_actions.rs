// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The WPT runners' input: WebDriver "Actions" (W3C WebDriver §15) and testdriver's `click` and
//! `send_keys`, fed to the page's input entry points, the ones the window's input reaches (hit
//! testing, pointer and mouse events, wheel events, key events and their default actions).
//!
//! The runner's testdriver backend (`testdriver-vendor.js`) resolves element origins to viewport
//! points and leaves a plan in the page's `__snowlarkWptActions`, one instruction per line:
//!
//! - `tick <ms>`: a new tick, lasting at least `<ms>` and at least one frame;
//! - `move <x> <y> <ms> <v|p>`: a pointer move to a viewport point (`p`: relative to the pointer),
//!   over `<ms>` with a move each frame;
//! - `down <button>`, `up <button>`: a pointer button (0 primary, 1 middle, 2 secondary, 3 back,
//!   4 forward);
//! - `kdown <code point>`, `kup <code point>`: a key, by WebDriver's key value;
//! - `scroll <x> <y> <dx> <dy>`: a wheel turn at a viewport point.
//!
//! The player takes the plan between event loop turns, runs its ticks as their times come, and
//! then calls the page's `__snowlarkWptActionsDone`, with an error message if the plan was refused.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use snowlark_page::input::Modifiers;
use snowlark_page::keyboard::KeyInput;
use snowlark_page::Page;
use snowlark_paint::Point;

/// A tick lasts at least one frame, as the page sees input at most once per rendering opportunity.
const FRAME: Duration = Duration::from_millis(16);

/// Presses of one button within this time and distance count as one multi-click.
const MULTI_CLICK_TIME: Duration = Duration::from_millis(500);
const MULTI_CLICK_RANGE: f32 = 2.0;

#[derive(Clone, Debug, PartialEq)]
enum Step {
    Move {
        x: f32,
        y: f32,
        ms: f64,
        relative: bool,
    },
    Down(u16),
    Up(u16),
    KeyDown(u32),
    KeyUp(u32),
    Scroll {
        x: f32,
        y: f32,
        dx: f32,
        dy: f32,
    },
}

#[derive(Debug, Default, PartialEq)]
struct Tick {
    ms: f64,
    steps: Vec<Step>,
}

fn parse(plan: &str) -> Result<VecDeque<Tick>, String> {
    let mut ticks = VecDeque::new();
    for line in plan.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let mut words = line.split_ascii_whitespace();
        let op = words.next().unwrap_or_default();
        let args: Vec<&str> = words.collect();
        let n = |i: usize| -> Result<f64, String> {
            args.get(i)
                .and_then(|a| a.parse::<f64>().ok())
                .filter(|v| v.is_finite())
                .ok_or_else(|| format!("bad action line: {line}"))
        };
        if op == "tick" {
            ticks.push_back(Tick {
                ms: n(0)?.max(0.0),
                steps: Vec::new(),
            });
            continue;
        }
        let step = match op {
            "move" => Step::Move {
                x: n(0)? as f32,
                y: n(1)? as f32,
                ms: n(2)?.max(0.0),
                relative: args.get(3) == Some(&"p"),
            },
            "down" => Step::Down(n(0)? as u16),
            "up" => Step::Up(n(0)? as u16),
            "kdown" => Step::KeyDown(n(0)? as u32),
            "kup" => Step::KeyUp(n(0)? as u32),
            "scroll" => Step::Scroll {
                x: n(0)? as f32,
                y: n(1)? as f32,
                dx: n(2)? as f32,
                dy: n(3)? as f32,
            },
            _ => return Err(format!("bad action line: {line}")),
        };
        match ticks.back_mut() {
            Some(t) => t.steps.push(step),
            None => return Err("an action before the first tick".into()),
        }
    }
    Ok(ticks)
}

/// A key on a US keyboard: Windows virtual-key code, scan code (`0x100` for extended keys), the
/// character it types and the one it types with Shift.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Key {
    vk: u16,
    scan: u16,
    text: Option<char>,
    shifted: Option<char>,
}

const fn named(vk: u16, scan: u16) -> Key {
    Key {
        vk,
        scan,
        text: None,
        shifted: None,
    }
}

const fn typed(vk: u16, scan: u16, text: char, shifted: char) -> Key {
    Key {
        vk,
        scan,
        text: Some(text),
        shifted: Some(shifted),
    }
}

/// WebDriver's key value (a character, or a code point of its table at U+E000..U+E05D) as a key;
/// `None` for a code point of the private use area that it does not name.
fn key_for(cp: u32) -> Option<Key> {
    const NUMPAD_SCAN: [u16; 10] = [0x52, 0x4F, 0x50, 0x51, 0x4B, 0x4C, 0x4D, 0x47, 0x48, 0x49];
    let key = match cp {
        0xE000 => named(0, 0),
        0xE001 => named(0x03, 0x146),
        0xE002 => named(0x2F, 0),
        0xE003 => named(0x08, 0x0E),
        0xE004 => named(0x09, 0x0F),
        0xE005 => named(0x0C, 0x4C),
        0xE006 => named(0x0D, 0x1C),
        0xE007 => named(0x0D, 0x11C),
        0xE008 => named(0x10, 0x2A),
        0xE009 => named(0x11, 0x1D),
        0xE00A => named(0x12, 0x38),
        0xE00B => named(0x13, 0x45),
        0xE00C => named(0x1B, 0x01),
        0xE00D => typed(0x20, 0x39, ' ', ' '),
        0xE00E => named(0x21, 0x149),
        0xE00F => named(0x22, 0x151),
        0xE010 => named(0x23, 0x14F),
        0xE011 => named(0x24, 0x147),
        0xE012 => named(0x25, 0x14B),
        0xE013 => named(0x26, 0x148),
        0xE014 => named(0x27, 0x14D),
        0xE015 => named(0x28, 0x150),
        0xE016 => named(0x2D, 0x152),
        0xE017 => named(0x2E, 0x153),
        0xE018 => typed(0xBA, 0x27, ';', ':'),
        0xE019 => typed(0xBB, 0x0D, '=', '+'),
        0xE01A..=0xE023 => {
            let n = (cp - 0xE01A) as u16;
            let c = char::from_digit(n as u32, 10)?;
            typed(0x60 + n, NUMPAD_SCAN[n as usize], c, c)
        }
        0xE024 => typed(0x6A, 0x37, '*', '*'),
        0xE025 => typed(0x6B, 0x4E, '+', '+'),
        0xE026 => typed(0x6C, 0x7E, ',', ','),
        0xE027 => typed(0x6D, 0x4A, '-', '-'),
        0xE028 => typed(0x6E, 0x53, '.', '.'),
        0xE029 => typed(0x6F, 0x135, '/', '/'),
        0xE031..=0xE03C => {
            let n = (cp - 0xE031) as u16;
            named(0x70 + n, if n < 10 { 0x3B + n } else { 0x57 + n - 10 })
        }
        0xE03D => named(0x5B, 0x15B),
        0xE050 => named(0x10, 0x36),
        0xE051 => named(0x11, 0x11D),
        0xE052 => named(0x12, 0x138),
        0xE053 => named(0x5C, 0x15C),
        // (the numeric keypad's navigation keys, Num Lock off)
        0xE054 => named(0x21, 0x49),
        0xE055 => named(0x22, 0x51),
        0xE056 => named(0x23, 0x4F),
        0xE057 => named(0x24, 0x47),
        0xE058 => named(0x25, 0x4B),
        0xE059 => named(0x26, 0x48),
        0xE05A => named(0x27, 0x4D),
        0xE05B => named(0x28, 0x50),
        0xE05C => named(0x2D, 0x52),
        0xE05D => named(0x2E, 0x53),
        0xE02A..=0xE030 | 0xE03E..=0xE04F | 0xE05E..=0xF8FF => return None,
        _ => {
            let c = char::from_u32(cp)?;
            return Some(printable(c));
        }
    };
    Some(key)
}

/// A character as the US keyboard's key that types it (with or without Shift); a character no key
/// types has no key code.
fn printable(c: char) -> Key {
    const ROWS: [(&str, u16); 3] = [("qwertyuiop", 0x10), ("asdfghjkl", 0x1E), ("zxcvbnm", 0x2C)];
    const DIGIT_SHIFTED: &str = ")!@#$%^&*(";
    const OTHERS: [(char, char, u16, u16); 12] = [
        ('`', '~', 0xC0, 0x29),
        ('-', '_', 0xBD, 0x0C),
        ('=', '+', 0xBB, 0x0D),
        ('[', '{', 0xDB, 0x1A),
        (']', '}', 0xDD, 0x1B),
        ('\\', '|', 0xDC, 0x2B),
        (';', ':', 0xBA, 0x27),
        ('\'', '"', 0xDE, 0x28),
        (',', '<', 0xBC, 0x33),
        ('.', '>', 0xBE, 0x34),
        ('/', '?', 0xBF, 0x35),
        (' ', ' ', 0x20, 0x39),
    ];
    let lower = c.to_ascii_lowercase();
    for (row, first) in ROWS {
        if let Some(i) = row.find(lower) {
            let base = typed(
                lower.to_ascii_uppercase() as u16,
                first + i as u16,
                lower,
                lower,
            );
            return Key {
                text: Some(c),
                shifted: Some(lower.to_ascii_uppercase()),
                ..base
            };
        }
    }
    let digit = c
        .to_digit(10)
        .or_else(|| DIGIT_SHIFTED.find(c).map(|i| i as u32));
    if let Some(d) = digit.filter(|_| c.is_ascii()) {
        let base = char::from_digit(d, 10).unwrap_or('0');
        let scan = if d == 0 { 0x0B } else { 0x01 + d as u16 };
        return Key {
            text: Some(c),
            ..typed(
                base as u16,
                scan,
                base,
                DIGIT_SHIFTED.as_bytes()[d as usize] as char,
            )
        };
    }
    for (base, shifted, vk, scan) in OTHERS {
        if c == base || c == shifted {
            return Key {
                text: Some(c),
                ..typed(vk, scan, base, shifted)
            };
        }
    }
    Key {
        vk: 0,
        scan: 0,
        text: Some(c),
        shifted: None,
    }
}

/// A move in progress over a tick's duration.
struct Glide {
    from: Point,
    to: Point,
    over: Duration,
}

struct Running {
    started: Instant,
    ends: Instant,
    glides: Vec<Glide>,
}

/// The input state of the runner's one keyboard and mouse (WebDriver's input state), for one test.
#[derive(Default)]
pub struct Player {
    plan: VecDeque<Tick>,
    running: Option<Running>,
    /// When the next tick may start.
    next: Option<Instant>,
    /// A plan was taken and not yet answered.
    active: bool,
    pointer: Point,
    /// Buttons down, as bits by button number.
    buttons: u32,
    /// Keys down, by WebDriver key value.
    keys: Vec<u32>,
    clicks: u32,
    last_press: Option<(Instant, Point, u16)>,
    /// The test's clipboard (text and markup): what a copy or cut key put there, for a paste key.
    clipboard: (Option<String>, Option<String>),
}

impl Player {
    pub fn new() -> Self {
        Self::default()
    }

    /// When the player next needs to run, if it has work.
    pub fn due(&self) -> Option<Instant> {
        if !self.active {
            return None;
        }
        Some(match &self.running {
            Some(r) if !r.glides.is_empty() => r.ends.min(Instant::now() + FRAME),
            Some(r) => r.ends,
            None => self.next.unwrap_or_else(Instant::now),
        })
    }

    /// Takes a new plan from the page, and runs what is due.
    pub fn poll(&mut self, page: &mut Page) {
        if !self.active {
            let Some(plan) = page.global_string("__snowlarkWptActions") else {
                return;
            };
            let _ = page.eval("delete window.__snowlarkWptActions", "snowlark-testdriver");
            self.active = true;
            match parse(&plan) {
                Ok(ticks) => {
                    self.plan = ticks;
                    self.next = None;
                }
                Err(e) => return self.finish(page, Some(e)),
            }
        }
        let now = Instant::now();
        if let Some(r) = &mut self.running {
            let glides: Vec<Point> = r
                .glides
                .iter()
                .map(|g| {
                    let t = now.duration_since(r.started).as_secs_f32() / g.over.as_secs_f32();
                    let t = t.min(1.0);
                    Point::new(
                        g.from.x + (g.to.x - g.from.x) * t,
                        g.from.y + (g.to.y - g.from.y) * t,
                    )
                })
                .collect();
            let ends = r.ends;
            let done = now >= ends;
            if done {
                // (each move ends at its target)
                let targets: Vec<Point> = r.glides.iter().map(|g| g.to).collect();
                self.running = None;
                for p in targets {
                    self.move_to(page, p, false);
                }
                self.next = Some(ends);
            } else {
                for p in glides {
                    self.move_to(page, p, false);
                }
                return;
            }
        }
        while self.running.is_none() {
            if self.next.is_some_and(|t| now < t) {
                return;
            }
            let Some(tick) = self.plan.pop_front() else {
                return self.finish(page, None);
            };
            if let Err(e) = self.start(page, tick, now) {
                self.plan.clear();
                return self.finish(page, Some(e));
            }
        }
    }

    fn start(&mut self, page: &mut Page, tick: Tick, now: Instant) -> Result<(), String> {
        let mut glides = Vec::new();
        for step in tick.steps {
            match step {
                Step::Move { x, y, ms, relative } => {
                    let to = if relative {
                        Point::new(self.pointer.x + x, self.pointer.y + y)
                    } else {
                        Point::new(x, y)
                    };
                    if ms > 0.0 {
                        glides.push(Glide {
                            from: self.pointer,
                            to,
                            over: Duration::from_secs_f64(ms / 1000.0),
                        });
                    } else {
                        self.move_to(page, to, true);
                    }
                }
                Step::Down(b) => self.press(page, b),
                Step::Up(b) => self.release(page, b),
                Step::KeyDown(cp) => self.key(page, cp, true)?,
                Step::KeyUp(cp) => self.key(page, cp, false)?,
                Step::Scroll { x, y, dx, dy } => self.scroll(page, Point::new(x, y), dx, dy),
            }
        }
        let ends = now + Duration::from_secs_f64(tick.ms / 1000.0).max(FRAME);
        self.running = Some(Running {
            started: now,
            ends,
            glides,
        });
        Ok(())
    }

    fn finish(&mut self, page: &mut Page, error: Option<String>) {
        self.active = false;
        self.running = None;
        self.next = None;
        let arg = error.map_or("null".to_owned(), |e| js_string(&e));
        let _ = page.eval(
            &format!(
                "typeof __snowlarkWptActionsDone === 'function' && __snowlarkWptActionsDone({arg})"
            ),
            "snowlark-testdriver",
        );
    }

    fn mods(&self) -> Modifiers {
        let down = |cps: &[u32]| self.keys.iter().any(|k| cps.contains(k));
        Modifiers {
            ctrl: down(&[0xE009, 0xE051]),
            shift: down(&[0xE008, 0xE050]),
            alt: down(&[0xE00A, 0xE052]),
            meta: down(&[0xE03D, 0xE053]),
        }
    }

    /// A move to `p`: a move the plan asks for is sent even where the pointer already is; the
    /// steps of a move over time only where it changed.
    fn move_to(&mut self, page: &mut Page, p: Point, always: bool) {
        if !always && p == self.pointer {
            return;
        }
        self.pointer = p;
        let mods = self.mods();
        page.pointer_moved(p, mods);
    }

    fn press(&mut self, page: &mut Page, button: u16) {
        let bit = 1u32 << button.min(31);
        if self.buttons & bit != 0 {
            return;
        }
        self.buttons |= bit;
        let now = Instant::now();
        let p = self.pointer;
        self.clicks = match self.last_press {
            Some((at, q, b))
                if self.clicks > 0
                    && b == button
                    && now.duration_since(at) <= MULTI_CLICK_TIME
                    && (p.x - q.x).abs() <= MULTI_CLICK_RANGE
                    && (p.y - q.y).abs() <= MULTI_CLICK_RANGE =>
            {
                self.clicks + 1
            }
            _ => 1,
        };
        self.last_press = Some((now, p, button));
        let mods = self.mods();
        page.pointer_pressed(p, button, mods, self.clicks);
    }

    fn release(&mut self, page: &mut Page, button: u16) {
        let bit = 1u32 << button.min(31);
        if self.buttons & bit == 0 {
            return;
        }
        self.buttons &= !bit;
        let mods = self.mods();
        page.pointer_released(self.pointer, button, mods, self.clicks.max(1));
        // (a context menu nothing canceled: there is no menu to show here)
        let _ = page.engine().take_context_menu();
    }

    fn key(&mut self, page: &mut Page, cp: u32, down: bool) -> Result<(), String> {
        let Some(key) = key_for(cp) else {
            return Err(format!("no key for the code point U+{cp:04X}"));
        };
        let repeat = self.keys.contains(&cp);
        if down {
            if !repeat {
                self.keys.push(cp);
            }
        } else {
            if !repeat {
                return Ok(());
            }
            self.keys.retain(|&k| k != cp);
        }
        let mods = self.mods();
        let text = if mods.shift { key.shifted } else { key.text };
        let paste_key = (mods.ctrl && key.vk == b'V' as u16) || (mods.shift && key.vk == 0x2D);
        let (paste, paste_html) = if down && paste_key {
            self.clipboard.clone()
        } else {
            (None, None)
        };
        let input = KeyInput {
            vk: key.vk,
            scan: key.scan,
            text,
            mods,
            repeat: down && repeat,
            paste,
            paste_html,
        };
        if !down {
            page.key_up(&input);
            return Ok(());
        }
        let out = page.key_down(&input);
        if let Some(copied) = out.copy {
            self.clipboard = (Some(copied), out.copy_html);
        }
        // The character the key types follows its keydown, as on the window's keyboard: not with
        // Ctrl or Alt held, and Enter and Backspace act in their keydown.
        if let Some(c) = text.filter(|c| !c.is_control()) {
            if !mods.ctrl && !mods.alt {
                page.key_char(c, &input);
            }
        }
        Ok(())
    }

    /// A wheel turn: the `wheel` event at the element under the point, then the scroll the window
    /// does without waiting for it: the innermost scroll container under the point that can scroll
    /// by the delta, else the viewport.
    fn scroll(&mut self, page: &mut Page, p: Point, dx: f32, dy: f32) {
        let delta = Point::new(dx, dy);
        let mods = self.mods();
        page.wheel(p, delta, mods);
        let engine = page.engine().clone();
        let at = page
            .eval("scrollX + ' ' + scrollY", "snowlark-testdriver")
            .ok()
            .and_then(|s| {
                let mut it = s.split(' ').map(|v| v.parse::<f32>().ok());
                Some(Point::new(it.next()??, it.next()??))
            })
            .unwrap_or(Point::ZERO);
        let doc_point = Point::new(p.x + at.x, p.y + at.y);
        let (root, body) = {
            let dom = engine.dom();
            let doc = engine.document();
            (
                doc.and_then(|d| dom.document_element(d)),
                doc.and_then(|d| dom.body(d)),
            )
        };
        let mut under: Vec<_> = engine
            .user_scroll_containers()
            .into_iter()
            .chain(engine.frame_scrollers())
            .filter(|(n, rect, scroll, max)| {
                Some(*n) != root
                    && Some(*n) != body
                    && rect.contains(doc_point)
                    && ((dy < 0.0 && scroll.y > 0.0)
                        || (dy > 0.0 && scroll.y < max.y)
                        || (dx < 0.0 && scroll.x > 0.0)
                        || (dx > 0.0 && scroll.x < max.x))
            })
            .collect();
        under.sort_by(|a, b| {
            (a.1.width * a.1.height)
                .partial_cmp(&(b.1.width * b.1.height))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        if let Some((node, ..)) = under.first() {
            engine.scroll_container_by(*node, delta);
        } else {
            let _ = page.eval(&format!("scrollBy({dx}, {dy})"), "snowlark-testdriver");
        }
    }
}

/// `s` as a JavaScript string literal.
fn js_string(s: &str) -> String {
    let mut o = String::from('"');
    for c in s.chars() {
        match c {
            '"' | '\\' => {
                o.push('\\');
                o.push(c);
            }
            c if (c as u32) < 0x20 || c == '\u{2028}' || c == '\u{2029}' => {
                o.push_str(&format!("\\u{:04x}", c as u32))
            }
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

/// [`Page::run_until`] with the player's input fed in between turns, and its times among the
/// event loop's wakeups.
pub fn run_until(
    page: &mut Page,
    deadline: Instant,
    player: &mut Player,
    mut done: impl FnMut(&mut Page) -> bool,
) -> bool {
    loop {
        player.poll(page);
        if done(page) {
            return true;
        }
        let ran = page.turn();
        player.poll(page);
        if done(page) {
            return true;
        }
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        if ran {
            continue;
        }
        let engine = page.engine().clone();
        let due = player.due();
        if due.is_none() && !engine.has_pending_work() && !engine.has_ready_task() {
            return done(page);
        }
        let mut wake = engine
            .next_wakeup()
            .unwrap_or(now + Duration::from_millis(50))
            .min(deadline)
            .min(now + Duration::from_millis(50));
        if let Some(d) = due {
            wake = wake.min(d);
        }
        if wake > now {
            engine.waker().wait_until(wake);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plans_parse_into_ticks() {
        let t = parse("tick 0\nmove 1 2 0 v\ndown 0\nup 0\ntick 30\nkdown 57352\nscroll 5 6 0 100")
            .unwrap();
        assert_eq!(t.len(), 2);
        assert_eq!(t[0].steps.len(), 3);
        assert_eq!(t[1].ms, 30.0);
        assert_eq!(t[1].steps[0], Step::KeyDown(0xE008));
        assert!(parse("down 0").is_err());
        assert!(parse("tick 0\nfly 1").is_err());
    }

    #[test]
    fn keys_follow_webdriver_codes_on_a_us_keyboard() {
        let a = key_for('a' as u32).unwrap();
        assert_eq!(
            (a.vk, a.scan, a.text, a.shifted),
            (0x41, 0x1E, Some('a'), Some('A'))
        );
        let big = key_for('A' as u32).unwrap();
        assert_eq!((big.vk, big.scan, big.text), (0x41, 0x1E, Some('A')));
        let bang = key_for('!' as u32).unwrap();
        assert_eq!((bang.vk, bang.scan, bang.text), (0x31, 0x02, Some('!')));
        let zero = key_for('0' as u32).unwrap();
        assert_eq!((zero.vk, zero.scan, zero.shifted), (0x30, 0x0B, Some(')')));
        assert_eq!(key_for(0xE008).unwrap().vk, 0x10);
        assert_eq!(key_for(0xE014).unwrap().scan, 0x14D);
        assert_eq!(key_for(0xE03C).unwrap().scan, 0x58);
        assert_eq!(key_for(0xE01F).unwrap().text, Some('5'));
        assert!(key_for(0xE0FF).is_none());
        assert_eq!(key_for('é' as u32).unwrap().vk, 0);
    }

    fn page_with(html: &str) -> Page {
        use snowlark_net::{NetConfig, NetThread};
        use snowlark_page::{PageOptions, RenderSetup};
        static NET: std::sync::OnceLock<NetThread> = std::sync::OnceLock::new();
        let net = NET.get_or_init(|| NetThread::start(NetConfig::default()).unwrap());
        let mut page = Page::new(PageOptions {
            net: Some(net.client()),
            viewport: (800, 600),
            render: Some(RenderSetup::new().unwrap()),
            ..Default::default()
        });
        page.load_html(
            html,
            &snowlark_net::Url::parse("http://example.test/").unwrap(),
        );
        assert!(page.run_until_settled(Duration::from_secs(5)), "settles");
        page
    }

    /// Runs `plan` and returns the page's `log` once the plan is answered.
    fn run(page: &mut Page, plan: &str) -> String {
        page.eval(
            &format!(
                "log.length = 0; window.__snowlarkWptActionsDone = e => log.push('done:' + e); \
                 window.__snowlarkWptActions = {}",
                js_string(plan)
            ),
            "test.js",
        )
        .unwrap();
        let mut player = Player::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        let answered = |p: &mut Page| {
            p.eval("String(log.some(l => l.startsWith('done:')))", "test.js")
                .unwrap()
                == "true"
        };
        assert!(run_until(page, deadline, &mut player, answered), "answered");
        page.eval("log.join(' ')", "test.js").unwrap()
    }

    const LOGGED: &str = "<!doctype html><style>body{margin:0}#b{position:absolute;left:100px;\
        top:50px;width:80px;height:40px}#t{position:absolute;left:0;top:200px}</style>\
        <button id=b>b</button><input id=t><script>var log = [];\
        for (const type of ['pointerdown','mousedown','pointerup','mouseup','click','dblclick',\
          'keydown','keypress','keyup','input','wheel'])\
          document.addEventListener(type, e => log.push(type + (e.isTrusted ? '' : '!') + \
            (type === 'click' ? ':' + e.target.id + e.detail : '') + \
            (e.key ? ':' + e.key + (e.shiftKey ? '+S' : '') : '')), true);</script>";

    #[test]
    fn a_pointer_plan_reaches_the_element_under_the_point_as_trusted_events() {
        let mut page = page_with(LOGGED);
        let log = run(
            &mut page,
            "tick 0\nmove 140 70 0 v\ndown 0\nup 0\ntick 0\ndown 0\nup 0",
        );
        assert_eq!(
            log,
            "pointerdown mousedown pointerup mouseup click:b1 \
             pointerdown mousedown pointerup mouseup click:b2 dblclick done:null"
        );
        // a press outside the button clicks the body
        let log = run(&mut page, "tick 600\ntick 0\nmove 10 10 0 v\ndown 0\nup 0");
        assert_eq!(
            log,
            "pointerdown mousedown pointerup mouseup click:1 done:null"
        );
    }

    #[test]
    fn key_plans_type_into_the_focused_control_with_shift_held() {
        let mut page = page_with(LOGGED);
        page.eval("t.focus()", "test.js").unwrap();
        let log = run(
            &mut page,
            "tick 0\nkdown 57352\nkdown 97\nkup 97\nkup 57352",
        );
        assert_eq!(
            log,
            "keydown:Shift+S keydown:A+S keypress:A+S input keyup:A+S keyup:Shift done:null"
        );
        assert_eq!(page.eval("t.value", "test.js").unwrap(), "A");
    }

    #[test]
    fn a_refused_plan_rejects() {
        let mut page = page_with(LOGGED);
        let log = run(&mut page, "tick 0\nkdown 57599");
        assert_eq!(log, "done:no key for the code point U+E0FF");
    }
}
