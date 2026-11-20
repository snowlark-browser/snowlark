// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Snowlark: the browser binary. `snowlark` (or `snowlark <url>`) opens the browser window; the `--` modes
//! are headless (`snowlark --eval "<js>"` prints the result).
//!
//! It is a Windows-subsystem program, so the window comes without a console window. The command-line
//! modes attach to the console they were started from (or keep the pipes and files they were given),
//! so their output still reaches it. Like any GUI-subsystem program, a console shell doesn't wait
//! for it: cmd and PowerShell show their prompt at once (output follows, and `%ERRORLEVEL%` /
//! `$LASTEXITCODE` are not set) unless the output is piped, or the caller waits (`start /wait`,
//! `Start-Process -Wait`, `| Out-Host`, any process API). PowerShell also doesn't capture or
//! redirect a GUI program's output (`$x = snowlark --eval 1`, `> file`): scripts use `snowlark-cli.exe`,
//! the console-subsystem build of this same file (`src/bin/snowlark-cli.rs`).
#![windows_subsystem = "windows"]

// Rust's allocations go to mimalloc rather than the Windows heap, which was 9 to 14% of page work,
// through its plain entry points where they give the alignment (`snowlark_js::rust_heap`). The C
// runtime's heap goes to mimalloc too (`snowlark_js::crt_heap`).
#[global_allocator]
static ALLOCATOR: snowlark_js::rust_heap::Allocator = snowlark_js::rust_heap::Allocator;

pub fn main() {
    let mut args: Vec<String> = std::env::args().collect();
    // The window's process model: `--process-model=tab|single`, else `SNOWLARK_PROCESS_MODEL`.
    let model = match take_process_model(&mut args) {
        Ok(m) => m,
        Err(bad) => {
            snowlark_paint::window::attach_parent_console();
            eprintln!("--process-model takes tab or single, not {bad:?}");
            std::process::exit(2);
        }
    };
    // `--gpu-process`: the window composites in a GPU process (with `--process-model=tab`).
    let gpu_process = take_flag(&mut args, "--gpu-process");
    // `--incognito`: the window opens as an Incognito window.
    let flags = snowlark_shell::WindowFlags {
        gpu_process,
        incognito: take_flag(&mut args, "--incognito"),
    };
    // The headless modes run their page on a thread with a page thread's stack (layout recurses
    // over the box tree, and the main thread's stack is 1 MiB on Windows); the window runs its pages
    // on tab threads of that size.
    if args.get(1).is_some_and(|a| a.starts_with("--")) {
        let code = std::thread::Builder::new()
            .name("snowlark main".into())
            .stack_size(snowlark_page::PAGE_THREAD_STACK)
            .spawn(move || run(args, model, flags))
            .expect("the main thread")
            .join()
            .unwrap_or(101);
        snowlark_js::write_pgo_profile();
        std::process::exit(code);
    }
    let code = run(args, model, flags);
    snowlark_js::write_pgo_profile();
    std::process::exit(code);
}

/// Takes `flag` out of `args`: whether it was there.
fn take_flag(args: &mut Vec<String>, flag: &str) -> bool {
    match args.iter().position(|a| a == flag) {
        Some(i) => {
            args.remove(i);
            true
        }
        None => false,
    }
}

/// Takes `--process-model=<model>` out of `args`; without it, `SNOWLARK_PROCESS_MODEL` says, and
/// without that, the default (`snowlark_shell::ProcessModel::choose`). An unknown model is the error.
/// Only the window has a process model: the headless modes run their page in this process.
fn take_process_model(args: &mut Vec<String>) -> Result<snowlark_shell::ProcessModel, String> {
    const FLAG: &str = "--process-model=";
    let flag = args
        .iter()
        .position(|a| a.starts_with(FLAG))
        .map(|i| args.remove(i)[FLAG.len()..].to_owned());
    let env = std::env::var("SNOWLARK_PROCESS_MODEL").ok();
    snowlark_shell::ProcessModel::choose(flag.as_deref(), env.as_deref())
}

fn run(
    args: Vec<String>,
    model: snowlark_shell::ProcessModel,
    flags: snowlark_shell::WindowFlags,
) -> i32 {
    let window = match args.get(1).map(String::as_str) {
        None => true,
        Some(a) => !a.starts_with("--"),
    };
    // The window mode prints only diagnostics (SNOWLARK_GPU_STATS, SNOWLARK_TRACE): those go to the
    // console too when it was started from one.
    if !window
        || std::env::var_os("SNOWLARK_GPU_STATS").is_some()
        || std::env::var_os("SNOWLARK_TRACE").is_some()
    {
        snowlark_paint::window::attach_parent_console();
    }
    match args.get(1).map(String::as_str) {
        Some("--eval") => {
            #[path = "cli/eval.rs"]
            mod eval;
            eval::run(&args[1..])
        }
        Some("--wpt") => {
            #[path = "cli/wpt.rs"]
            mod wpt;
            wpt::run(&args)
        }
        Some("--speedometer") => {
            #[path = "cli/speedometer.rs"]
            mod speedometer;
            speedometer::run(&args[1..])
        }
        Some("--reftest") => {
            #[path = "cli/reftest.rs"]
            mod reftest;
            reftest::run(&args)
        }
        Some("--render-demo") => {
            #[path = "cli/render_demo.rs"]
            mod render_demo;
            render_demo::run(&args[2..])
        }
        Some("--fetch" | "--fetch-trace") => {
            #[path = "cli/fetch.rs"]
            mod fetch;
            fetch::run(&args)
        }
        Some("--computed") => {
            #[path = "cli/computed.rs"]
            mod computed;
            computed::run(&args[2..])
        }
        Some("--layout-demo") => {
            #[path = "cli/layout_demo.rs"]
            mod layout_demo;
            layout_demo::run(&args[2..])
        }
        Some("--update-filters") => {
            #[path = "cli/blocker.rs"]
            mod blocker;
            std::process::exit(blocker::run())
        }
        Some("--render") => {
            #[path = "cli/render.rs"]
            mod render;
            render::run(&args[2..])
        }
        Some("--bench") => {
            #[path = "cli/bench.rs"]
            mod bench;
            bench::run(&args[2..])
        }
        Some("--video") => {
            #[path = "cli/video.rs"]
            mod video;
            video::run(&args[2..])
        }
        Some("--type=renderer") => {
            #[path = "cli/renderer.rs"]
            mod renderer;
            renderer::run()
        }
        Some("--type=gpu") => {
            #[path = "cli/gpu.rs"]
            mod gpu;
            gpu::run()
        }
        Some("--media-types") => {
            #[path = "cli/media_types.rs"]
            mod media_types;
            media_types::run()
        }
        // The browser window: no arguments (the home page) or an address.
        None => snowlark_shell::run(None, model, flags),
        Some(a) if !a.starts_with("--") => snowlark_shell::run(Some(a), model, flags),
        _ => {
            eprintln!("usage: snowlark [--process-model=tab|single] [--gpu-process] [--incognito] [<url>] | --eval <js> | --render <url> <png> [width] | --reftest <list> <outdir> | --video <url> <png> [--time T] [--wait S] | --media-types");
            2
        }
    }
}
