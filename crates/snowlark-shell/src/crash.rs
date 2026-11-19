// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Crashes: the process's panic hook (every panic goes to `crash.log` in the profile folder, with its
//! backtrace, before anything else happens) and the page the window shows for a crashed tab (what
//! happened, the panic message, a Reload button), drawn by the window itself: nothing of a crashed
//! tab's own output is shown or trusted.

use std::io::Write;
use std::path::{Path, PathBuf};

use snowlark_paint::{Color, DisplayListBuilder, Point, Radii, Rect, Size};

use crate::chrome::{glyph, Fonts, Theme};

/// `crash.log` stops growing past this; the old log is kept as `crash.log.old`.
const LOG_LIMIT: u64 = 1 << 20;

/// Installs the process's panic hook: each panic's thread, location, message and backtrace are
/// appended to `log`, then the previous hook runs (the default one prints to stderr).
pub fn install_panic_hook(log: PathBuf) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        let message = snowlark_js::guard::panic_message(info.payload());
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_default();
        let backtrace = std::backtrace::Backtrace::force_capture();
        let entry = format!(
            "=== {} thread '{}' panicked at {location}:\n{message}\n{backtrace}\n",
            utc_now(),
            thread.name().unwrap_or("<unnamed>"),
        );
        // (a log that cannot be written must not stop the panic)
        let _ = append_log(&log, &entry);
        previous(info);
    }));
}

fn append_log(log: &Path, entry: &str) -> std::io::Result<()> {
    if let Some(dir) = log.parent() {
        std::fs::create_dir_all(dir)?;
    }
    if std::fs::metadata(log).is_ok_and(|m| m.len() > LOG_LIMIT) {
        let _ = std::fs::rename(log, log.with_extension("log.old"));
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)?;
    f.write_all(entry.as_bytes())
}

/// The current time as `YYYY-MM-DD hh:mm:ss UTC`.
fn utc_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    utc(secs)
}

fn utc(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // (days since 1970-01-01 to a civil date: H. Hinnant's algorithm)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} UTC",
        rem / 3600,
        rem / 60 % 60,
        rem % 60
    )
}

/// What the window shows in place of a crashed tab's page.
#[derive(Clone, Debug, PartialEq)]
pub struct SadTab {
    /// The panic message (from the tab's thread: shown as text only).
    pub message: String,
}

const COLUMN: f32 = 560.0;
const BUTTON: Size = Size::new(96.0, 36.0);

/// Where the page's parts go in a content area of `area` DIPs: (left of the text column, top of the
/// icon, Reload button).
fn layout(area: Size) -> (f32, f32, Rect) {
    let width = COLUMN.min(area.width - 48.0).max(120.0);
    let x = ((area.width - width) / 2.0).max(24.0).round();
    let top = ((area.height - 300.0) * 0.4).max(24.0).round();
    let button = Rect::new(x, top + 248.0, BUTTON.width, BUTTON.height);
    (x, top, button)
}

impl SadTab {
    /// The Reload button (content-area DIPs).
    pub fn reload_button(area: Size) -> Rect {
        layout(area).2
    }

    /// Draws the page over the whole content area.
    pub fn paint(
        &self,
        b: &mut DisplayListBuilder,
        area: Size,
        fonts: &Fonts,
        t: &Theme,
        button_hover: bool,
    ) {
        let bg = if t.dark {
            Color::hex(0x202124)
        } else {
            Color::WHITE
        };
        b.fill_rect(Rect::new(0.0, 0.0, area.width, area.height), bg);
        let (x, top, button) = layout(area);
        let width = COLUMN.min(area.width - 48.0).max(120.0);
        fonts.icon(
            b,
            glyph::SAD,
            Point::new(x + 24.0, top + 24.0),
            48.0,
            t.text_dim,
        );
        fonts.label(b, "Aw, Snap!", x, top + 88.0, 26.0, t.text, width);
        fonts.label(
            b,
            "Something went wrong while displaying this webpage.",
            x,
            top + 128.0,
            15.0,
            t.text,
            width,
        );
        fonts.label(
            b,
            "Snowlark stopped this tab after an internal error. Your other tabs are not affected.",
            x,
            top + 154.0,
            13.0,
            t.text_dim,
            width,
        );
        // The message: its first lines, each cut to the column (the third is a renderer's crash
        // dump, when one was written).
        let mut y = top + 184.0;
        for line in self
            .message
            .lines()
            .filter(|l| !l.trim().is_empty())
            .take(3)
        {
            fonts.label(b, line, x, y, 12.0, t.text_dim, width);
            y += 18.0;
        }
        let fill = if button_hover {
            t.accent.with_alpha(220)
        } else {
            t.accent
        };
        b.fill_rounded_rect(button, Radii::uniform(BUTTON.height / 2.0), fill);
        let label = "Reload";
        let w = fonts.width(label, 14.0);
        let on_accent = if t.dark {
            Color::hex(0x202124)
        } else {
            Color::WHITE
        };
        fonts.label(
            b,
            label,
            (button.x + (button.width - w) / 2.0).round(),
            button.y + button.height / 2.0,
            14.0,
            on_accent,
            button.width,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_dates() {
        assert_eq!(utc(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(utc(951_782_400), "2000-02-29 00:00:00 UTC");
        assert_eq!(utc(1_793_448_000), "2026-10-31 12:00:00 UTC");
    }

    #[test]
    fn the_log_appends_and_rolls_over() {
        let dir = std::env::temp_dir().join(format!("snowlark-crashlog-{}", std::process::id()));
        let log = dir.join("crash.log");
        let _ = std::fs::remove_dir_all(&dir);
        append_log(&log, "one\n").unwrap();
        append_log(&log, "two\n").unwrap();
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "one\ntwo\n");
        let big = "x".repeat(LOG_LIMIT as usize + 1);
        append_log(&log, &big).unwrap();
        append_log(&log, "three\n").unwrap();
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "three\n");
        assert!(dir.join("crash.log.old").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_reload_button_is_inside_the_area() {
        for area in [Size::new(1200.0, 800.0), Size::new(300.0, 200.0)] {
            let r = SadTab::reload_button(area);
            assert!(r.x >= 0.0 && r.right() <= area.width.max(r.right()));
            assert!(r.width > 0.0 && r.height > 0.0);
        }
    }
}
