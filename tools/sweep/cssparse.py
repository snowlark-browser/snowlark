# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""A forgiving CSS scanner, enough to list what a stylesheet uses.

`scan(text)` yields what the text uses, as authored:
- `("decl", property, value)`: a declaration (custom properties and `!important` left out of the value);
- `("selector", text, nested)`: one complex selector of a style rule's selector list; `nested` when the
  rule sits inside another style rule (CSS Nesting);
- `("at", name, prelude)`: an at-rule, block or statement.

It follows CSS Syntax's block structure (strings, escapes, comments, nested parentheses and brackets) but
does not tokenize fully: what it gets wrong is a stray declaration or selector, never a crash. Then
`value_tokens` and `selector_tokens` name the features a value or selector uses (functions, units,
keywords, pseudo-classes), which is what the support diff groups failures by.
"""

from __future__ import annotations

import re

# At-rules whose block holds declarations; the others hold rules (or keyframes).
DECL_AT_RULES = {
    "font-face", "page", "property", "counter-style", "font-palette-values", "viewport", "position-try",
    "view-transition", "top-left", "top-center", "top-right", "bottom-left", "bottom-center", "bottom-right",
    "left-top", "left-middle", "left-bottom", "right-top", "right-middle", "right-bottom",
}
KEYFRAMES = {"keyframes", "-webkit-keyframes", "-moz-keyframes", "-o-keyframes"}


def strip_comments(text: str) -> str:
    """The text without comments; strings are kept as they are."""
    out = []
    i, n = 0, len(text)
    while i < n:
        c = text[i]
        if c in "\"'":
            j = _string_end(text, i)
            out.append(text[i:j])
            i = j
        elif c == "/" and text.startswith("/*", i):
            j = text.find("*/", i + 2)
            i = n if j < 0 else j + 2
            out.append(" ")
        else:
            out.append(c)
            i += 1
    return "".join(out)


def _string_end(text: str, i: int) -> int:
    """The index after the string that starts at `i` (or the end of its line, as CSS Syntax does)."""
    q = text[i]
    j = i + 1
    n = len(text)
    while j < n:
        c = text[j]
        if c == "\\":
            j += 2
            continue
        if c == q:
            return j + 1
        if c == "\n":
            return j
        j += 1
    return n


def split_top(text: str, sep: str) -> list[str]:
    """`text` split on `sep` outside strings, parentheses and brackets."""
    parts, depth, start, i, n = [], 0, 0, 0, len(text)
    while i < n:
        c = text[i]
        if c in "\"'":
            i = _string_end(text, i)
            continue
        if c == "\\":
            i += 2
            continue
        if c in "([":
            depth += 1
        elif c in ")]":
            depth = max(0, depth - 1)
        elif c == sep and depth == 0:
            parts.append(text[start:i])
            start = i + 1
        i += 1
    parts.append(text[start:])
    return parts


def scan(text: str):
    """Yields ("decl", prop, value), ("selector", text, nested) and ("at", name, prelude) events."""
    text = strip_comments(text)
    # Each open block: "rules" (top level, @media...), "style" (a style rule: declarations and nested
    # rules), "decl" (@font-face...), "keyframes" (keyframe selectors), "skip" (inside a keyframe),
    # "ignore" (a block in a custom property's value).
    stack = ["rules"]
    buf_start = 0
    depth = 0  # parentheses and brackets
    i, n = 0, len(text)
    while i < n:
        c = text[i]
        if c in "\"'":
            i = _string_end(text, i)
            continue
        if c == "\\":
            i += 2
            continue
        if c in "([":
            depth += 1
        elif c in ")]":
            depth = max(0, depth - 1)
        elif depth == 0 and c == "{":
            prelude = text[buf_start:i].strip()
            kind = stack[-1]
            if kind == "style":
                # Inside a style rule, the declarations before a nested rule end at the last ';'.
                cut = _last_top(prelude, ";")
                if cut >= 0:
                    yield from _decls(prelude[:cut])
                    prelude = prelude[cut + 1:].strip()
            if kind == "ignore" or (kind == "style" and prelude.startswith("--")):
                # A custom property whose value holds a block: none of it is CSS to list.
                stack.append("ignore")
            elif prelude.startswith("@"):
                name, rest = _at_name(prelude)
                yield ("at", name, rest)
                if name in DECL_AT_RULES:
                    stack.append("decl")
                elif name in KEYFRAMES:
                    stack.append("keyframes")
                else:
                    stack.append("style" if kind == "style" else "rules")
            elif kind in ("keyframes", "skip"):
                stack.append("skip")
            elif kind == "decl":
                stack.append("decl")
            else:
                for sel in split_top(prelude, ","):
                    sel = " ".join(sel.split())
                    if sel:
                        yield ("selector", sel, kind == "style")
                stack.append("style")
            buf_start = i + 1
        elif depth == 0 and c == "}":
            kind = stack[-1]
            if kind in ("style", "decl", "skip"):
                yield from _decls(text[buf_start:i])
            if len(stack) > 1:
                stack.pop()
            buf_start = i + 1
        elif depth == 0 and c == ";":
            kind = stack[-1]
            stmt = text[buf_start:i].strip()
            if kind == "rules" or stmt.startswith("@"):
                if stmt.startswith("@"):
                    name, rest = _at_name(stmt)
                    yield ("at", name, rest)
                buf_start = i + 1
            # In declaration blocks the ';' is kept and split at '}' (or at a nested rule's '{').
        i += 1


def _at_name(prelude: str) -> tuple[str, str]:
    m = re.match(r"@([\w-]+)", prelude)
    if not m:
        return "", prelude
    return m.group(1).lower(), " ".join(prelude[m.end():].split())


def _last_top(text: str, sep: str) -> int:
    """Index of the last `sep` outside strings and parentheses, or -1."""
    last, depth, i, n = -1, 0, 0, len(text)
    while i < n:
        c = text[i]
        if c in "\"'":
            i = _string_end(text, i)
            continue
        if c in "([":
            depth += 1
        elif c in ")]":
            depth = max(0, depth - 1)
        elif c == sep and depth == 0:
            last = i
        i += 1
    return last


def _decls(body: str):
    for part in split_top(body, ";"):
        part = part.strip()
        if not part or part.startswith("@"):
            continue
        colon = part.find(":")
        if colon <= 0:
            continue
        prop = part[:colon].strip().lower()
        if not re.fullmatch(r"-?[a-z][a-z0-9-]*", prop) or prop.startswith("--"):
            continue
        value = part[colon + 1:].strip()
        value = re.sub(r"\s*!\s*important\s*$", "", value, flags=re.I)
        value = " ".join(value.split())
        if value:
            yield ("decl", prop, value)


# ----- feature tokens ----------------------------------------------------------------------------

_URL = re.compile(r"url\((?:[^()\"']|\"[^\"]*\"|'[^']*')*\)", re.I)
_STR = re.compile(r"\"(?:[^\"\\]|\\.)*\"|'(?:[^'\\]|\\.)*'")
_FUNC = re.compile(r"(-?[a-zA-Z][\w-]*)\(")
_UNIT = re.compile(r"(?<![\w#.-])[+-]?(?:\d+\.?\d*|\.\d+)(?:e[+-]?\d+)?([a-zA-Z]+|%)")
_IDENT = re.compile(r"(?<![\w#.-])(-?[a-zA-Z][\w-]*)(?![\w(-])")


def value_tokens(prop: str, value: str) -> list[str]:
    """The features a declaration's value uses: `fn:<name>()`, `unit:<unit>`, `kw:<prop>:<keyword>`.

    Strings and url() contents are left out; numbers without a unit and colours in hex are not features."""
    v = _STR.sub('""', _URL.sub("url()", value))
    toks = []
    for m in _FUNC.finditer(v):
        toks.append(f"fn:{m.group(1).lower()}()")
    for m in _UNIT.finditer(v):
        u = m.group(1).lower()
        if u == "e":
            continue
        toks.append(f"unit:{u}")
    for m in _IDENT.finditer(v):
        toks.append(f"kw:{prop}:{m.group(1).lower()}")
    return list(dict.fromkeys(toks))


_PSEUDO = re.compile(r"(::?-?[a-zA-Z][\w-]*)(\()?")
_ATTR = re.compile(r"\[[^\]]*\]")


def selector_tokens(sel: str) -> list[str]:
    """The features a selector uses: `pseudo:<:name>` (functional ones end in `()`), `nesting:&`,
    `attr:<op>` for attribute operators other than plain `=`, `combinator:<c>` for `||`."""
    s = _STR.sub('""', sel)
    toks = []
    for m in _ATTR.finditer(s):
        op = re.search(r"([~|^$*]?=)", m.group(0))
        if op and op.group(1) != "=":
            toks.append(f"attr:{op.group(1)}")
        if re.search(r"\s[is]\s*\]$", m.group(0)):
            toks.append("attr:case-flag")
    s = _ATTR.sub("[]", s)
    for m in _PSEUDO.finditer(s):
        toks.append(f"pseudo:{m.group(1).lower()}" + ("()" if m.group(2) else ""))
    if "&" in s:
        toks.append("nesting:&")
    if "||" in s:
        toks.append("combinator:||")
    return list(dict.fromkeys(toks))


def value_shape(value: str) -> str:
    """The value with numbers and strings made generic, so similar declarations group together."""
    v = _STR.sub('"…"', _URL.sub("url(…)", value))
    v = re.sub(r"#[0-9a-fA-F]{3,8}\b", "#hex", v)
    return re.sub(r"(?<![\w-])[+-]?(?:\d+\.?\d*|\.\d+)", "N", v)
