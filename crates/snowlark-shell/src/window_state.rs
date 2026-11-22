// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The window's size, position and maximized state, kept in the profile (`window.json`) so the next
//! run of that profile opens where this one was. Read before the window is made, written when the
//! placement changes (at most once a second) and when the window closes; a missing, corrupt or
//! nonsensical file means the default window ([`snowlark_paint::placement::choose`] judges the values
//! against the monitors).

use std::path::Path;

use serde::{Deserialize, Serialize};
use snowlark_paint::placement::Placement;

/// A placement as the profile's files keep it (`window.json`, and each window of the session
/// file).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedPlacement {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    maximized: bool,
    dpi: u32,
}

impl From<Placement> for SavedPlacement {
    fn from(p: Placement) -> SavedPlacement {
        SavedPlacement {
            x: p.x,
            y: p.y,
            width: p.width,
            height: p.height,
            maximized: p.maximized,
            dpi: p.dpi,
        }
    }
}

impl From<SavedPlacement> for Placement {
    fn from(f: SavedPlacement) -> Placement {
        Placement {
            x: f.x,
            y: f.y,
            width: f.width,
            height: f.height,
            maximized: f.maximized,
            dpi: f.dpi,
        }
    }
}

/// The saved placement; `None` when there is no usable file.
pub fn load(file: &Path) -> Option<Placement> {
    let f: SavedPlacement = serde_json::from_str(&std::fs::read_to_string(file).ok()?).ok()?;
    Some(f.into())
}

/// Where a new regular window opens beside `from`, an open one: its restored rectangle moved
/// right and down by ten device-independent pixels, maximized if it is.
pub fn beside(from: &Placement) -> Placement {
    let step = (TILE * from.dpi as i32 / 96).max(1);
    Placement {
        x: from.x + step,
        y: from.y + step,
        ..*from
    }
}

/// How far a new window is set off from the one it opens beside (device-independent pixels).
const TILE: i32 = 10;

/// Writes the placement (a temporary file renamed over the old one: a kill never leaves half a file).
pub fn save(file: &Path, p: &Placement) -> std::io::Result<()> {
    let f = SavedPlacement::from(*p);
    let text = serde_json::to_string(&f).unwrap_or_default();
    crate::restore::write_atomic(file, text.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> std::path::PathBuf {
        let d =
            std::env::temp_dir().join(format!("snowlark-winstate-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_placement_round_trips_and_profiles_stay_apart() {
        let (a, b) = (dir("a"), dir("b"));
        let p = Placement {
            x: -1200,
            y: 40,
            width: 1000,
            height: 700,
            maximized: true,
            dpi: 144,
        };
        save(&a.join("window.json"), &p).unwrap();
        assert_eq!(load(&a.join("window.json")), Some(p));
        // Another profile has its own file: nothing there.
        assert_eq!(load(&b.join("window.json")), None);
        let _ = std::fs::remove_dir_all(&a);
        let _ = std::fs::remove_dir_all(&b);
    }

    #[test]
    fn a_missing_or_damaged_file_is_no_placement() {
        let d = dir("bad");
        let f = d.join("window.json");
        assert_eq!(load(&f), None);
        for text in [
            "",
            "{nope",
            "[1,2]",
            "{\"x\":1}",
            "{\"x\":\"a\",\"y\":0,\"width\":1,\"height\":1,\"maximized\":false,\"dpi\":96}",
        ] {
            std::fs::write(&f, text).unwrap();
            assert_eq!(load(&f), None, "{text}");
        }
        let _ = std::fs::remove_dir_all(&d);
    }
}
