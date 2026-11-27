// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The desktop's light or dark preference: the desktop portal's `org.freedesktop.appearance`
//! `color-scheme` (the Settings portal: 1 prefers dark, 2 prefers light, 0 says nothing), read once
//! and then followed through its `SettingChanged` signal on a thread of its own, which wakes the
//! window loop to tell its windows ([`super::Event::ThemeChanged`]). Without a portal (or one that
//! says nothing), GTK's own setting: `GTK_THEME` naming a dark variant, else
//! `gtk-application-prefer-dark-theme` or a `-dark` theme name in GTK 4's or GTK 3's
//! `settings.ini`.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Once;
use std::time::Duration;

use dbus::arg::{RefArg, Variant};
use dbus::blocking::Connection;
use dbus::message::MatchRule;
use sctk::reexports::calloop::ping::Ping;
use smithay_client_toolkit as sctk;

use crate::desktop_bus;

const PORTAL: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const SETTINGS: &str = "org.freedesktop.portal.Settings";
const NAMESPACE: &str = "org.freedesktop.appearance";
const KEY: &str = "color-scheme";

/// 0: not read yet; 1: light; 2: dark.
static DARK: AtomicU8 = AtomicU8::new(0);

/// Whether the desktop prefers dark.
pub fn prefers_dark() -> bool {
    match DARK.load(Ordering::Relaxed) {
        0 => {
            let dark = read();
            DARK.store(if dark { 2 } else { 1 }, Ordering::Relaxed);
            dark
        }
        v => v == 2,
    }
}

/// The portal's `color-scheme`, if it says one: 1 dark, 2 light.
fn portal_scheme(conn: &Connection) -> Option<u32> {
    let proxy = conn.with_proxy(PORTAL, PORTAL_PATH, desktop_bus::CALL_TIMEOUT);
    let one: Result<(Variant<Box<dyn RefArg>>,), _> =
        proxy.method_call(SETTINGS, "ReadOne", (NAMESPACE, KEY));
    // (portals before version 2 have only `Read`, which wraps the value in one more variant)
    let value = match one {
        Ok((v,)) => v,
        Err(_) => {
            let (v,): (Variant<Box<dyn RefArg>>,) =
                proxy.method_call(SETTINGS, "Read", (NAMESPACE, KEY)).ok()?;
            v
        }
    };
    scheme_of(&value.0)
}

/// The number in a value, through any variants around it.
fn scheme_of(v: &dyn RefArg) -> Option<u32> {
    if let Some(n) = v.as_u64() {
        return u32::try_from(n).ok();
    }
    let mut inner = v.as_iter()?;
    scheme_of(inner.next()?)
}

fn read() -> bool {
    let portal = desktop_bus::session().and_then(|c| portal_scheme(&c));
    match portal {
        Some(1) => true,
        Some(2) => false,
        _ => gtk_prefers_dark(),
    }
}

/// GTK's own setting ([module docs](self)).
fn gtk_prefers_dark() -> bool {
    if let Some(theme) = std::env::var_os("GTK_THEME") {
        return dark_theme_name(&theme.to_string_lossy());
    }
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::Path::new(&h).join(".config")));
    let Some(config) = config else {
        return false;
    };
    ["gtk-4.0", "gtk-3.0"].iter().any(|v| {
        std::fs::read_to_string(config.join(v).join("settings.ini"))
            .is_ok_and(|ini| settings_prefer_dark(&ini))
    })
}

/// A GTK theme name (`Adwaita-dark`, or `GTK_THEME`'s `name:variant`) that is a dark one.
fn dark_theme_name(name: &str) -> bool {
    let name = name.trim().to_ascii_lowercase();
    name.ends_with(":dark") || name.ends_with("-dark")
}

/// Whether a GTK `settings.ini` asks for dark.
fn settings_prefer_dark(ini: &str) -> bool {
    ini.lines().any(|line| {
        let Some((key, value)) = line.split_once('=') else {
            return false;
        };
        let value = value.trim();
        match key.trim() {
            "gtk-application-prefer-dark-theme" => {
                value == "1" || value.eq_ignore_ascii_case("true")
            }
            "gtk-theme-name" => dark_theme_name(value),
            _ => false,
        }
    })
}

/// Follows the portal's setting on a thread of its own (once a process), pinging `changed` when
/// it changes.
pub fn watch(changed: Ping) {
    static STARTED: Once = Once::new();
    STARTED.call_once(|| {
        let _ = std::thread::Builder::new()
            .name("appearance".into())
            .spawn(move || follow(changed));
    });
}

fn follow(changed: Ping) {
    let Some(conn) = desktop_bus::session() else {
        return;
    };
    let rule = MatchRule::new_signal(SETTINGS, "SettingChanged").with_path(PORTAL_PATH);
    let ping = changed.clone();
    let added = conn.add_match(
        rule,
        move |(ns, key, value): (String, String, Variant<Box<dyn RefArg>>), _: &Connection, _| {
            if ns == NAMESPACE && key == KEY {
                let dark = scheme_of(&value.0) == Some(1)
                    || (scheme_of(&value.0) != Some(2) && gtk_prefers_dark());
                let now = if dark { 2 } else { 1 };
                if DARK.swap(now, Ordering::Relaxed) != now {
                    ping.ping();
                }
            }
            true
        },
    );
    if added.is_err() {
        return;
    }
    while conn.process(Duration::from_secs(3600)).is_ok() {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gtk_settings_that_ask_for_dark() {
        assert!(settings_prefer_dark(
            "[Settings]\ngtk-application-prefer-dark-theme=1\n"
        ));
        assert!(settings_prefer_dark(
            "[Settings]\ngtk-application-prefer-dark-theme = true\n"
        ));
        assert!(settings_prefer_dark(
            "[Settings]\ngtk-theme-name=Adwaita-dark\n"
        ));
        assert!(!settings_prefer_dark(
            "[Settings]\ngtk-theme-name=Adwaita\ngtk-application-prefer-dark-theme=0\n"
        ));
        assert!(dark_theme_name("Adwaita:dark"));
        assert!(!dark_theme_name("Adwaita"));
    }

    /// Against a running desktop portal whose setting `gsettings` changes
    /// (`SNOWLARK_TEST_PORTAL=1`, GNOME's `color-scheme` at `default` to begin with): the setting
    /// is read, and a change of it wakes the loop with the new one.
    #[test]
    fn the_portals_setting_is_read_and_followed() {
        use sctk::reexports::calloop::{ping::make_ping, EventLoop};
        if std::env::var_os("SNOWLARK_TEST_PORTAL").is_none() {
            return;
        }
        let gsettings = |v: &str| {
            std::process::Command::new("gsettings")
                .args(["set", "org.gnome.desktop.interface", "color-scheme", v])
                .status()
                .unwrap()
        };
        assert!(!prefers_dark());
        let mut lp: EventLoop<bool> = EventLoop::try_new().unwrap();
        let (ping, source) = make_ping().unwrap();
        lp.handle()
            .insert_source(source, |_, _, told: &mut bool| *told = true)
            .unwrap();
        watch(ping);
        std::thread::sleep(Duration::from_millis(500));
        gsettings("prefer-dark");
        let mut told = false;
        let end = std::time::Instant::now() + Duration::from_secs(10);
        while !told && std::time::Instant::now() < end {
            lp.dispatch(Duration::from_millis(100), &mut told).unwrap();
        }
        assert!(told, "the change woke the loop");
        assert!(prefers_dark());
        gsettings("default");
    }

    #[test]
    fn the_scheme_is_found_through_variants() {
        let plain: Box<dyn RefArg> = Box::new(1u32);
        assert_eq!(scheme_of(&plain), Some(1));
        let wrapped: Box<dyn RefArg> = Box::new(Variant(Box::new(2u32) as Box<dyn RefArg>));
        assert_eq!(scheme_of(&wrapped), Some(2));
    }
}
