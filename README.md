# Snowlark

Snowlark is a fast, private web browser for Windows, written in Rust and built from the web's
specifications. More at [snowlark.org](https://snowlark.org).

The DOM, the JavaScript bindings (generated from the specifications' Web IDL), layout, painting, networking,
media and the browser window are Snowlark's own code. JavaScript runs on [V8](https://v8.dev), style on
[Stylo](https://github.com/servo/stylo), HTML parsing on [html5ever](https://github.com/servo/html5ever), flex
and grid layout on [Taffy](https://github.com/DioxusLabs/taffy), and drawing and text on Direct2D and
DirectWrite. It runs on Windows 10 and 11 (64-bit).

## Status

An early preview. The browser opens and runs real sites, with tabs, bookmarks, history, settings, developer
tools, content blocking and video, and each tab runs in a renderer process of its own. Expect pages that look
wrong, missing features and crashes: [KNOWN-ISSUES.md](KNOWN-ISSUES.md) lists the open problems we know of.
Conformance is tracked with [web-platform-tests](https://web-platform-tests.org) and performance with
Speedometer 3.1 and a suite of micro benchmarks ([docs/benchmarks.md](docs/benchmarks.md)).

## Download

Preview builds for Windows 10 and 11 (64-bit) are on the
[releases page](https://github.com/snowlark-browser/snowlark/releases). Unzip and run `snowlark.exe`;
there is no installer. The builds are not signed yet, so Windows SmartScreen may warn on first start:
choose "More info", then "Run anyway".

## Building

You need Windows 10 or 11 (x64), Visual Studio 2022 or its Build Tools with the "Desktop development with C++"
workload (MSVC x64 and a Windows SDK), [Rust](https://rustup.rs) (stable, `x86_64-pc-windows-msvc`), Git, and
Python 3 for the tools. From a PowerShell prompt in the repository:

```powershell
.\dev.ps1 build --profile iter
```

`dev.ps1` runs cargo inside the Visual Studio x64 environment. The first build downloads the crates and a V8
library built for Snowlark (about 32 MB, checked against a SHA-256; [docs/v8-build.md](docs/v8-build.md) says
what it is and how to build it yourself), and takes a while. The result is `target\iter\snowlark.exe`, the
browser, and `target\iter\snowlark-cli.exe`, the same program for the console.

## Running

```powershell
target\iter\snowlark.exe https://example.org
```

The console program has headless modes:

```powershell
target\iter\snowlark-cli.exe --render https://example.org page.png 1280
```
```powershell
target\iter\snowlark-cli.exe --eval "document.title"
```

## Testing

```powershell
.\dev.ps1 test --workspace
```

[CONTRIBUTING.md](CONTRIBUTING.md) covers the test layout, running web-platform-tests, the benchmarks and the
rules the code follows. [ARCHITECTURE.md](ARCHITECTURE.md) describes the crates and the main design decisions;
`docs/` has the design documents of single areas.

## Use of AI

Snowlark is developed with AI coding assistants, mainly Anthropic's Claude. They write most of the code and
do much of the debugging, testing and benchmarking, directed by the project's maintainer. Every change has to
pass the same checks whoever wrote it: the unit tests, the web-platform-tests gate, and the micro and
Speedometer benchmarks for anything that can move performance. The browser itself has no built-in AI features.

## Reporting a problem

Open an [issue](https://github.com/snowlark-browser/snowlark/issues) with the page address, what you
expected, what you saw, and the build (the release name or the zip's file name, or the commit you built).
Report security problems privately instead, as [SECURITY.md](SECURITY.md) says.

Everyone taking part in the project is expected to follow the [code of conduct](CODE_OF_CONDUCT.md).

## Licences

Snowlark's own code is under the [Mozilla Public License 2.0](LICENSE). The third-party code in `vendor/`
(patched copies of html5ever, Stylo, Taffy, url and the v8 crate) keeps its own licences; each folder's
`SNOWLARK-PATCHES.md` lists what was changed. [THIRD-PARTY.md](THIRD-PARTY.md) describes the other third-party
components and data in this repository.

Each release contains `THIRD-PARTY-NOTICES.txt`, listing the open source software in Snowlark with the
full text of each licence. The source of the components Snowlark ships with changes, such as Stylo, is
also in [snowlark-browser/third-party](https://github.com/snowlark-browser/third-party).
