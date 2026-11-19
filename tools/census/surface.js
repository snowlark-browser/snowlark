// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// The web-exposed surface of the global this script runs in (a window or a dedicated worker), as a
// flat map from member key to kind. Shared by Chrome and Snowlark so both are enumerated identically.
//
// Keys:
//   "Name"                       an interface object (a global function with a prototype)
//   "Name.member"                a static member of an interface object, or a namespace's member
//   "Name.prototype.member"      a member of an interface's prototype object
//   "<global>.member"            a member of the global ("window" or "self"): own properties of the
//                                global object and of every object on its prototype chain up to
//                                Object.prototype (WebIDL puts a [Global] interface's members on the
//                                instance; where they live is recorded apart, in `placement`)
//   "document.member" ...        own properties of a few instances (unforgeable members live there)
// Kinds: interface, namespace, method, data, getter, setter, accessor (getter and setter).
// Symbol-keyed members are written "@@iterator" etc.
function censusSurface() {
  "use strict";
  const G = globalThis;
  const isWindow = typeof Window === "function" && G instanceof Window;
  const globalName = isWindow ? "window" : "self";
  const members = {};
  const placement = {};
  const parents = {};
  const values = {};
  const kindOf = (d) =>
    "value" in d
      ? typeof d.value === "function"
        ? "method"
        : "data"
      : d.get && d.set
        ? "accessor"
        : d.get
          ? "getter"
          : "setter";
  const keyName = (k) =>
    typeof k === "symbol" ? "@@" + String(k.description || "").replace(/^Symbol\./, "") : k;
  const own = (obj, prefix, skip) => {
    let keys;
    try {
      keys = Reflect.ownKeys(obj);
    } catch (e) {
      return;
    }
    for (const k of keys) {
      const n = keyName(k);
      if (skip && skip.has(n)) continue;
      const d = Object.getOwnPropertyDescriptor(obj, k);
      if (d) members[prefix + n] = kindOf(d);
    }
  };
  const typeOfValue = (get) => {
    try {
      const v = get();
      return v === null ? "null" : typeof v;
    } catch (e) {
      return "throws";
    }
  };
  const isInterfaceObject = (d) =>
    "value" in d && typeof d.value === "function" && /^[A-Z]/.test(d.value.name || "") &&
    d.value.prototype && typeof d.value.prototype === "object";
  const isNamespace = (name, d) =>
    "value" in d && d.value !== null && typeof d.value === "object" &&
    (/^[A-Z]/.test(name) || name === "console");

  // The global object and its prototype chain.
  const staticSkip = new Set(["length", "name", "prototype", "arguments", "caller"]);
  const protoSkip = new Set(["constructor"]);
  let level = 0;
  for (let o = G; o && o !== Object.prototype; o = Object.getPrototypeOf(o), level++) {
    for (const k of Reflect.ownKeys(o)) {
      const n = keyName(k);
      const d = Object.getOwnPropertyDescriptor(o, k);
      if (!d) continue;
      if (level === 0 && isInterfaceObject(d)) {
        const f = d.value;
        members[n] = "interface";
        own(f, n + ".", staticSkip);
        own(f.prototype, n + ".prototype.", protoSkip);
        const p = Object.getPrototypeOf(f);
        parents[n] = p && p !== Function.prototype ? p.name || "" : "";
        continue;
      }
      if (level === 0 && isNamespace(n, d)) {
        members[n] = "namespace";
        own(d.value, n + ".", null);
        continue;
      }
      const key = globalName + "." + n;
      if (key in members) continue;
      members[key] = kindOf(d);
      placement[key] = level === 0 ? "instance" : "prototype" + level;
      if (d.get) values[key] = typeOfValue(() => G[k]);
    }
  }

  // Instances whose own members (unforgeable ones, location's) are not on any prototype, and whose
  // getters' value types show stubs (an object in one engine, undefined or null in the other).
  const instances = isWindow
    ? { document: () => document, location: () => location, navigator: () => navigator, screen: () => screen }
    : { location: () => location, navigator: () => navigator };
  for (const [name, get] of Object.entries(instances)) {
    let obj;
    try {
      obj = get();
    } catch (e) {
      continue;
    }
    if (obj === null || typeof obj !== "object") continue;
    own(obj, name + ".", null);
    values[name + ".@class"] = Object.prototype.toString.call(obj);
    for (let p = Object.getPrototypeOf(obj); p && p !== Object.prototype; p = Object.getPrototypeOf(p)) {
      for (const k of Reflect.ownKeys(p)) {
        const d = Object.getOwnPropertyDescriptor(p, k);
        if (d && d.get && typeof k === "string" && !((name + "." + k) in values)) {
          values[name + "." + k] = typeOfValue(() => obj[k]);
        }
      }
    }
  }
  return {
    global: globalName,
    userAgent: (G.navigator && navigator.userAgent) || "",
    secureContext: !!G.isSecureContext,
    members,
    placement,
    parents,
    values,
  };
}
