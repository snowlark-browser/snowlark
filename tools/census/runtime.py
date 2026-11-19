# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Stage 3: every corpus page rendered by Snowlark headless, and what went wrong on it.

`snowlark-cli --render <url> <png> 1280` runs with content blocking off, `SNOWLARK_CONSOLE=tagged` (one
`[console.<level>] text (source)` line per console message, uncaught exceptions as errors with where
they were thrown) and `SNOWLARK_RENDER_TIMEOUT_MS`; a page that never settles is captured at that
timeout, and a process still running a minute after it (a script that never yields) is stopped by
PID. The PNG is checked against a blank canvas.
"""

from __future__ import annotations

import json
import os
import re
import struct
import subprocess
import time
import zlib
from pathlib import Path

# A JSON summary of the settled document, printed by SNOWLARK_EVAL.
EVAL = (
    "JSON.stringify({title: document.title, url: location.href, readyState: document.readyState, "
    "elements: document.getElementsByTagName('*').length, "
    "text: document.body ? (document.body.textContent || '').trim().length : -1})"
)

_RE_LOADED = re.compile(r"^\[render\] (loaded|not loaded) after (\d+) ms( \(timeout\))?")
_RE_DONE = re.compile(r"^\[render\] (\S+) -> .* in (\d+) ms")
_RE_CONSOLE = re.compile(r"^\[console\.(\w+)\] (.*)$")
_RE_SOURCE = re.compile(r" \(([^()\s]*:\d+:\d+)\)$")
_RE_REFERENCE = re.compile(r"ReferenceError: ([\w$]+) is not defined")
_RE_TYPE = [
    re.compile(r"TypeError: (?:[\w$.\[\]()]*?\.)?([\w$]+) is not a (?:function|constructor)"),
    re.compile(r"TypeError: Cannot read properties of (?:undefined|null) \(reading '([^']+)'\)"),
    re.compile(r"TypeError: Cannot set properties of (?:undefined|null) \(setting '([^']+)'\)"),
    re.compile(r"TypeError: (?:[\w$.]*?\.)?([\w$]+) is (?:undefined|null|not iterable)"),
    re.compile(r"TypeError: Failed to construct '([\w$]+)'"),
]


def parse_log(text: str) -> dict:
    """The facts of one `--render` log: settle time, console errors and exceptions, missing names."""
    out = {
        "loaded": None,
        "timeout": None,
        "settle_ms": None,
        "total_ms": None,
        "final_url": None,
        "errors": [],
        "exceptions": [],
        "warnings": 0,
        "reference_errors": [],
        "type_error_names": [],
        "eval": None,
        "panic": None,
    }
    for line in text.splitlines():
        m = _RE_LOADED.match(line)
        if m:
            out["loaded"] = m.group(1) == "loaded"
            out["settle_ms"] = int(m.group(2))
            out["timeout"] = bool(m.group(3))
            continue
        m = _RE_DONE.match(line)
        if m:
            out["final_url"] = m.group(1)
            out["total_ms"] = int(m.group(2))
            continue
        if line.startswith("{") and '"readyState"' in line:
            try:
                out["eval"] = json.loads(line)
            except json.JSONDecodeError:
                pass
            continue
        if "panicked at" in line and out["panic"] is None:
            out["panic"] = line.strip()[:300]
            continue
        m = _RE_CONSOLE.match(line)
        if not m:
            continue
        level, rest = m.groups()
        if level == "warn":
            out["warnings"] += 1
        if level != "error":
            continue
        source = None
        sm = _RE_SOURCE.search(rest)
        if sm:
            source = sm.group(1)
            rest = rest[: sm.start()]
        entry = {"text": rest[:500], "source": source}
        if rest.startswith("Uncaught"):
            out["exceptions"].append(entry)
        else:
            out["errors"].append(entry)
        for r in _RE_REFERENCE.findall(rest):
            if r not in out["reference_errors"]:
                out["reference_errors"].append(r)
        for pattern in _RE_TYPE:
            for name in pattern.findall(rest):
                if name not in out["type_error_names"]:
                    out["type_error_names"].append(name)
    return out


def render_site(snowlark: str, url: str, outdir: Path, timeout_s: int) -> dict:
    outdir.mkdir(parents=True, exist_ok=True)
    png, log = outdir / "snowlark.png", outdir / "snowlark.log"
    if png.exists():
        png.unlink()
    env = dict(os.environ)
    env.pop("SNOWLARK_BLOCK", None)  # content blocking off
    env.update(
        SNOWLARK_CONSOLE="tagged",
        SNOWLARK_RENDER_TIMEOUT_MS=str(timeout_s * 1000),
        SNOWLARK_EVAL=EVAL,
    )
    started = time.time()
    killed = False
    with open(log, "wb") as f:
        proc = subprocess.Popen(
            [snowlark, "--render", url, str(png), "1280"], stdout=f, stderr=subprocess.STDOUT, env=env
        )
        try:
            proc.wait(timeout=timeout_s + 60)
        except subprocess.TimeoutExpired:
            killed = True
            subprocess.run(
                ["taskkill", "/PID", str(proc.pid), "/T", "/F"],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
            proc.wait(timeout=30)
    wall = time.time() - started
    result = parse_log(log.read_bytes().decode("utf-8", "replace"))
    result.update(url=url, exit_code=proc.returncode, killed=killed, wall_s=round(wall, 1))
    result["image"] = analyze_png(png) if png.exists() else None
    (outdir / "snowlark.json").write_text(json.dumps(result, indent=1), encoding="utf-8")
    return result


# ----- PNG (standard library only) -----------------------------------------------------------------


def decode_png(path: Path, max_rows: int | None = None) -> tuple[int, int, int, list[bytes]]:
    """(width, height, bytes per pixel, rows) of an 8-bit, non-interlaced grey/RGB(A) PNG; at most
    `max_rows` rows are reconstructed."""
    data = path.read_bytes()
    if data[:8] != b"\x89PNG\r\n\x1a\n":
        raise ValueError("not a PNG")
    pos, idat = 8, []
    width = height = depth = ctype = interlace = None
    while pos < len(data):
        (length,) = struct.unpack(">I", data[pos : pos + 4])
        kind = data[pos + 4 : pos + 8]
        body = data[pos + 8 : pos + 8 + length]
        pos += 12 + length
        if kind == b"IHDR":
            width, height, depth, ctype, _, _, interlace = struct.unpack(">IIBBBBB", body)
        elif kind == b"IDAT":
            idat.append(body)
        elif kind == b"IEND":
            break
    if depth != 8 or interlace != 0 or ctype not in (0, 2, 4, 6):
        raise ValueError(f"unsupported PNG (depth {depth}, color type {ctype}, interlace {interlace})")
    bpp = {0: 1, 2: 3, 4: 2, 6: 4}[ctype]
    raw = zlib.decompress(b"".join(idat))
    stride = width * bpp
    rows: list[bytes] = []
    prev = bytes(stride)
    n = height if max_rows is None else min(height, max_rows)
    for y in range(n):
        f = raw[y * (stride + 1)]
        line = bytearray(raw[y * (stride + 1) + 1 : (y + 1) * (stride + 1)])
        if f == 1:
            for i in range(bpp, stride):
                line[i] = (line[i] + line[i - bpp]) & 255
        elif f == 2:
            line = bytearray((a + b) & 255 for a, b in zip(line, prev))
        elif f == 3:
            for i in range(stride):
                left = line[i - bpp] if i >= bpp else 0
                line[i] = (line[i] + ((left + prev[i]) >> 1)) & 255
        elif f == 4:
            for i in range(stride):
                a = line[i - bpp] if i >= bpp else 0
                b = prev[i]
                c = prev[i - bpp] if i >= bpp else 0
                p = a + b - c
                pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
                pr = a if pa <= pb and pa <= pc else (b if pb <= pc else c)
                line[i] = (line[i] + pr) & 255
        prev = bytes(line)
        rows.append(prev)
    return width, height, bpp, rows


def analyze_png(path: Path, max_rows: int = 800, step: int = 2) -> dict:
    """How far the capture is from a blank canvas: the share of sampled pixels (every `step`-th
    pixel of every `step`-th row in the first `max_rows` rows, the first viewport) that differ from the
    most common colour, and the number of distinct colours. `blank` when under 0.05% differ."""
    try:
        width, height, bpp, rows = decode_png(path, max_rows)
    except (ValueError, zlib.error, struct.error) as e:
        return {"error": str(e)}
    counts: dict[bytes, int] = {}
    for row in rows[::step]:
        for x in range(0, width * bpp, bpp * step):
            px = row[x : x + bpp]
            counts[px] = counts.get(px, 0) + 1
    total = sum(counts.values()) or 1
    dominant, top = max(counts.items(), key=lambda kv: kv[1]) if counts else (b"", 0)
    ink = 1 - top / total
    return {
        "width": width,
        "height": height,
        "ink": round(ink, 5),
        "colors": len(counts),
        "background": dominant.hex(),
        "blank": ink < 0.0005,
    }
