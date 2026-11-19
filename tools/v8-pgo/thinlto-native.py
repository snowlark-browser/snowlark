# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Turns a V8 library built with ThinLTO (bitcode members) into a native one that any linker takes.

ThinLTO normally runs in the final link, which would need every build of ours to link with lld-link. This does the
link-time step once, the way a distributed ThinLTO build does: a thin link of the library's members alone
(`lld-link /thinlto-index-only`) with every symbol our Rust code can reference kept as a root, then one backend
compile per member (`clang -fthinlto-index=`), and the native objects put back into a library in the same order.

    python tools/v8-pgo/thinlto-native.py --lib <thinlto rusty_v8.lib> --clang <clang dir> --work <dir>
        --roots <v8 crate src> --roots crates --roots <src_binding.rs> --out <native rusty_v8.lib> [--jobs 16]

The roots, the symbols no consumer may lose (everything else may be made internal): every symbol the bindings'
members define, V8's public C++ API (namespaces `v8` and `cppgc`, outside `internal` and `base`), and every
`link_name` and every `fn`/`static` with a `__` in its name in the given Rust sources. A root missing from them shows
up as an unresolved symbol in the final link, never silently.
"""

import argparse
import concurrent.futures
import os
import re
import shutil
import struct
import subprocess
import sys
import time

BITCODE = (b"BC\xc0\xde", b"\xde\xc0\x17\x0b")
# Backend flags as the build's own link step passes them (`build/config/compiler/BUILD.gn`, ThinLTO on Windows).
BACKEND_FLAGS = [
    "-O2",
    "-ffunction-sections",
    "-fdata-sections",
    "-mllvm", "-inlinehint-threshold=360",
    "-mllvm", "-split-threshold-for-reg-with-hint=0",
    "-mllvm", "-instcombine-lower-dbg-declare=0",
    "-mllvm", "-disable-auto-upgrade-debug-info",
]
# The members that hold the bindings' C functions (`src/binding.cc`, `src/crdtp_binding.cc`): every symbol they
# define is a root.
BINDING_MEMBERS = ("rusty_v8/binding.obj", "rusty_v8/crdtp_binding.obj")
THIN_LINK_FLAGS = [
    "-mllvm:-import-instr-limit=30",
    "-mllvm:-inlinehint-threshold=360",
    "-mllvm:-disable-auto-upgrade-debug-info",
]


def read_archive(path):
    """Yields (name, bytes) for each member of a COFF/GNU archive, skipping the symbol tables."""
    data = open(path, "rb").read()
    if not data.startswith(b"!<arch>\n"):
        sys.exit(f"{path}: not an archive")
    pos, longnames = 8, b""
    while pos + 60 <= len(data):
        header = data[pos:pos + 60]
        name = header[:16].decode("latin-1").rstrip()
        size = int(header[48:58].decode("latin-1").strip())
        body = data[pos + 60:pos + 60 + size]
        pos += 60 + size + (size & 1)
        if name == "/" or name == "/SYM64/":
            continue
        if name == "//":
            longnames = body
            continue
        if name.startswith("/") and name[1:].isdigit():
            start = int(name[1:])
            end = start
            while end < len(longnames) and longnames[end:end + 1] not in (b"\0", b"\n"):
                end += 1
            name = longnames[start:end].decode("utf-8", "replace").rstrip("/")
        else:
            name = name.rstrip("/")
        yield name, body


def public_api(lib):
    """V8's public C++ API among the names the archive's index lists: those whose outermost namespace is `v8` or
    `cppgc`, not inside `internal` or `base`. Code of ours may call any of them by mangled name some day."""
    data = open(lib, "rb").read()
    size = int(data[56:66].decode("latin-1").strip())
    body = data[68:68 + size]  # the first linker member: big-endian count, offsets, names
    n = struct.unpack(">I", body[:4])[0]
    names = [x.decode("latin-1") for x in body[4 + 4 * n:].split(b"\0")[:n]]
    out = []
    for s in names:
        if s.startswith("?") and "@@" in s:
            parts = s[1:s.index("@@")].split("@")
            if parts[-1] in ("v8", "cppgc") and not {"internal", "base"} & set(parts):
                out.append(s)
    return out


def roots_from(paths):
    names = set()
    link_name = re.compile(r'#\[link_name\s*=\s*"(?:\\u\{1\})?([^"]+)"\]')
    decl = re.compile(r"\b(?:fn|static(?:\s+mut)?)\s+([A-Za-z_]\w*__\w+)\s*[(:<]")
    for root in paths:
        files = [root] if os.path.isfile(root) else [
            os.path.join(d, f) for d, _, fs in os.walk(root) for f in fs if f.endswith(".rs")]
        for f in files:
            text = open(f, encoding="utf-8", errors="replace").read()
            names.update(link_name.findall(text))
            names.update(decl.findall(text))
    return sorted(names)


def coff_defined_externals(path):
    """The external symbols a COFF object (plain or /bigobj) defines."""
    data = open(path, "rb").read()
    if data[:4] == b"\x00\x00\xff\xff":  # bigobj: ANON_OBJECT_HEADER_BIGOBJ
        sym_ptr, nsyms = struct.unpack_from("<II", data, 48)
        rec, sec_fmt = 20, "<i"
    else:
        sym_ptr, nsyms = struct.unpack_from("<II", data, 8)
        rec, sec_fmt = 18, "<h"
    strings = sym_ptr + nsyms * rec
    names, i = [], 0
    while i < nsyms:
        r = sym_ptr + i * rec
        raw = data[r:r + 8]
        section = struct.unpack_from(sec_fmt, data, r + 12)[0]
        storage, aux = data[r + rec - 2], data[r + rec - 1]
        if storage == 2 and section > 0:  # IMAGE_SYM_CLASS_EXTERNAL, defined in a section
            if raw[:4] == b"\0\0\0\0":
                off = strings + struct.unpack_from("<I", raw, 4)[0]
                name = data[off:data.index(b"\0", off)]
            else:
                name = raw.rstrip(b"\0")
            names.append(name.decode("latin-1"))
        i += 1 + aux
    return names


def coff_symbols(path):
    """(defined, undefined) external symbol names of a COFF object."""
    data = open(path, "rb").read()
    if data[:4] == b"\x00\x00\xff\xff":
        sym_ptr, nsyms = struct.unpack_from("<II", data, 48)
        rec, sec_fmt = 20, "<i"
    else:
        sym_ptr, nsyms = struct.unpack_from("<II", data, 8)
        rec, sec_fmt = 18, "<h"
    strings = sym_ptr + nsyms * rec
    defined, undefined, i = set(), set(), 0
    while i < nsyms:
        r = sym_ptr + i * rec
        raw = data[r:r + 8]
        section = struct.unpack_from(sec_fmt, data, r + 12)[0]
        storage, aux = data[r + rec - 2], data[r + rec - 1]
        if storage in (2, 105):  # external, weak external
            if raw[:4] == b"\0\0\0\0":
                off = strings + struct.unpack_from("<I", raw, 4)[0]
                name = data[off:data.index(b"\0", off)].decode("latin-1")
            else:
                name = raw.rstrip(b"\0").decode("latin-1")
            if storage == 105 or section > 0:
                defined.add(name)
            else:
                undefined.add(name)
        i += 1 + aux
    return defined, undefined


def write_thunks(path, pairs):
    """A COFF object with one COMDAT (any) section per (name, target): `name: jmp target`."""
    n = len(pairs)
    strtab = bytearray(b"\0\0\0\0")

    def sym_name(name):
        b = name.encode("latin-1")
        if len(b) <= 8:
            return b.ljust(8, b"\0")
        off = len(strtab)
        strtab.extend(b + b"\0")
        return struct.pack("<II", 0, off)

    header_size, section_size = 20, 40
    data_start = header_size + section_size * n
    # Per section: 5 bytes of code, then one 10-byte relocation.
    raw = bytearray()
    sections = bytearray()
    symbols = bytearray()
    # Symbols: for section k, [section symbol, its aux, the thunk]; then each target, undefined.
    target_index = {}
    targets = sorted({t for _, t in pairs})
    first_target = 3 * n
    for j, t in enumerate(targets):
        target_index[t] = first_target + j
    for k, (name, target) in enumerate(pairs):
        code_at = data_start + len(raw)
        raw.extend(b"\xe9\0\0\0\0")
        reloc_at = data_start + len(raw)
        raw.extend(struct.pack("<IIH", 1, target_index[target], 4))  # IMAGE_REL_AMD64_REL32
        # IMAGE_SCN_CNT_CODE | LNK_COMDAT | ALIGN_16BYTES | MEM_EXECUTE | MEM_READ
        sections.extend(struct.pack("<8sIIIIIIHHI", b".text", 0, 0, 5, code_at, reloc_at, 0, 1, 0, 0x60501020))
        symbols.extend(struct.pack("<8sIhHBB", b".text", 0, k + 1, 0, 3, 1))  # static section symbol + 1 aux
        symbols.extend(struct.pack("<IHHIHBB2s", 5, 1, 0, 0, 0, 2, 0, b""))   # aux: length, relocs, ANY
        symbols.extend(struct.pack("<8sIhHBB", sym_name(name), 0, k + 1, 0x20, 2, 0))  # function, external
    for t in targets:
        symbols.extend(struct.pack("<8sIhHBB", sym_name(t), 0, 0, 0x20, 2, 0))
    nsyms = 3 * n + len(targets)
    sym_ptr = data_start + len(raw)
    struct.pack_into("<I", strtab, 0, len(strtab))
    header = struct.pack("<HHIIIHH", 0x8664, n, 0, sym_ptr, nsyms, 0, 0)
    with open(path, "wb") as f:
        f.write(header + sections + raw + symbols + strtab)


def run(cmd, log, cwd=None):
    with open(log, "w") as out:
        return subprocess.run(cmd, stdout=out, stderr=subprocess.STDOUT, cwd=cwd).returncode


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--lib", required=True)
    ap.add_argument("--clang", required=True, help="Chromium's clang folder (bin\\clang-cl.exe, bin\\lld-link.exe)")
    ap.add_argument("--work", required=True)
    ap.add_argument("--roots", action="append", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--jobs", type=int, default=16)
    a = ap.parse_args()

    work = os.path.abspath(a.work)
    os.makedirs(os.path.dirname(os.path.abspath(a.out)), exist_ok=True)
    members_dir = os.path.join(work, "m")
    shutil.rmtree(members_dir, ignore_errors=True)
    os.makedirs(members_dir)
    lld = os.path.join(a.clang, "bin", "lld-link.exe")
    # clang-cl.exe in its GNU mode (the driver's mode follows the program's name): -x ir and -fthinlto-index.
    clang = os.path.join(work, "clang.exe")
    shutil.copyfile(os.path.join(a.clang, "bin", "clang-cl.exe"), clang)

    t0 = time.time()
    members = []  # (path, kind) in archive order
    bindings = []
    for i, (name, body) in enumerate(read_archive(a.lib)):
        if body[:4] in BITCODE:
            kind = "bc"
        elif body[:2] == b"\x64\x86" or body[:4] == b"\x00\x00\xff\xff":
            kind = "native"
        else:
            continue  # rlib metadata (lib.rmeta) and other non-objects: no linker reads them
        base = re.sub(r"[^\w.-]", "_", os.path.basename(name.replace("\\", "/")))[-80:]
        path = os.path.join(members_dir, f"{i:05d}_{base}")
        if kind == "bc" and not path.endswith(".o"):
            path += ".o"
        with open(path, "wb") as f:
            f.write(body)
        members.append((path, kind))
        if kind == "bc" and name.replace("\\", "/").endswith(BINDING_MEMBERS):
            bindings.append(path)
    n_bc = sum(1 for _, k in members if k == "bc")
    print(f"{len(members)} members ({n_bc} bitcode), {time.time() - t0:.0f} s", flush=True)

    # The modules' own triple (it carries the MSVC version they were compiled for), so the backend keeps it.
    first_bc = next(p for p, k in members if k == "bc")
    triple = re.search(rb"x86_64-pc-windows-msvc[0-9.]*[0-9]", open(first_bc, "rb").read()).group().decode()

    roots = set(roots_from(a.roots))
    api = public_api(a.lib)
    print(f"{len(api)} public API symbols", flush=True)
    roots.update(api)
    if len(bindings) != len(BINDING_MEMBERS):
        sys.exit(f"expected the members {BINDING_MEMBERS}, found {bindings}")
    for path in bindings:
        plain = path + ".plain.obj"
        code = run([clang, f"--target={triple}", "-x", "ir", path, "-c", "-O0", "-o", plain], plain + ".log")
        if code != 0:
            sys.exit(f"compiling {path} alone failed, see {plain}.log")
        defined = coff_defined_externals(plain)
        print(f"{os.path.basename(path)}: {len(defined)} defined symbols", flush=True)
        roots.update(defined)
    roots = sorted(roots)
    print(f"{len(roots)} roots", flush=True)
    rsp = os.path.join(work, "thin-link.rsp")
    regular = os.path.join(work, "regular-lto.obj")
    if os.path.exists(regular):
        os.remove(regular)
    with open(rsp, "w") as f:
        f.write("/machine:x64 /dll /noentry /nodefaultlib /force:unresolved /opt:lldlto=2\n")
        f.write(f'/out:"{os.path.join(work, "thin-link.dll")}"\n')
        f.write(f'/thinlto-index-only:"{os.path.join(work, "thin-link.list")}"\n')
        for flag in THIN_LINK_FLAGS:
            f.write(flag + "\n")
        for r in roots:
            f.write(f'"/include:{r}"\n')
        # Whatever the modules put in the regular (not thin) part of their LTO unit, vtables among it: the index-only
        # link compiles it but would not keep it.
        f.write(f'"/lto-obj-path:{regular}"\n')
        f.write("/start-lib\n")
        for path, _ in members:
            f.write(f'"{path}"\n')
        f.write("/end-lib\n")
    t0 = time.time()
    code = run([lld, "@" + rsp], os.path.join(work, "thin-link.log"))
    print(f"thin link: exit {code}, {time.time() - t0:.0f} s", flush=True)
    if code != 0:
        sys.exit(f"thin link failed, see {work}\\thin-link.log")

    def backend(path):
        index = path + ".thinlto.bc"
        if not os.path.exists(index) or os.path.getsize(index) == 0:
            return path, None, 0  # not reachable from the roots: no consumer can need it
        out = path[:-2] + ".native.obj"
        cmd = [clang, f"--target={triple}", "-x", "ir", path, f"-fthinlto-index={index}",
               "-c", "-o", out] + BACKEND_FLAGS
        return path, out, run(cmd, path + ".log")

    t0 = time.time()
    outputs, failed, skipped = {}, [], 0
    with concurrent.futures.ThreadPoolExecutor(a.jobs) as pool:
        for path, out, code in pool.map(backend, [p for p, k in members if k == "bc"]):
            if out is None:
                skipped += 1
            elif code != 0:
                failed.append(path)
            else:
                outputs[path] = out
    print(f"backends: {len(outputs)} compiled, {skipped} unreachable, {len(failed)} failed, "
          f"{time.time() - t0:.0f} s", flush=True)
    if failed:
        sys.exit("failed: " + ", ".join(failed[:5]) + f" (logs next to them, {members_dir})")

    # Vector deleting destructors (`??_E`) are weak aliases of the scalar ones (`??_G`) in this ABI; the index-only
    # link resolves no module as their owner, so every backend leaves them undefined. Define each missing one as
    # a jump to its alias target, in a COMDAT of its own (archive members are pulled by the names they define).
    objs = list(outputs.values()) + [p for p, k in members if k == "native"]
    if os.path.exists(regular):
        objs.append(regular)
    defined, undefined = set(), set()
    for o in objs:
        d, u = coff_symbols(o)
        defined |= d
        undefined |= u
    missing = undefined - defined
    pairs = sorted((m, "??_G" + m[4:]) for m in missing if m.startswith("??_E") and "??_G" + m[4:] in defined)
    n_dtors = len(pairs)
    # The same for the global deallocation helpers newer compilers call from deleting destructors (`__global_delete`,
    # `__global_array_delete`): weak externals whose default is `__empty_global_delete` of the same signature, which
    # is what they resolve to in a link without ThinLTO when no module defines them.
    for m in sorted(missing):
        for helper in ("?__global_delete@@", "?__global_array_delete@@"):
            if m.startswith(helper) and "?__empty_global_delete@@" + m[len(helper):] in defined:
                pairs.append((m, "?__empty_global_delete@@" + m[len(helper):]))
    thunks = os.path.join(work, "deleting-destructors.obj")
    if pairs:
        write_thunks(thunks, pairs)
    print(f"{n_dtors} vector deleting destructors and {len(pairs) - n_dtors} global delete helpers defined as their "
          f"aliases; {len(missing) - len(pairs)} other names left to the consumer (C runtime, Windows, the crate's "
          f"Rust side)", flush=True)

    # Members are named relative to the work folder (lld runs there), so the library carries no path of the
    # machine that built it.
    def member(p):
        try:
            return os.path.relpath(p, work)
        except ValueError:  # another drive
            return p

    rsp = os.path.join(work, "lib.rsp")
    with open(rsp, "w") as f:
        if pairs:
            f.write(f'"{member(thunks)}"\n')
        if os.path.exists(regular):
            print(f"regular LTO part: {os.path.getsize(regular)} bytes", flush=True)
            f.write(f'"{member(regular)}"\n')
        for path, kind in members:
            obj = outputs.get(path) if kind == "bc" else path
            if obj:
                f.write(f'"{member(obj)}"\n')
    out_lib = os.path.abspath(a.out)
    code = run([lld, "/lib", "/nologo", f"/out:{out_lib}", "@" + rsp], os.path.join(work, "lib.log"), cwd=work)
    print(f"library: exit {code}, {os.path.getsize(a.out) if code == 0 else 0} bytes", flush=True)
    sys.exit(code)


if __name__ == "__main__":
    main()
