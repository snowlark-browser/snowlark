// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// Snowlark's testdriver backend: the WPT runner serves this in place of /resources/testdriver-vendor.js.
// Clicks, keys and action sequences (WebDriver "Actions") are not synthesized here: the script resolves
// element origins to viewport points and hands the runner a plan of ticks, which the runner feeds to the
// page's input entry points (the ones the window's input takes: hit testing, then pointer, mouse, wheel
// and keyboard events and their default actions, as trusted events). Actions it does not support reject.
(function () {
  "use strict";
  var internal = window.test_driver_internal;
  if (!internal) return;
  internal.in_automation = true;

  // The runner reads `__snowlarkWptActions` (a plan) between event loop turns, clears it, and calls
  // `__snowlarkWptActionsDone(error)` when the plan has run. Neither is enumerable.
  var queue = Promise.resolve();
  function submit(lines) {
    var run = function () {
      return new Promise(function (resolve, reject) {
        Object.defineProperty(window, "__snowlarkWptActionsDone", {
          configurable: true,
          writable: true,
          value: function (error) {
            delete window.__snowlarkWptActionsDone;
            if (error) reject(new Error("snowlark testdriver: " + error));
            else resolve();
          },
        });
        Object.defineProperty(window, "__snowlarkWptActions", {
          configurable: true,
          writable: true,
          value: lines.join("\n"),
        });
      });
    };
    var p = queue.then(run, run);
    queue = p.catch(function () {});
    return p;
  }

  // An element's in-view center point (WebDriver), in the top-level viewport, as the reference
  // engine's backend finds it: the first client rect, clipped to its document's viewport, offset by
  // the same-origin frame that holds it.
  function frameOf(element) {
    if (document.contains(element)) return window;
    var frames = document.getElementsByTagName("iframe");
    for (var i = 0; i < frames.length; i++) {
      var doc = null;
      try { doc = frames[i].contentDocument; } catch (e) { }
      if (doc && doc.contains(element)) return frames[i];
    }
    return null;
  }

  function centerOf(element) {
    var frame = frameOf(element);
    if (!frame) throw new Error("snowlark testdriver: element in a different document or frame");
    var win = frame === window ? window : frame.contentWindow;
    var rects = element.getClientRects();
    if (!rects.length) throw new Error("snowlark testdriver: element has no box");
    var inView = function (r) {
      var left = Math.max(0, r.left), right = Math.min(win.innerWidth, r.right);
      var top = Math.max(0, r.top), bottom = Math.min(win.innerHeight, r.bottom);
      return [0.5 * (left + right), 0.5 * (top + bottom)];
    };
    var c = inView(rects[0]);
    var doc = win.document;
    var tree = doc.elementsFromPoint(c[0], c[1]);
    if (tree.indexOf(element) === -1 && !(tree[0] && element.contains(tree[0]))) {
      element.scrollIntoView({ behavior: "instant", block: "end", inline: "nearest" });
      c = inView(element.getClientRects()[0]);
    }
    if (frame !== window) {
      var f = frame.getClientRects()[0];
      c[0] += f.left;
      c[1] += f.top;
    }
    return c;
  }

  function num(v, fallback) {
    v = Number(v);
    return isFinite(v) ? v : fallback;
  }

  // WebDriver's tick: each source's action at that index, in source order; the tick lasts as long as
  // its longest pause or move.
  function plan(sources) {
    var lines = [];
    var ticks = 0;
    sources.forEach(function (s) { ticks = Math.max(ticks, s.actions.length); });
    sources.forEach(function (s) {
      if (s.type === "pointer") {
        var kind = (s.parameters && s.parameters.pointerType) || "mouse";
        if (kind !== "mouse") throw new Error("snowlark testdriver: " + kind + " pointers are not supported");
      } else if (s.type !== "key" && s.type !== "wheel" && s.type !== "none") {
        throw new Error("snowlark testdriver: unknown input source type " + s.type);
      }
    });
    for (var t = 0; t < ticks; t++) {
      var duration = 0;
      var body = [];
      sources.forEach(function (s) {
        var a = s.actions[t];
        if (!a) return;
        if (a.type === "pause") {
          duration = Math.max(duration, num(a.duration, 0));
          return;
        }
        if (s.type === "key") {
          var cp = String(a.value).codePointAt(0);
          if (a.type === "keyDown") body.push("kdown " + cp);
          else if (a.type === "keyUp") body.push("kup " + cp);
          else throw new Error("snowlark testdriver: unknown key action " + a.type);
          return;
        }
        var origin = a.origin === undefined ? "viewport" : a.origin;
        var x = num(a.x, 0), y = num(a.y, 0), base = "v";
        if (origin === "pointer") base = "p";
        else if (origin !== "viewport") {
          var c = centerOf(origin);
          x += c[0];
          y += c[1];
        }
        if (s.type === "pointer") {
          if (a.type === "pointerMove") {
            var d = num(a.duration, 0);
            duration = Math.max(duration, d);
            body.push("move " + x + " " + y + " " + d + " " + base);
          } else if (a.type === "pointerDown") body.push("down " + num(a.button, 0));
          else if (a.type === "pointerUp") body.push("up " + num(a.button, 0));
          else if (a.type === "pointerCancel") throw new Error("pointerCancel is not supported");
          else throw new Error("snowlark testdriver: unknown pointer action " + a.type);
        } else if (s.type === "wheel" && a.type === "scroll") {
          if (base === "p") throw new Error("snowlark testdriver: a scroll's origin cannot be the pointer");
          duration = Math.max(duration, num(a.duration, 0));
          body.push("scroll " + x + " " + y + " " + num(a.deltaX, 0) + " " + num(a.deltaY, 0));
        } else {
          throw new Error("snowlark testdriver: unknown " + s.type + " action " + a.type);
        }
      });
      lines.push("tick " + duration);
      lines.push.apply(lines, body);
    }
    return lines;
  }

  internal.action_sequence = function (actions) {
    if (window.top !== window) {
      return Promise.reject(new Error("snowlark testdriver: actions are sent from the top-level window only"));
    }
    var lines;
    try { lines = plan(actions); } catch (e) { return Promise.reject(e); }
    return submit(lines);
  };

  // testdriver.js has scrolled the element into view and found its in-view center.
  internal.click = function (element, coords) {
    return submit(["tick 0", "move " + coords.x + " " + coords.y + " 0 v", "down 0", "up 0"]);
  };

  // WebDriver "Element Send Keys": focus, then each character's key down and up; a shifted character
  // is typed with Shift held, and modifier keys stay down until the end.
  internal.send_keys = function (element, keys) {
    if (document.activeElement !== element && element.focus) element.focus();
    var lines = ["tick 0"];
    var mods = [0xE008, 0xE009, 0xE00A, 0xE03D, 0xE050, 0xE051, 0xE052, 0xE053];
    var held = [];
    var shifted = false;
    for (var ch of keys) {
      var cp = ch.codePointAt(0);
      var isShifted = /^[A-Z~!@#$%^&*()_+{}|:"<>?]$/.test(ch);
      if (isShifted !== shifted && held.indexOf(0xE008) === -1) {
        lines.push((isShifted ? "kdown " : "kup ") + 0xE008);
        shifted = isShifted;
      }
      if (cp === 0xE000) {
        held.forEach(function (m) { lines.push("kup " + m); });
        held = [];
      } else if (mods.indexOf(cp) !== -1) {
        if (held.indexOf(cp) === -1) {
          held.push(cp);
          lines.push("kdown " + cp);
        }
      } else {
        lines.push("kdown " + cp, "kup " + cp);
      }
    }
    if (shifted) lines.push("kup " + 0xE008);
    held.forEach(function (m) { lines.push("kup " + m); });
    return submit(lines);
  };

  internal.set_permission = function () {
    return Promise.reject(new Error("snowlark testdriver: permissions are not supported"));
  };
})();
