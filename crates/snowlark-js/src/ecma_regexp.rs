// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! ECMAScript regular expressions for Rust code that has no V8 scope in its hands.
//!
//! The URL Pattern algorithms (URLPattern's parser and matcher, a Rust library) compile and run
//! regular expressions from deep inside their own call tree, through a trait that carries no
//! context. [`with_scope`] makes the caller's scope the current one for the duration of a closure;
//! [`validate`] and [`exec`] use it. A call outside of [`with_scope`] fails (no match, not valid).

use std::cell::Cell;

thread_local! {
    /// The scope [`with_scope`] is running under, erased of its lifetimes.
    static CURRENT: Cell<*mut ()> = const { Cell::new(std::ptr::null_mut()) };
}

/// Restores the previous current scope, also when the closure unwinds.
struct Restore(*mut ());

impl Drop for Restore {
    fn drop(&mut self) {
        CURRENT.with(|c| c.set(self.0));
    }
}

/// Runs `f` with `scope` as the scope [`validate`] and [`exec`] use on this thread.
pub fn with_scope<R>(scope: &mut v8::PinScope<'_, '_>, f: impl FnOnce() -> R) -> R {
    let ptr = scope as *mut v8::PinScope<'_, '_> as *mut ();
    let _restore = Restore(CURRENT.with(|c| c.replace(ptr)));
    f()
}

/// Runs `f` on the current scope, if there is one.
fn with_current<R>(f: impl FnOnce(&mut v8::PinScope<'_, '_>) -> R) -> Option<R> {
    let ptr = CURRENT.with(|c| c.get());
    if ptr.is_null() {
        return None;
    }
    // SAFETY: `ptr` was made from a live `&mut PinScope` by `with_scope`, which is still running on
    // this thread (it restores the previous value before returning), and that borrow is held by
    // `with_scope`'s caller for as long as the closure runs, so nothing else uses the scope now. The
    // lifetime parameters were erased in `with_scope` and are chosen again here, short enough that
    // nothing derived from this reference outlives the closure.
    let scope = unsafe { &mut *(ptr as *mut v8::PinScope<'_, '_>) };
    Some(f(scope))
}

/// The creation flags for `flags` ("v" or "u", with an optional "i").
fn creation_flags(flags: &str) -> v8::RegExpCreationFlags {
    let mut f = v8::RegExpCreationFlags::empty();
    for c in flags.chars() {
        f |= match c {
            'v' => v8::RegExpCreationFlags::UNICODE_SETS,
            'u' => v8::RegExpCreationFlags::UNICODE,
            'i' => v8::RegExpCreationFlags::IGNORE_CASE,
            _ => v8::RegExpCreationFlags::empty(),
        };
    }
    f
}

/// Whether `pattern` compiles with `flags` ("u" or "v", with an optional "i").
pub fn validate(pattern: &str, flags: &str) -> bool {
    with_current(|scope| {
        v8::tc_scope!(let tc, scope);
        let Some(source) = v8::String::new(tc, pattern) else {
            return false;
        };
        let ok = v8::RegExp::new(tc, source, creation_flags(flags)).is_some();
        if !ok {
            tc.reset();
        }
        ok
    })
    .unwrap_or(false)
}

/// Runs `pattern` against `text` (the RegExp Builtin Exec algorithm, without `lastIndex`): `None`
/// when it does not match, else the capture groups in order as slices of `text` (`None` for a
/// group that did not take part).
pub fn exec<'t>(pattern: &str, flags: &str, text: &'t str) -> Option<Vec<Option<&'t str>>> {
    with_current(|scope| {
        v8::tc_scope!(let tc, scope);
        let result = exec_in(tc, pattern, flags, text);
        if tc.has_caught() {
            tc.reset();
        }
        result
    })
    .flatten()
}

fn exec_in<'t>(
    scope: &mut v8::PinScope<'_, '_>,
    pattern: &str,
    flags: &str,
    text: &'t str,
) -> Option<Vec<Option<&'t str>>> {
    let source = v8::String::new(scope, pattern)?;
    let re = v8::RegExp::new(
        scope,
        source,
        creation_flags(flags) | v8::RegExpCreationFlags::HAS_INDICES,
    )?;
    let subject = v8::String::new(scope, text)?;
    let m = re.exec(scope, subject)?;
    if m.is_null_or_undefined() {
        return None;
    }
    // The `d` flag's `indices`: [start, end] in UTF-16 code units for each group, which become
    // slices of `text` through the offsets of its characters.
    let key = v8::String::new(scope, "indices")?;
    let indices: v8::Local<v8::Array> = m.get(scope, key.into())?.try_into().ok()?;
    let offsets: Vec<usize> = if text.is_ascii() {
        Vec::new()
    } else {
        let mut v = Vec::with_capacity(text.len() + 1);
        for (i, c) in text.char_indices() {
            for _ in 0..c.len_utf16() {
                v.push(i);
            }
        }
        v.push(text.len());
        v
    };
    let byte = |unit: usize| -> usize {
        if offsets.is_empty() {
            unit
        } else {
            offsets[unit]
        }
    };
    let mut out = Vec::new();
    for i in 1..indices.length() {
        let entry = indices.get_index(scope, i)?;
        if entry.is_null_or_undefined() {
            out.push(None);
            continue;
        }
        let pair: v8::Local<v8::Array> = entry.try_into().ok()?;
        let start = pair.get_index(scope, 0)?.uint32_value(scope)? as usize;
        let end = pair.get_index(scope, 1)?.uint32_value(scope)? as usize;
        out.push(Some(text.get(byte(start)..byte(end))?));
    }
    Some(out)
}
