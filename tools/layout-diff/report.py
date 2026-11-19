# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""HTML reports and the side-by-side PNG (composed as a page and screenshotted by the same Chrome)."""

import html
import json
import os
import pathlib

COLORS = ["#e00000", "#ff8c00", "#0070f0", "#00a050", "#a000c0"]


def esc(v):
    return html.escape("" if v is None else str(v))


def box(r):
    return "-" if r is None else "%g, %g, %g &times; %g" % tuple(round(v, 1) for v in r)


def compose(chrome, item_list, chrome_png, snowlark_png, width, out_png, workdir):
    """Writes `out_png`: the region around the first item, Chrome left, Snowlark right, the items outlined
    (first in red, the next ones in other colours). Returns the path or None."""
    if not item_list or not chrome_png or not snowlark_png:
        return None
    first = item_list[0]
    ys = []
    for r in (first["chrome"], first["snowlark"]):
        if r:
            ys += [r[1], r[1] + r[3]]
    top = max(0, min(ys) - 150)
    bottom = max(ys) + 150
    if bottom - top > 1600:
        bottom = top + 1600
    height = int(bottom - top)
    gap, head = 24, 28

    def outlines(key):
        out = []
        for k, it in enumerate(item_list[:5]):
            r = it[key]
            if not r:
                continue
            c = COLORS[k % len(COLORS)]
            out.append(
                f'<div style="position:absolute;left:{r[0] - 2}px;top:{r[1] - top - 2}px;'
                f'width:{max(r[2], 1)}px;height:{max(r[3], 1)}px;border:2px solid {c};'
                f'box-shadow:0 0 0 1px #fff"></div>'
                f'<div style="position:absolute;left:{max(r[0], 0)}px;top:{max(r[1] - top - 20, 0)}px;'
                f'background:{c};color:#fff;font:bold 12px sans-serif;padding:1px 4px">{k + 1}</div>'
            )
        return "".join(out)

    def pane(name, png, key):
        uri = pathlib.Path(png).resolve().as_uri()
        return (
            f'<div style="position:relative;width:{width}px;height:{height + head}px;overflow:hidden;'
            f'flex:none;background:#fff"><div style="height:{head}px;font:bold 16px sans-serif;'
            f'line-height:{head}px;background:#222;color:#fff;padding-left:8px">{name} '
            f'(y {int(top)}&ndash;{int(bottom)})</div><div style="position:relative;height:{height}px;'
            f'overflow:hidden"><img src="{uri}" style="position:absolute;left:0;top:{-top}px;'
            f'display:block">{outlines(key)}</div></div>'
        )

    page = (
        '<!doctype html><html><body style="margin:0;background:#888;display:flex;gap:'
        f'{gap}px">{pane("Chrome", chrome_png, "chrome")}{pane("Snowlark", snowlark_png, "snowlark")}'
        "</body></html>"
    )
    src = os.path.join(workdir, "compose.html")
    with open(src, "w", encoding="utf-8") as f:
        f.write(page)
    total_w = 2 * width + gap
    session = chrome.new_page(total_w, height + head)
    chrome.navigate(session, pathlib.Path(src).resolve().as_uri(), timeout=30)
    chrome.screenshot(session, total_w, height + head, out_png)
    os.remove(src)
    return out_png


def item_html(k, it, open_=False):
    why = it.get("why") or {}
    why_rows = "".join(
        f"<li>{esc(aspect)}: {esc(v) if v not in (None, 'TEXT-WRAP', 'TEXT-WIDTH') else '<b>unexplained (root)</b>'}</li>"
        for aspect, v in why.items()
    )
    diff = "".join(
        f"<tr class=d><td>{esc(p)}</td><td>{esc(a)}</td><td>{esc(b)}</td></tr>" for p, a, b in it["props"]["differing"]
    )
    rel = ", ".join(f"<code>{esc(p)}: {esc(v)}</code>" for p, v in it["props"]["relevant"])
    styles = "".join(
        f"<tr{' class=d' if d else ''}><td>{esc(p)}</td><td>{esc(a)}</td><td>{esc(b)}</td></tr>"
        for p, a, b, d in it["styles"]
    )
    text = it.get("text")
    text_row = ""
    if text and (text["chrome"][0] is not None or text["snowlark"][0] is not None):
        text_row = (
            f"<tr><td>own text height, lines</td><td>{esc(text['chrome'][0])}, {esc(text['chrome'][1])}</td>"
            f"<td>{esc(text['snowlark'][0])}, {esc(text['snowlark'][1])}</td><td></td></tr>"
        )
    color = COLORS[k % len(COLORS)] if k < 5 else "#666"
    return f"""
<details class=item {'open' if open_ else ''}>
<summary><span class=n style="background:{color}">{k + 1}</span> <b>{esc(', '.join(it['kinds']))}</b>
 <code>{esc(it['label'])}</code> &Delta; {esc(it.get('delta'))}{f" <i>(&times;{it['repeats']} similar elements)</i>" if it.get('repeats', 1) > 1 else ''}</summary>
<p class=path>{esc(it['path'])}</p>
<table class=boxes><tr><th></th><th>Chrome</th><th>Snowlark</th><th>&Delta; (x, y, w, h) / relative (x, y)</th></tr>
<tr><td>box</td><td>{box(it['chrome'])}</td><td>{box(it['snowlark'])}</td><td>{esc(it.get('delta'))} / {esc(it.get('rel'))}</td></tr>
{text_row}</table>
{'<p>Explanations tried:</p><ul>' + why_rows + '</ul>' if why_rows else ''}
{source_html(it)}<p><b>Properties involved</b> (differing computed values):</p>
{'<table class=props><tr><th>property</th><th>Chrome</th><th>Snowlark</th></tr>' + diff + '</table>' if diff else '<p>none differ</p>'}
<p>Deciding properties with non-initial values: {rel or 'none'}</p>
<details><summary>all computed values</summary><table class=props><tr><th>property</th><th>Chrome</th><th>Snowlark</th></tr>{styles}</table></details>
</details>"""


def source_html(it):
    src = it.get("source")
    if not src:
        return ""
    rel = ", ".join(f"<code>{esc(p)}: {esc(v)}</code>" for p, v in src["props"]["relevant"])
    diff = ", ".join(f"<code>{esc(p)}: {esc(a)} / {esc(b)}</code>" for p, a, b in src["props"]["differing"])
    return (
        "<p><b>Accumulated:</b> the previous siblings each differ by less than the tolerance, and the sum "
        f"crosses it here. The small difference starts at <code>{esc(src['label'])}</code> "
        f"(&Delta; {esc(src.get('delta'))}; Chrome {box(src['chrome'])}, Snowlark {box(src['snowlark'])}), "
        f"<span class=path>{esc(src['path'])}</span>. Its differing values: {diff or 'none'}; "
        f"deciding values: {rel or 'none'}.</p>"
    )


CSS = """
body{font:14px/1.45 system-ui,sans-serif;margin:16px;color:#111;background:#fff}
code{font:12px ui-monospace,Consolas,monospace;background:#f2f2f2;padding:0 3px}
table{border-collapse:collapse;margin:6px 0}td,th{border:1px solid #ccc;padding:2px 6px;font-size:12px;text-align:left;vertical-align:top}
tr.d td{background:#ffe3e3}.path{font:11px Consolas,monospace;color:#555;word-break:break-all}
.item{border:1px solid #ddd;border-radius:6px;padding:6px 10px;margin:8px 0}.n{color:#fff;padding:0 6px;border-radius:3px}
img.side{max-width:100%;border:1px solid #999}.meta td{font-size:13px}.warn{color:#b00000;font-weight:bold}
"""


def site_report(path, result):
    roots, raw = result.get("roots", []), result.get("raw", [])
    s = result.get("summary", {})
    meta = result["meta"]
    warn = "".join(f"<p class=warn>{esc(w)}</p>" for w in result.get("warnings", []))
    head = "".join(
        f"<tr><td>{esc(k)}</td><td>{esc(v)}</td></tr>"
        for k, v in [
            ("URL", meta["url"]),
            ("final URL (Chrome / Snowlark)", f"{meta.get('chrome_url')} / {meta.get('snowlark_url')}"),
            ("width, settle, tolerance", f"{meta['width']} px, {meta['settle_ms']} ms, {meta['tol']} px"),
            ("user agent (both)", meta.get("ua")),
            ("time (Chrome / Snowlark / total)", f"{meta.get('chrome_s')} s / {meta.get('snowlark_s')} s / {meta.get('total_s')} s"),
            ("snapshot script (Chrome / Snowlark)", f"{meta.get('chrome_walk_ms')} ms / {meta.get('snowlark_walk_ms')} ms"),
            ("elements (Chrome / Snowlark / matched)", f"{s.get('chrome_elements')} / {s.get('snowlark_elements')} / {s.get('matched')}"),
            ("differing, missing, extra", f"{s.get('differing')}, {s.get('missing')}, {s.get('extra')}"),
            ("not reported by Snowlark's getComputedStyle", ", ".join(s.get("unsupported_props", [])) or "-"),
        ]
    )
    side = (
        '<p><img class=side src="side-by-side.png"></p>' if result.get("side_by_side") else ""
    )
    shots = " ".join(
        f'<a href="{n}">{n}</a>' for n in ("chrome.png", "snowlark.png", "chrome.json", "snowlark.json", "snowlark.log")
        if os.path.exists(os.path.join(os.path.dirname(path), n))
    )
    body = f"""<!doctype html><html><head><meta charset=utf-8><title>layout-diff {esc(meta['url'])}</title>
<style>{CSS}</style></head><body>
<h1>Layout diff: <a href="{esc(meta['url'])}">{esc(meta['url'])}</a></h1>
{warn}<table class=meta>{head}</table><p>Files: {shots}</p>
<h2>First difference (root cause)</h2>
{item_html(0, roots[result['first']], True) if roots and result.get('first') is not None else '<p>No differences beyond the tolerance.</p>'}
{side}
<h2>Root causes, document order (first {len(roots)})</h2>
{''.join(item_html(k, it) for k, it in enumerate(roots))}
<h2>Raw differences, ancestor first, consequences collapsed (first {len(raw)})</h2>
{''.join(item_html(k + 100, it) for k, it in enumerate(raw))}
</body></html>"""
    with open(path, "w", encoding="utf-8") as f:
        f.write(body)


def props_text(it, limit=6):
    if it.get("source"):
        src = it["source"]
        return f"from {src['label']} {src.get('delta')}: " + props_text(src, limit)
    d = [f"{p}: {a} -> {b}" for p, a, b in it["props"]["differing"]]
    r = [f"{p}={v}" for p, v in it["props"]["relevant"]]
    return "; ".join((d + r)[:limit])


def corpus_report(outdir, rows):
    """summary.tsv, summary.json and summary.html with the per-site first difference and the ranking."""
    from collections import Counter

    with open(os.path.join(outdir, "summary.json"), "w", encoding="utf-8") as f:
        json.dump(rows, f, indent=1)
    with open(os.path.join(outdir, "summary.tsv"), "w", encoding="utf-8") as f:
        f.write("url\tstatus\tkind\telement\tdelta\tproperties\tchrome_s\tsnowlark_s\ttotal_s\tnotes\n")
        for r in rows:
            f.write("\t".join(str(r.get(k, "")).replace("\t", " ") for k in
                              ("url", "status", "kind", "element", "delta", "properties", "chrome_s", "snowlark_s",
                               "total_s", "notes")) + "\n")
    kinds, props, contexts = Counter(), Counter(), Counter()
    for r in rows:
        for it in r.get("roots_top", []):
            kinds[it["kind"]] += 1
            for p in it["differing"]:
                props[p] += 1
            contexts[f"{it['kind']} of {it['tag']} ({it['display']})"] += 1

    def table(counter, title):
        return f"<h3>{title}</h3><table><tr><th>count</th><th>{title}</th></tr>" + "".join(
            f"<tr><td>{n}</td><td>{esc(k)}</td></tr>" for k, n in counter.most_common(25)) + "</table>"

    trs = "".join(
        f"<tr><td><a href=\"{esc(r.get('report', ''))}\">{esc(r['url'])}</a></td><td>{esc(r.get('status'))}</td>"
        f"<td>{esc(r.get('kind'))}</td><td><code>{esc(r.get('element'))}</code></td><td>{esc(r.get('delta'))}</td>"
        f"<td>{esc(r.get('properties'))}</td><td>{esc(r.get('chrome_s'))} / {esc(r.get('snowlark_s'))} / "
        f"{esc(r.get('total_s'))}</td><td>{esc(r.get('notes'))}</td></tr>"
        for r in rows
    )
    page = f"""<!doctype html><html><head><meta charset=utf-8><title>layout-diff corpus</title><style>{CSS}</style>
</head><body><h1>Layout diff corpus</h1>
<table><tr><th>URL</th><th>status</th><th>first difference</th><th>element</th><th>&Delta; x, y, w, h</th>
<th>CSS properties involved</th><th>s (Chrome / Snowlark / total)</th><th>notes</th></tr>{trs}</table>
<h2>Ranking over the first root causes of every site</h2>
{table(kinds, 'kind')}{table(contexts, 'kind of element (display)')}{table(props, 'differing computed property')}
</body></html>"""
    with open(os.path.join(outdir, "summary.html"), "w", encoding="utf-8") as f:
        f.write(page)
