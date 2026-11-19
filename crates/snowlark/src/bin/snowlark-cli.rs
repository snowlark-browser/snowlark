// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `snowlark-cli`: the same program as `snowlark`, built for the console subsystem. Shells wait for it,
//! and PowerShell captures and redirects its output (`$x = snowlark-cli --eval 1+1`, `> out.txt`),
//! which it does not do for a Windows-subsystem program like `snowlark.exe`. Use it in scripts; the
//! WPT runner and other process APIs can use either.

// (main.rs's `windows_subsystem` attribute only applies to the binary whose root it is)
#[allow(unused_attributes)]
#[path = "../main.rs"]
mod snowlark;

fn main() {
    snowlark::main()
}
