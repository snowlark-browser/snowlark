# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Why is a JS object alive: the shortest retaining path from V8's GC roots to it.

Reads a V8 heap snapshot (the .heapsnapshot JSON that `SNOWLARK_HEAP_SNAPSHOT=<file>` makes Snowlark write,
or one saved from DevTools) and prints, for each object whose constructor name matches, the chain of
references that keeps it alive, root first. Weak edges keep nothing alive and are not followed.

    python tools/heap-retainers/retainers.py <file.heapsnapshot> [--name Window] [--max 10]

A removed or replaced frame that leaks shows up as a `Window` (its global object) reachable through
a path that does not go through the page's own window: the first steps after `(Global handles)` name
the strong handle of the engine that holds it (a wrapper, a callback function...), the next ones the
JS references from there to the frame's realm.
"""

import argparse
import json
import sys
from collections import deque


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("snapshot")
    ap.add_argument("--name", default="Window", help="constructor name prefix of the targets")
    ap.add_argument("--max", type=int, default=10, help="paths to print")
    ap.add_argument("--depth", type=int, default=40, help="steps of a path to print")
    ap.add_argument(
        "--holders",
        action="store_true",
        help="for each Window: the page it is (its script's own globals) and every root that keeps it",
    )
    args = ap.parse_args()

    print(f"reading {args.snapshot} ...", file=sys.stderr)
    with open(args.snapshot, "r", encoding="utf-8") as f:
        snap = json.load(f)
    meta = snap["snapshot"]["meta"]
    nf = meta["node_fields"]
    ef = meta["edge_fields"]
    node_types = meta["node_types"][0]
    edge_types = meta["edge_types"][0]
    nodes = snap["nodes"]
    edges = snap["edges"]
    strings = snap["strings"]
    n_len, e_len = len(nf), len(ef)
    n_type, n_name, n_id, n_size, n_edges = (
        nf.index("type"),
        nf.index("name"),
        nf.index("id"),
        nf.index("self_size"),
        nf.index("edge_count"),
    )
    e_type, e_name, e_to = ef.index("type"), ef.index("name_or_index"), ef.index("to_node")
    count = len(nodes) // n_len
    print(f"{count} objects, {len(edges) // e_len} references", file=sys.stderr)

    # Each node's first edge.
    first = [0] * (count + 1)
    acc = 0
    for i in range(count):
        first[i] = acc
        acc += nodes[i * n_len + n_edges]
    first[count] = acc

    weak = edge_types.index("weak")
    string_edges = {edge_types.index(t) for t in ("context", "property", "internal", "shortcut")
                    if t in edge_types}

    # Edges that keep nothing alive by themselves: weak ones, and the key side of a weak table's
    # entry ("part of key (K @k) -> value ... pair in WeakMap": drawn from the key K to the value, the
    # value lives while the key and the table both do; the table's own edge to it stays).
    import re

    key_of = re.compile(r"part of key \(.*? @(\d+)\)")
    skip = bytearray(len(edges) // e_len)
    for i in range(count):
        own_id = nodes[i * n_len + n_id]
        for k in range(first[i], first[i + 1]):
            e = k * e_len
            t = edges[e + e_type]
            if t == weak:
                skip[k] = 1
            elif t in string_edges:
                name = strings[edges[e + e_name]]
                if "part of key (" in name:
                    m = key_of.search(name)
                    if m and int(m.group(1)) == own_id:
                        skip[k] = 1

    def node_label(i):
        t = node_types[nodes[i * n_len + n_type]]
        name = strings[nodes[i * n_len + n_name]]
        if len(name) > 90:
            name = name[:87] + "..."
        return f"{t} {name!r} @{nodes[i * n_len + n_id]}"

    def edge_label(e):
        t = edges[e + e_type]
        tn = edge_types[t]
        v = edges[e + e_name]
        name = strings[v] if t in string_edges else str(v)
        if len(name) > 60:
            name = name[:57] + "..."
        return f"-[{tn} {name}]->"

    # Breadth first from the synthetic root (node 0): each node's parent edge on a shortest path.
    parent = [-1] * count
    parent_edge = [-1] * count
    seen = bytearray(count)
    seen[0] = 1
    queue = deque([0])
    while queue:
        i = queue.popleft()
        for k in range(first[i], first[i + 1]):
            e = k * e_len
            if skip[k]:
                continue
            j = edges[e + e_to] // n_len
            if not seen[j]:
                seen[j] = 1
                parent[j] = i
                parent_edge[j] = e
                queue.append(j)

    if args.holders:
        holders(args, nodes, edges, strings, node_types, edge_types, n_len, e_len, n_type, n_name,
                e_type, e_name, e_to, first, count, skip, seen, node_label, edge_label)
        return

    targets = [
        i
        for i in range(count)
        if node_types[nodes[i * n_len + n_type]] == "object"
        and strings[nodes[i * n_len + n_name]].startswith(args.name)
        and seen[i]
    ]
    print(f"{len(targets)} live objects named {args.name}*", file=sys.stderr)
    for t in targets[: args.max]:
        path = []
        i = t
        while i != 0 and i != -1:
            path.append((parent_edge[i], i))
            i = parent[i]
        path.reverse()
        print(f"\n{node_label(t)}: {len(path)} steps from the roots")
        shown = path if len(path) <= args.depth else path[: args.depth // 2] + [None] + path[-args.depth // 2 :]
        for step in shown:
            if step is None:
                print("    ...")
                continue
            e, j = step
            print(f"    {edge_label(e)} {node_label(j)}")


def holders(args, nodes, edges, strings, node_types, edge_types, n_len, e_len, n_type, n_name,
            e_type, e_name, e_to, first, count, skip, seen, node_label, edge_label):
    """Each live Window (global proxy): its page, told by the globals its script made, and the roots
    (global handles, handle scopes...) it is reachable from, each with its shortest path."""
    name_of = lambda i: strings[nodes[i * n_len + n_name]]
    type_of = lambda i: node_types[nodes[i * n_len + n_type]]

    def out(i):
        for k in range(first[i], first[i + 1]):
            e = k * e_len
            yield e, edges[e + e_to] // n_len

    def edge_name(e):
        t = edge_types[edges[e + e_type]]
        v = edges[e + e_name]
        return strings[v] if t in ("context", "property", "internal", "shortcut") else str(v)

    # The global object of each global proxy, through their native context.
    global_of = {}
    context_of = {}
    for i in range(count):
        if name_of(i) == "system / NativeContext":
            proxy = obj = None
            for e, j in out(i):
                n = edge_name(e)
                if n == "global_proxy_object":
                    proxy = j
                elif n == "global_object":
                    obj = j
            if proxy is not None:
                if obj is None:
                    # (no global_object edge in some V8 versions: the proxy's prototype is it)
                    obj = next((j for e, j in out(proxy) if edge_name(e) == "__proto__"), proxy)
                global_of[proxy] = obj
                context_of[proxy] = i
    props = {
        p: {edge_name(e) for e, _ in out(g) if edge_types[edges[e + e_type]] == "property"}
        for p, g in global_of.items()
    }
    common = {}
    for names in props.values():
        for n in names:
            common[n] = common.get(n, 0) + 1
    builtin = {n for n, c in common.items() if c * 2 > len(props)}

    # Reverse references (strong ones), and the entries right under the roots' categories.
    rev = [[] for _ in range(count)]
    for i in range(count):
        for e, j in out(i):
            if not skip[e // e_len]:
                rev[j].append(i)
    # (the first object under the synthetic root categories: "(GC roots)" -> "(Global handles)" ->
    # the object a global handle holds; a non-synthetic child of the root is an entry itself)
    categories = {0}
    entry_of = {}
    walk = [0]
    while walk:
        c = walk.pop()
        for e, j in out(c):
            if skip[e // e_len]:
                continue
            if type_of(j) == "synthetic":
                if j not in categories:
                    categories.add(j)
                    walk.append(j)
            else:
                entry_of.setdefault(j, (c, e))

    proxies = [p for p in global_of if seen[p]]
    # (each realm's proxy, global object and native context, by the proxy)
    realm_of = {}
    for p in global_of:
        for x in (p, global_of[p], context_of[p]):
            realm_of.setdefault(x, p)
    print(f"{len(proxies)} live windows", file=sys.stderr)
    for t in proxies:
        own = sorted(props.get(t, set()) - builtin)
        print(f"\n{node_label(t)}: its script's globals {own[:12]}{' ...' if len(own) > 12 else ''}")
        # The root entries it is reachable from.
        # Backwards from it, not through another realm's global (what the other realms' script
        # reaches is told apart: "reachable from the realms of").
        mark = bytearray(count)
        mark[t] = 1
        stack = [t]
        entries = []
        others = set()
        while stack:
            i = stack.pop()
            if i in entry_of:
                entries.append(i)
            for j in rev[i]:
                if mark[j] or j in categories or j == 0:
                    continue
                mark[j] = 1
                r = realm_of.get(j)
                if r is not None and r != t:
                    others.add(r)
                    continue
                stack.append(j)
        if others:
            print(f"    reachable from the realms of {sorted(node_label(r) for r in others)[:6]}")
        # (the engine's handles first)
        entries.sort(key=lambda x: name_of(entry_of[x][0]) != "(Global handles)")
        print(f"    kept, not through another realm, from {len(entries)} root entries:")
        # What they are, grouped: the root category, the object's kind and its own properties.
        kinds = {}
        for x in entries:
            own_props = sorted(
                edge_name(e) for e, _ in out(x) if edge_types[edges[e + e_type]] == "property"
            )
            own_props = [n for n in own_props if n != "__proto__"]
            key = (name_of(entry_of[x][0]), type_of(x), name_of(x)[:60], tuple(own_props[:10]))
            if key not in kinds:
                kinds[key] = [0, x]
            kinds[key][0] += 1
        for (cat, ty, nm, own_props), (n, example) in sorted(
            kinds.items(), key=lambda kv: -kv[1][0]
        )[:12]:
            print(f"      {n:5} x {cat} -> {ty} {nm!r} {{{', '.join(own_props)}}}")
            # (one of them: its properties' values, and what else refers to it)
            for e, j in list(out(example))[:8]:
                if edge_types[edges[e + e_type]] in ("property", "element"):
                    print(f"              e.g. {edge_name(e)} = {node_label(j)}")
            for i in rev[example][:4]:
                print(f"              referred to by {node_label(i)}")
        for x in entries[: args.max]:
            # Shortest path from the entry to the window.
            prev = {x: None}
            q = deque([x])
            while q and t not in prev:
                i = q.popleft()
                for e, j in out(i):
                    if not skip[e // e_len] and j not in prev and mark[j]:
                        prev[j] = (i, e)
                        q.append(j)
            path = []
            i = t
            while prev.get(i):
                pi, e = prev[i]
                path.append((e, i))
                i = pi
            path.reverse()
            c, ce = entry_of[x]
            print(f"    {node_label(c)} {edge_label(ce)} {node_label(x)}")
            for e, j in path[: args.depth]:
                print(f"        {edge_label(e)} {node_label(j)}")


if __name__ == "__main__":
    main()
