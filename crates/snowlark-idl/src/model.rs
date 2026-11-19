// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The linked model: partials merged, mixins applied, typedefs resolvable, inheritance checked, exposure
//! computed per member. This is what the generator reads.

use crate::ast::*;
use std::collections::{BTreeMap, BTreeSet};

/// A member of an interface after linking, with where it came from.
#[derive(Clone, Debug)]
pub struct IMember {
    pub member: Member,
    /// The mixin that contributed this member, if any (`Element includes ParentNode`).
    pub mixin: Option<String>,
    /// The global names this member is exposed in (`*` for all).
    pub exposed: BTreeSet<String>,
    /// Extended attributes of the partial interface or mixin definition the member was written in
    /// (`[SecureContext]` on a partial applies to its members).
    pub container_ext: Vec<ExtAttr>,
}

impl IMember {
    pub fn secure_context(&self) -> bool {
        has_ext(self.member.ext(), "SecureContext") || has_ext(&self.container_ext, "SecureContext")
    }
}

#[derive(Clone, Debug)]
pub struct Interface {
    pub name: String,
    pub kind: InterfaceKind,
    pub inherits: Option<String>,
    pub ext: Vec<ExtAttr>,
    pub members: Vec<IMember>,
    /// Mixins applied with `includes`, in source order.
    pub includes: Vec<String>,
    pub exposed: BTreeSet<String>,
    pub span: Span,
}

impl Interface {
    pub fn has_ext(&self, name: &str) -> bool {
        has_ext(&self.ext, name)
    }

    pub fn exposed_in(&self, global: &str) -> bool {
        exposed_in(&self.exposed, global)
    }
}

#[derive(Clone, Debug)]
pub struct Dictionary {
    pub name: String,
    pub inherits: Option<String>,
    pub members: Vec<DictMember>,
    pub ext: Vec<ExtAttr>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct Enum {
    pub name: String,
    pub values: Vec<String>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct Typedef {
    pub name: String,
    pub ty: Type,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct Callback {
    pub name: String,
    pub ret: Type,
    pub args: Vec<Argument>,
    pub ext: Vec<ExtAttr>,
    pub span: Span,
}

/// What an identifier in a type names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NamedKind {
    Interface,
    CallbackInterface,
    Namespace,
    Mixin,
    Dictionary,
    Enum,
    Callback,
    Typedef,
    Buffer,
    Unknown,
}

#[derive(Default, Debug)]
pub struct Model {
    pub interfaces: BTreeMap<String, Interface>,
    pub mixins: BTreeMap<String, Interface>,
    pub dictionaries: BTreeMap<String, Dictionary>,
    pub enums: BTreeMap<String, Enum>,
    pub typedefs: BTreeMap<String, Typedef>,
    pub callbacks: BTreeMap<String, Callback>,
    /// Other names for interfaces: `[LegacyWindowAlias]` (`SVGPoint` is `DOMPoint`) and the spec-prose alias
    /// `WindowProxy`, which HTML uses as a type for the `Window` object's proxy.
    pub aliases: BTreeMap<String, String>,
    /// Linking problems: duplicate definitions, partials without a definition, unknown types, inheritance
    /// cycles. The goal is none.
    pub diagnostics: Vec<String>,
}

/// Mixin members left out of one including interface: (interface, mixin, member).
pub const SKIPPED_MIXIN_MEMBERS: &[(&str, &str, &str)] = &[
    // SVG 2 §16.2: `SVGAElement includes HTMLHyperlinkElementUtils` would put a string `href` on
    // the interface that SVGURIReference already gives an SVGAnimatedString `href`; the
    // SVGAnimatedString stays (as in Chrome and Firefox) and the other URL members come from the
    // mixin.
    ("SVGAElement", "HyperlinkElementUtils", "href"),
];

/// Whether an exposure set includes a global name.
pub fn exposed_in(set: &BTreeSet<String>, global: &str) -> bool {
    set.contains("*") || set.contains(global)
}

fn exposure_of(ext: &[ExtAttr]) -> Option<BTreeSet<String>> {
    ext.iter()
        .find(|a| a.name == "Exposed")
        .map(|a| a.tokens().into_iter().map(str::to_string).collect())
}

struct Partial {
    members: Vec<Member>,
    ext: Vec<ExtAttr>,
}

impl Model {
    /// Links parsed files (file name, definitions) into a model.
    pub fn link(files: Vec<(String, Vec<Definition>)>) -> Model {
        let mut m = Model::default();
        let mut partial_ifaces: Vec<(InterfaceKind, String, Partial, Span)> = Vec::new();
        let mut partial_dicts: Vec<(String, Vec<DictMember>, Span)> = Vec::new();
        let mut includes: Vec<(String, String, Span)> = Vec::new();

        for (_, defs) in files {
            for def in defs {
                match def {
                    Definition::Interface {
                        kind,
                        name,
                        partial,
                        inherits,
                        members,
                        ext,
                        span,
                    } => {
                        if partial {
                            partial_ifaces.push((kind, name, Partial { members, ext }, span));
                            continue;
                        }
                        let exposed = exposure_of(&ext).unwrap_or_default();
                        let iface = Interface {
                            name: name.clone(),
                            kind,
                            inherits,
                            members: members
                                .into_iter()
                                .map(|member| IMember {
                                    exposed: exposure_of(member.ext())
                                        .unwrap_or_else(|| exposed.clone()),
                                    member,
                                    mixin: None,
                                    container_ext: Vec::new(),
                                })
                                .collect(),
                            ext,
                            includes: Vec::new(),
                            exposed,
                            span: span.clone(),
                        };
                        let map = if kind == InterfaceKind::Mixin {
                            &mut m.mixins
                        } else {
                            &mut m.interfaces
                        };
                        if let Some(old) = map.insert(name.clone(), iface) {
                            m.diagnostics.push(format!(
                                "{}:{}: duplicate definition of {name} (first at {}:{})",
                                span.file, span.line, old.span.file, old.span.line
                            ));
                        }
                    }
                    Definition::Dictionary {
                        name,
                        partial,
                        inherits,
                        members,
                        ext,
                        span,
                    } => {
                        if partial {
                            partial_dicts.push((name, members, span));
                        } else if let Some(old) = m.dictionaries.insert(
                            name.clone(),
                            Dictionary {
                                name: name.clone(),
                                inherits,
                                members,
                                ext,
                                span: span.clone(),
                            },
                        ) {
                            m.diagnostics.push(format!(
                                "{}:{}: duplicate dictionary {name} (first at {}:{})",
                                span.file, span.line, old.span.file, old.span.line
                            ));
                        }
                    }
                    Definition::Enum {
                        name, values, span, ..
                    } => {
                        if m.enums
                            .insert(
                                name.clone(),
                                Enum {
                                    name: name.clone(),
                                    values,
                                    span: span.clone(),
                                },
                            )
                            .is_some()
                        {
                            m.diagnostics.push(format!(
                                "{}:{}: duplicate enum {name}",
                                span.file, span.line
                            ));
                        }
                    }
                    Definition::Typedef { name, ty, span, .. } => {
                        if m.typedefs
                            .insert(
                                name.clone(),
                                Typedef {
                                    name: name.clone(),
                                    ty,
                                    span: span.clone(),
                                },
                            )
                            .is_some()
                        {
                            m.diagnostics.push(format!(
                                "{}:{}: duplicate typedef {name}",
                                span.file, span.line
                            ));
                        }
                    }
                    Definition::Callback {
                        name,
                        ret,
                        args,
                        ext,
                        span,
                    } => {
                        if m.callbacks
                            .insert(
                                name.clone(),
                                Callback {
                                    name: name.clone(),
                                    ret,
                                    args,
                                    ext,
                                    span: span.clone(),
                                },
                            )
                            .is_some()
                        {
                            m.diagnostics.push(format!(
                                "{}:{}: duplicate callback {name}",
                                span.file, span.line
                            ));
                        }
                    }
                    Definition::Includes {
                        target,
                        mixin,
                        span,
                    } => includes.push((target, mixin, span)),
                }
            }
        }

        // Partial interfaces, namespaces and mixins: their members join the definition, exposed where the
        // partial says or else where the definition is.
        for (kind, name, partial, span) in partial_ifaces {
            let map = if kind == InterfaceKind::Mixin {
                &mut m.mixins
            } else {
                &mut m.interfaces
            };
            let Some(target) = map.get_mut(&name) else {
                m.diagnostics.push(format!(
                    "{}:{}: partial {name} has no definition",
                    span.file, span.line
                ));
                continue;
            };
            let exposed = exposure_of(&partial.ext).unwrap_or_else(|| target.exposed.clone());
            let container_ext: Vec<ExtAttr> = partial
                .ext
                .iter()
                .filter(|a| a.name != "Exposed")
                .cloned()
                .collect();
            for member in partial.members {
                target.members.push(IMember {
                    exposed: exposure_of(member.ext()).unwrap_or_else(|| exposed.clone()),
                    member,
                    mixin: None,
                    container_ext: container_ext.clone(),
                });
            }
        }
        for (name, members, span) in partial_dicts {
            match m.dictionaries.get_mut(&name) {
                Some(d) => d.members.extend(members),
                None => m.diagnostics.push(format!(
                    "{}:{}: partial dictionary {name} has no definition",
                    span.file, span.line
                )),
            }
        }

        // Mixins: each member is copied into the including interface, exposed where both the mixin member
        // and the interface are (Web IDL §2.4: a mixin's members are exposed in the intersection).
        for (target, mixin, span) in includes {
            let Some(mx) = m.mixins.get(&mixin).cloned() else {
                m.diagnostics.push(format!(
                    "{}:{}: {target} includes unknown mixin {mixin}",
                    span.file, span.line
                ));
                continue;
            };
            let Some(iface) = m.interfaces.get_mut(&target) else {
                m.diagnostics.push(format!(
                    "{}:{}: unknown interface {target} includes {mixin}",
                    span.file, span.line
                ));
                continue;
            };
            if iface.includes.contains(&mixin) {
                continue;
            }
            iface.includes.push(mixin.clone());
            for mut im in mx.members {
                let skipped = SKIPPED_MIXIN_MEMBERS.iter().any(|&(t, x, name)| {
                    t == target
                        && x == mixin
                        && matches!(&im.member, Member::Attribute(a) if a.name == name)
                });
                if skipped {
                    continue;
                }
                // An empty set means the mixin did not say: the member is exposed where the interface is.
                im.exposed = if im.exposed.is_empty() || im.exposed.contains("*") {
                    iface.exposed.clone()
                } else if iface.exposed.contains("*") {
                    im.exposed.clone()
                } else {
                    im.exposed.intersection(&iface.exposed).cloned().collect()
                };
                im.mixin = Some(mixin.clone());
                let mut ext = mx.ext.clone();
                ext.retain(|a| a.name != "Exposed");
                ext.extend(im.container_ext);
                im.container_ext = ext;
                iface.members.push(im);
            }
        }

        // Names the specs use as types but define in prose rather than IDL.
        // CSSOM §6.2: "CSSOMString ... implementations choose DOMString or USVString"; all engines use
        // DOMString.
        m.typedefs
            .entry("CSSOMString".into())
            .or_insert_with(|| Typedef {
                name: "CSSOMString".into(),
                ty: Type::prim(Prim::DOMString),
                span: Span {
                    file: "(cssom prose)".into(),
                    line: 0,
                },
            });
        m.aliases.insert("WindowProxy".into(), "Window".into());
        for i in m.interfaces.values() {
            if let Some(a) = ext(&i.ext, "LegacyWindowAlias") {
                for alias in a.tokens() {
                    m.aliases.insert(alias.to_string(), i.name.clone());
                }
            }
        }

        m.check();
        m
    }

    /// What an identifier names.
    pub fn kind_of(&self, name: &str) -> NamedKind {
        let name = self.aliases.get(name).map_or(name, String::as_str);
        if let Some(i) = self.interfaces.get(name) {
            return match i.kind {
                InterfaceKind::CallbackInterface => NamedKind::CallbackInterface,
                InterfaceKind::Namespace => NamedKind::Namespace,
                _ => NamedKind::Interface,
            };
        }
        if self.mixins.contains_key(name) {
            NamedKind::Mixin
        } else if self.dictionaries.contains_key(name) {
            NamedKind::Dictionary
        } else if self.enums.contains_key(name) {
            NamedKind::Enum
        } else if self.callbacks.contains_key(name) {
            NamedKind::Callback
        } else if self.typedefs.contains_key(name) {
            NamedKind::Typedef
        } else if BUFFER_TYPES.contains(&name) {
            NamedKind::Buffer
        } else {
            NamedKind::Unknown
        }
    }

    /// Expands typedefs (recursively, into union members and generic arguments too). Nullability and type
    /// extended attributes of the use site are kept.
    pub fn resolve(&self, ty: &Type) -> Type {
        self.resolve_depth(ty, 0)
    }

    fn resolve_depth(&self, ty: &Type, depth: u32) -> Type {
        if depth > 32 {
            return ty.clone();
        }
        let r = |t: &Type| Box::new(self.resolve_depth(t, depth + 1));
        let kind = match &ty.kind {
            TypeKind::Named(n) => {
                if let Some(td) = self.typedefs.get(n) {
                    let mut inner = self.resolve_depth(&td.ty, depth + 1);
                    inner.nullable |= ty.nullable;
                    inner.ext.extend(ty.ext.iter().cloned());
                    return inner;
                }
                TypeKind::Named(self.aliases.get(n).unwrap_or(n).clone())
            }
            TypeKind::Prim(p) => TypeKind::Prim(*p),
            TypeKind::Sequence(t) => TypeKind::Sequence(r(t)),
            TypeKind::FrozenArray(t) => TypeKind::FrozenArray(r(t)),
            TypeKind::ObservableArray(t) => TypeKind::ObservableArray(r(t)),
            TypeKind::AsyncSequence(t) => TypeKind::AsyncSequence(r(t)),
            TypeKind::Promise(t) => TypeKind::Promise(r(t)),
            TypeKind::Record(k, v) => TypeKind::Record(*k, r(v)),
            TypeKind::Union(ts) => {
                // Flatten nested unions (Web IDL "flattened member types").
                let mut flat = Vec::new();
                for t in ts {
                    let t = self.resolve_depth(t, depth + 1);
                    match t.kind {
                        // `[AllowShared]` on a nested union (`[AllowShared] ArrayBufferView`) applies
                        // to its buffer view members.
                        TypeKind::Union(inner) if !t.nullable => {
                            let shared = t.ext.iter().filter(|e| e.name == "AllowShared");
                            let shared: Vec<ExtAttr> = shared.cloned().collect();
                            flat.extend(inner.into_iter().map(|mut m| {
                                m.ext.extend(shared.iter().cloned());
                                m
                            }))
                        }
                        _ => flat.push(t),
                    }
                }
                TypeKind::Union(flat)
            }
        };
        Type {
            kind,
            nullable: ty.nullable,
            ext: ty.ext.clone(),
        }
    }

    /// The inheritance chain of an interface, the interface first.
    pub fn chain(&self, name: &str) -> Vec<&Interface> {
        let mut out = Vec::new();
        let mut cur = self.interfaces.get(name);
        while let Some(i) = cur {
            if out.iter().any(|o: &&Interface| o.name == i.name) {
                break;
            }
            out.push(i);
            cur = i.inherits.as_deref().and_then(|p| self.interfaces.get(p));
        }
        out
    }

    /// The dictionary inheritance chain, the most basic dictionary first (the order members are converted).
    pub fn dictionary_chain(&self, name: &str) -> Vec<&Dictionary> {
        let mut out = Vec::new();
        let mut cur = self.dictionaries.get(name);
        while let Some(d) = cur {
            if out.iter().any(|o: &&Dictionary| o.name == d.name) {
                break;
            }
            out.push(d);
            cur = d.inherits.as_deref().and_then(|p| self.dictionaries.get(p));
        }
        out.reverse();
        out
    }

    /// Whether `name` is `ancestor` or inherits from it.
    pub fn inherits_from(&self, name: &str, ancestor: &str) -> bool {
        self.chain(name).iter().any(|i| i.name == ancestor)
    }

    fn check(&mut self) {
        let mut diags = Vec::new();
        for i in self.interfaces.values() {
            if let Some(p) = &i.inherits {
                match self.kind_of(p) {
                    NamedKind::Interface | NamedKind::CallbackInterface => {}
                    _ => diags.push(format!(
                        "{}:{}: {} inherits from unknown interface {p}",
                        i.span.file, i.span.line, i.name
                    )),
                }
                if self.chain(&i.name).len() > 64 || self.chain(p).iter().any(|c| c.name == i.name)
                {
                    diags.push(format!("{}: inheritance cycle", i.name));
                }
            }
            if i.exposed.is_empty() && i.kind != InterfaceKind::CallbackInterface {
                diags.push(format!(
                    "{}:{}: {} has no [Exposed]",
                    i.span.file, i.span.line, i.name
                ));
            }
            for im in &i.members {
                for ty in member_types(&im.member) {
                    self.check_type(
                        ty,
                        &format!("{}.{}", i.name, member_name(&im.member)),
                        &mut diags,
                    );
                }
            }
        }
        for mx in self.mixins.values() {
            for im in &mx.members {
                for ty in member_types(&im.member) {
                    self.check_type(
                        ty,
                        &format!("{}.{}", mx.name, member_name(&im.member)),
                        &mut diags,
                    );
                }
            }
        }
        for d in self.dictionaries.values() {
            if let Some(p) = &d.inherits {
                if self.kind_of(p) != NamedKind::Dictionary {
                    diags.push(format!("{} inherits from unknown dictionary {p}", d.name));
                }
            }
            for dm in &d.members {
                self.check_type(&dm.ty, &format!("{}.{}", d.name, dm.name), &mut diags);
            }
        }
        for t in self.typedefs.values() {
            self.check_type(&t.ty, &t.name, &mut diags);
        }
        for c in self.callbacks.values() {
            self.check_type(&c.ret, &c.name, &mut diags);
            for a in &c.args {
                self.check_type(&a.ty, &c.name, &mut diags);
            }
        }
        self.diagnostics.extend(diags);
    }

    fn check_type(&self, ty: &Type, ctx: &str, diags: &mut Vec<String>) {
        match &ty.kind {
            TypeKind::Named(n) => match self.kind_of(n) {
                NamedKind::Unknown => diags.push(format!("{ctx}: unresolved type {n}")),
                NamedKind::Mixin | NamedKind::Namespace => {
                    diags.push(format!("{ctx}: {n} is not a type"))
                }
                _ => {}
            },
            TypeKind::Prim(_) => {}
            TypeKind::Sequence(t)
            | TypeKind::FrozenArray(t)
            | TypeKind::ObservableArray(t)
            | TypeKind::AsyncSequence(t)
            | TypeKind::Promise(t)
            | TypeKind::Record(_, t) => self.check_type(t, ctx, diags),
            TypeKind::Union(ts) => {
                for t in ts {
                    self.check_type(t, ctx, diags);
                }
            }
        }
    }
}

/// The types a member mentions.
pub fn member_types(m: &Member) -> Vec<&Type> {
    match m {
        Member::Const(c) => vec![&c.ty],
        Member::Attribute(a) => vec![&a.ty],
        Member::Operation(o) => std::iter::once(&o.ret)
            .chain(o.args.iter().map(|a| &a.ty))
            .collect(),
        Member::Constructor(args, _) => args.iter().map(|a| &a.ty).collect(),
        Member::Stringifier(_) => Vec::new(),
        Member::Iterable {
            key, value, args, ..
        } => key
            .iter()
            .chain(std::iter::once(value))
            .chain(args.iter().map(|a| &a.ty))
            .collect(),
        Member::Maplike { key, value, .. } => vec![key, value],
        Member::Setlike { value, .. } => vec![value],
    }
}

/// A member's name for diagnostics.
pub fn member_name(m: &Member) -> String {
    match m {
        Member::Const(c) => c.name.clone(),
        Member::Attribute(a) => a.name.clone(),
        Member::Operation(o) => o.name.clone().unwrap_or_else(|| match o.special {
            Some(Special::Getter) => "(getter)".into(),
            Some(Special::Setter) => "(setter)".into(),
            Some(Special::Deleter) => "(deleter)".into(),
            None => "(stringifier)".into(),
        }),
        Member::Constructor(..) => "constructor".into(),
        Member::Stringifier(_) => "(stringifier)".into(),
        Member::Iterable { .. } => "(iterable)".into(),
        Member::Maplike { .. } => "(maplike)".into(),
        Member::Setlike { .. } => "(setlike)".into(),
    }
}

/// Where a specification moved on from the snapshot in `idl/`, or every engine departs from it: (file,
/// text, replacement). SVG Paths made `SVGPathSegment` a dictionary
/// (https://svgwg.org/specs/paths/#InterfaceSVGPathData). The Console Standard types `table()`'s
/// second argument and `dir()`'s second argument, so a call such as `console.dir("%o", x)` would
/// throw a TypeError; every engine's console takes any arguments for both and prints them as they
/// are, and pages call them that way. Gamepad's `GamepadEventInit.gamepad` defaults to null while
/// `GamepadEvent.gamepad` is not nullable; the engines answer null for an event made without one.
const CORRECTIONS: &[(&str, &str, &str)] = &[
    (
        "gamepad.idl",
        "[SameObject] readonly attribute Gamepad gamepad;",
        "[SameObject] readonly attribute Gamepad? gamepad;",
    ),
    (
        "console.idl",
        "undefined table(optional any tabularData, optional sequence<DOMString> properties);",
        "undefined table(any... data);",
    ),
    (
        "console.idl",
        "undefined dir(optional any item, optional object? options);",
        "undefined dir(any... data);",
    ),
    (
    "svg-paths.idl",
    "[LegacyNoInterfaceObject, Exposed=Window]\ninterface SVGPathSegment {\n  attribute DOMString type;\n  attribute FrozenArray<float> values;\n};",
    "dictionary SVGPathSegment {\n  required DOMString type;\n  required sequence<float> values;\n};",
    ),
];

/// Parses every `.idl` file in a directory (sorted by name) and links them. Files that fail to parse are
/// reported in the model's diagnostics and left out.
pub fn load_dir(dir: &std::path::Path) -> std::io::Result<(Model, Vec<String>, usize)> {
    load_dir_with(dir, &[])
}

/// [`load_dir`] plus extra IDL sources (name, text) that are not files of `dir`.
pub fn load_dir_with(
    dir: &std::path::Path,
    extra: &[(String, String)],
) -> std::io::Result<(Model, Vec<String>, usize)> {
    let mut paths: Vec<_> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "idl"))
        .collect();
    paths.sort();
    let count = paths.len();
    let mut files = Vec::new();
    let mut parse_errors = Vec::new();
    for p in paths {
        let name = p.file_name().unwrap().to_string_lossy().into_owned();
        let mut src = std::fs::read_to_string(&p)?;
        for (file, old, new) in CORRECTIONS {
            if *file == name {
                src = src.replace(old, new);
            }
        }
        match crate::parser::parse(&src, &name) {
            Ok(defs) => files.push((name, defs)),
            Err(e) => parse_errors.push(e.to_string()),
        }
    }
    for (name, src) in extra {
        match crate::parser::parse(src, name) {
            Ok(defs) => files.push((name.clone(), defs)),
            Err(e) => parse_errors.push(e.to_string()),
        }
    }
    Ok((Model::link(files), parse_errors, count))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(src: &str) -> Model {
        Model::link(vec![(
            "t.idl".into(),
            crate::parser::parse(src, "t.idl").unwrap(),
        )])
    }

    #[test]
    fn merges_partials_and_mixins() {
        let m = model(
            r#"
            [Exposed=Window] interface Node { readonly attribute unsigned short nodeType; };
            [Exposed=Window] interface Element : Node {};
            partial interface Element { [SecureContext] undefined f(); };
            interface mixin ParentNode { readonly attribute unsigned long childElementCount; };
            Element includes ParentNode;
            typedef (Node or DOMString) NodeOrString;
            [Exposed=(Window,Worker)] interface X { undefined g(NodeOrString? n); };
            "#,
        );
        assert!(m.diagnostics.is_empty(), "{:?}", m.diagnostics);
        let el = &m.interfaces["Element"];
        assert_eq!(el.members.len(), 2);
        assert_eq!(el.members[1].mixin.as_deref(), Some("ParentNode"));
        assert!(el.members[1].exposed.contains("Window"));
        assert_eq!(m.chain("Element").len(), 2);
        let Member::Operation(op) = &m.interfaces["X"].members[0].member else {
            panic!()
        };
        let t = m.resolve(&op.args[0].ty);
        assert!(t.nullable);
        assert!(matches!(t.kind, TypeKind::Union(ref v) if v.len() == 2));
    }

    #[test]
    fn reports_unresolved_types() {
        let m = model("[Exposed=Window] interface A { attribute Nope x; };");
        assert_eq!(m.diagnostics, vec!["A.x: unresolved type Nope".to_string()]);
    }
}
