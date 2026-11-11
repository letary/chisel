//! The scope-hoisting linker + method-granular dead-code elimination.
//!
//! Phase A — **unify identities.** Rewrite each `import`-local and injected free SDK reference to the
//! origin binding's `(name, SyntaxContext)`, so every reference to a logical binding shares one
//! identity across modules.
//! Phase B — **member-aware reachability.** Each class splits into a *core* unit (constructor +
//! instance fields + static fields + superclass), one unit per *static method*, and one per
//! *instance method*. Statics are pulled by `Class.name`; instance methods are pulled by *name
//! presence* — a `.name(...)` access anywhere in reachable code (a computed `obj[x]` access keeps
//! all instance methods, conservatively). Two more things count as naming a member: a destructuring
//! key (`const { velocity } = body` reads the getter) and an object-literal key that matches an
//! instance *setter* somewhere (`Object.assign(inst, { friction })` / `node.aspect(Physics,
//! { friction })` write through it — without this the accessor pair vanished and the write became
//! a silent own property). So an unused `Mesh.sphere`, an unused `Vec3.reflect`, and
//! everything only they pulled in all fall away. An *object facade* — `const X = { … }` of pure
//! properties (`device`, `app`, `Color`) — is split the same way: one unit per property, pulled by
//! `X.name` and by name presence, so `device.language` alone carries no `motion`; any other use of
//! `X` (a bare value, a computed or unknown member) keeps it whole. User modules get the *same* demand-driven DCE — an
//! unused pure declaration / unused export is dropped, its own class members are method-DCE'd — with
//! one difference: a **side-effecting** top-level statement in user code is a root (always kept and
//! emitted), whereas in the SDK (`sideEffects: false`) it is dropped — *unless* it mutates one of
//! that module's own top-level bindings (`X.y = …`, `Object.defineProperty(X, …)`), which makes it
//! part of `X`'s declaration: kept iff `X` is reached, and its references are held back until then.
//!
//! **Name tables and key gates** (`Input.names`, [`Names`]) extend Phase B to what a STRING selects.
//! A *name table* is a facade the SDK looks a run-time string up in (`CATALOG[name]`): its members
//! are pulled by *string presence* — a string literal equal to the member's name anywhere in
//! reachable code (a member's own value included, so `{ push: "pop" }` carries `pop` with `push`) —
//! and by nothing else: a `.push(…)` call is not a name. A template or a `"lit" + x` concatenation
//! keeps every member its literal part begins or ends. A *key gate* is an SDK function that only
//! matters when some object carries a property (`options.transition`): until reachable code WRITES
//! that key — an object-literal key, a `x.key = …` assignment, a class field or accessor, the key as
//! a string — references to the function are held back, and if none ever does they are emitted as
//! references to its declared stand-in. Both hold for names that appear in the bundle's code; a name
//! that only arrives as run-time DATA is not seen (the SDK's lookups warn about it).
//! Phase C — **concatenate** kept items/members in dependency order, unwrapping `export`.
//! Phase D — **hygiene()** finalizes globally-unique names; emit one `Module`.

use std::collections::{BTreeSet, HashMap, HashSet};

use swc_core::atoms::Atom;
use swc_core::common::sync::Lrc;
use swc_core::common::{SourceMap, SyntaxContext, DUMMY_SP};
use swc_core::ecma::ast::*;
use swc_core::ecma::transforms::base::hygiene::hygiene;
use swc_core::ecma::visit::{Visit, VisitMut, VisitMutWith, VisitWith};

use crate::graph::{self, ModuleGraph};
use crate::vocab::Vocab;

/// `Input.names`: what a run-time string selects (see the module docs). Not part of the vocabulary
/// manifest: an older binary ignores the unknown input field and bundles the tables whole.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Names {
    pub tables: Vec<NameTable>,
    pub gates: Vec<NameGate>,
}

/// A name table: the top-level `const <name> = { … }` of `module` (a facade, see `facade_of`).
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct NameTable {
    pub module: String,
    pub name: String,
}

/// A key gate: `name` of `module` is needed only by a bundle that writes the property `key`;
/// otherwise its references become references to `otherwise` (a binding of the same module).
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct NameGate {
    pub module: String,
    pub name: String,
    pub key: String,
    pub otherwise: String,
}

type Key = (Atom, SyntaxContext);

#[derive(Clone)]
struct Target {
    sym: Atom,
    ctxt: SyntaxContext,
}

// ---- Phase A: identity unification -------------------------------------------------------------

struct Relinker<'a> {
    top_ctxt: SyntaxContext,
    unresolved_ctxt: SyntaxContext,
    import_map: &'a HashMap<Atom, Target>,
    sdk_map: &'a HashMap<Atom, Target>,
}

impl Relinker<'_> {
    fn target_for(&self, id: &Ident) -> Option<&Target> {
        if id.ctxt == self.top_ctxt {
            self.import_map.get(&id.sym)
        } else if id.ctxt == self.unresolved_ctxt {
            self.sdk_map.get(&id.sym)
        } else {
            None
        }
    }
}

impl VisitMut for Relinker<'_> {
    fn visit_mut_ident(&mut self, id: &mut Ident) {
        if let Some(t) = self.target_for(id) {
            id.sym = t.sym.clone();
            id.ctxt = t.ctxt;
        }
    }

    /// In `{ user }` the ident is key *and* value — relinking it in place would rename the key.
    /// Expand to `user: <origin>` so only the value side is rewritten.
    fn visit_mut_prop(&mut self, p: &mut Prop) {
        if let Prop::Shorthand(id) = p {
            if self.target_for(id).is_some() {
                let key = PropName::Ident(IdentName::new(id.sym.clone(), id.span));
                let mut value = id.clone();
                self.visit_mut_ident(&mut value);
                *p = Prop::KeyValue(KeyValueProp { key, value: Box::new(Expr::Ident(value)) });
                return;
            }
        }
        p.visit_mut_children_with(self);
    }
}

// ---- reference classification ------------------------------------------------------------------

#[derive(Clone)]
enum Edge {
    Decl(Key),            // ref to a non-class top-level binding
    Core(Key),            // `new C(...)` — pulls the class core
    Escape(Key),          // bare class ref (value / extends / instanceof / computed) — pulls core + all statics
    Static(Key, Atom),    // `C.name` — a static member
    Instance(Atom),       // `obj.name` on a non-class object — marks the instance-method name live
    AllInstance,          // `obj[expr]` — dynamic access keeps all instance methods
    ObjMember(Key, Atom), // `X.name` on an object facade — that property
    ObjEscape(Key),       // any other use of a facade (bare value, computed / unknown member, `new`) — whole
    Str(Atom),            // a string that names a member of a name table
    KeyWrite(Atom),       // a write of a gate's property key
    Gated(Key),           // ref to a gated function — a `Decl` once its key is written
}

#[derive(Clone, PartialEq, Eq, Hash)]
enum Unit {
    Decl(Key),
    Core(Key),
    Static(Key, Atom),
    Instance(Key, Atom),
    ObjMember(Key, Atom),
}

enum Task {
    Reach(Unit),
    Live(Atom),
    AllLive,
    Str(Atom),
    Key(Atom),
    Gated(Key),
}

/// What the collector classifies references against.
struct Cx<'a> {
    class_keys: &'a HashSet<Key>,
    binding_keys: &'a HashSet<Key>,
    /// Every name that is an instance **setter** on some class (see `visit_prop`).
    setter_names: &'a HashSet<Atom>,
    /// The object facades (see `facade_of`) and their property names.
    obj_members: &'a HashMap<Key, Vec<Atom>>,
    /// The facades that are name tables, and every member name of one.
    tables: &'a HashSet<Key>,
    table_names: &'a HashSet<Atom>,
    /// The gated functions (→ their key and stand-in), and every gate's key.
    gates: &'a HashMap<Key, (Atom, Key)>,
    gate_keys: &'a HashSet<Atom>,
}

struct Collector<'a> {
    cx: &'a Cx<'a>,
    out: Vec<Edge>,
}

impl Collector<'_> {
    /// A whole string: the name of a table member, the key of a gate.
    fn note_str(&mut self, v: &str) {
        let a = Atom::from(v);
        if self.cx.table_names.contains(&a) {
            self.out.push(Edge::Str(a.clone()));
        }
        if self.cx.gate_keys.contains(&a) {
            self.out.push(Edge::KeyWrite(a));
        }
    }

    /// A literal PART of a string built at run time (`\`slide-from-${side}\``, `"in" + curve`): every
    /// name it begins or ends may be the result.
    fn note_part(&mut self, part: &str) {
        if part.is_empty() {
            return;
        }
        for name in self.cx.table_names {
            if name.starts_with(part) || name.ends_with(part) {
                self.out.push(Edge::Str(name.clone()));
            }
        }
        for key in self.cx.gate_keys {
            if key.starts_with(part) || key.ends_with(part) {
                self.out.push(Edge::KeyWrite(key.clone()));
            }
        }
    }

    fn note_key(&mut self, name: &Atom) {
        if self.cx.gate_keys.contains(name) {
            self.out.push(Edge::KeyWrite(name.clone()));
        }
    }
}

fn str_lit(e: &Expr) -> Option<&str> {
    match e {
        Expr::Lit(Lit::Str(s)) => s.value.as_str(),
        Expr::Paren(p) => str_lit(&p.expr),
        _ => None,
    }
}

impl Visit for Collector<'_> {
    fn visit_ident(&mut self, id: &Ident) {
        let k = (id.sym.clone(), id.ctxt);
        if self.cx.class_keys.contains(&k) {
            self.out.push(Edge::Escape(k));
        } else if self.cx.obj_members.contains_key(&k) {
            self.out.push(Edge::ObjEscape(k));
        } else if self.cx.gates.contains_key(&k) {
            self.out.push(Edge::Gated(k));
        } else if self.cx.binding_keys.contains(&k) {
            self.out.push(Edge::Decl(k));
        }
    }

    fn visit_new_expr(&mut self, n: &NewExpr) {
        if let Expr::Ident(callee) = &*n.callee {
            let k = (callee.sym.clone(), callee.ctxt);
            if self.cx.obj_members.contains_key(&k) {
                self.out.push(Edge::ObjEscape(k));
                visit_args(self, &n.args);
                return;
            } else if self.cx.class_keys.contains(&k) {
                self.out.push(Edge::Core(k));
                visit_args(self, &n.args);
                return;
            } else if self.cx.binding_keys.contains(&k) {
                self.out.push(if self.cx.gates.contains_key(&k) { Edge::Gated(k) } else { Edge::Decl(k) });
                visit_args(self, &n.args);
                return;
            }
        }
        n.visit_children_with(self);
    }

    fn visit_member_expr(&mut self, m: &MemberExpr) {
        if let Expr::Ident(obj) = &*m.obj {
            let k = (obj.sym.clone(), obj.ctxt);
            if self.cx.class_keys.contains(&k) {
                match &m.prop {
                    MemberProp::Ident(name) => self.out.push(Edge::Static(k, name.sym.clone())),
                    MemberProp::Computed(c) => {
                        self.out.push(Edge::Escape(k));
                        c.expr.visit_with(self);
                    }
                    MemberProp::PrivateName(_) => self.out.push(Edge::Escape(k)),
                }
                return;
            }
            // A name table: `T.name` is that member, `T[key]` is a LOOKUP — the table itself, its
            // members by the strings that name them (`Task::Str`); a member name is not presence.
            if self.cx.tables.contains(&k) {
                let members = &self.cx.obj_members[&k];
                match &m.prop {
                    MemberProp::Ident(name) if members.contains(&name.sym) => self.out.push(Edge::ObjMember(k, name.sym.clone())),
                    MemberProp::Computed(c) => match str_lit(&c.expr) {
                        Some(name) if members.contains(&Atom::from(name)) => self.out.push(Edge::ObjMember(k, Atom::from(name))),
                        Some(_) => self.out.push(Edge::ObjEscape(k)),
                        None => {
                            self.out.push(Edge::Decl(k));
                            c.expr.visit_with(self);
                        }
                    },
                    _ => self.out.push(Edge::ObjEscape(k)),
                }
                return;
            }
            if let Some(members) = self.cx.obj_members.get(&k) {
                let name = match &m.prop {
                    MemberProp::Ident(name) => Some(name.sym.clone()),
                    MemberProp::Computed(c) => match &*c.expr {
                        Expr::Lit(Lit::Str(s)) => Some(Atom::from(s.value.as_str().unwrap_or(""))),
                        other => {
                            other.visit_with(self);
                            None
                        }
                    },
                    MemberProp::PrivateName(_) => None,
                };
                match name {
                    // The property itself — and its name as presence, so a same-named class
                    // instance member stays live exactly as before facades were split.
                    Some(name) if members.contains(&name) => {
                        self.out.push(Edge::ObjMember(k, name.clone()));
                        self.out.push(Edge::Instance(name));
                    }
                    // A name the literal does not declare (`X.hasOwnProperty`, a property attached by
                    // a later `X.y = …`) or a dynamic key: nothing can be proved → keep it whole.
                    _ => self.out.push(Edge::ObjEscape(k)),
                }
                return;
            }
        }
        // member access on an instance / `this` / nested expression.
        match &m.prop {
            MemberProp::Ident(name) => {
                self.out.push(Edge::Instance(name.sym.clone()));
                m.obj.visit_with(self);
            }
            MemberProp::Computed(c) => match &*c.expr {
                // A string-literal key names a specific member.
                Expr::Lit(Lit::Str(s)) => {
                    self.out.push(Edge::Instance(Atom::from(s.value.as_str().unwrap_or(""))));
                    // …and is a string like any other (`o["transition"] = …` writes a gate's key)
                    self.note_str(s.value.as_str().unwrap_or(""));
                    m.obj.visit_with(self);
                }
                // Any other computed *read* (`v[0]`, `store[k]`, `m.m[12]`) is data access — it can't
                // dispatch a named method. (A dynamic *call* `obj[k](...)` is caught in visit_call_expr.)
                other => {
                    m.obj.visit_with(self);
                    other.visit_with(self);
                }
            },
            MemberProp::PrivateName(_) => m.obj.visit_with(self),
        }
    }

    fn visit_call_expr(&mut self, n: &CallExpr) {
        // A dynamic computed call `obj[k](...)` could dispatch any method name → keep them all.
        // (Numeric/string-literal keys are not dynamic; string keys are handled as member reads.)
        if let Callee::Expr(callee) = &n.callee {
            if let Expr::Member(MemberExpr { prop: MemberProp::Computed(c), .. }) = &**callee {
                if !matches!(&*c.expr, Expr::Lit(Lit::Num(_)) | Expr::Lit(Lit::Str(_))) {
                    self.out.push(Edge::AllInstance);
                }
            }
        }
        n.visit_children_with(self);
    }

    /// An object-literal key is a write-by-name waiting to happen: `Object.assign(inst, { friction:
    /// 0.02 })`, `node.aspect(Physics, { friction })`, `{ ...defaults, friction }` all reach a same-
    /// named **setter** on whatever instance the object lands on. A plain method would merely be
    /// overwritten by such a write, but a dropped setter turns it into a silent own data property —
    /// so a literal key marks the name live iff some class declares an instance setter of that name.
    /// (A computed key `{ [k]: v }` stays data access, like a computed read.)
    fn visit_prop(&mut self, p: &Prop) {
        if let Some(name) = prop_key_name(p) {
            self.note_key(&name);
            if self.cx.setter_names.contains(&name) {
                self.out.push(Edge::Instance(name));
            }
        }
        p.visit_children_with(self);
    }

    /// `x.key = …` writes a gate's key like an object-literal key does.
    fn visit_assign_expr(&mut self, n: &AssignExpr) {
        if let AssignTarget::Simple(SimpleAssignTarget::Member(MemberExpr { prop: MemberProp::Ident(name), .. })) = &n.left {
            self.note_key(&name.sym);
        }
        n.visit_children_with(self);
    }

    /// A class field of a gate's key: an instance handed over as the options object carries it.
    fn visit_class_prop(&mut self, p: &ClassProp) {
        if let Some(name) = simple_prop_name(&p.key) {
            self.note_key(&name);
        }
        p.visit_children_with(self);
    }

    /// A string in expression position (never a property key: `PropName::Str` is not a `Lit`).
    fn visit_lit(&mut self, l: &Lit) {
        if let Lit::Str(s) = l {
            if let Some(v) = s.value.as_str() {
                self.note_str(v);
            }
        }
    }

    fn visit_tpl(&mut self, t: &Tpl) {
        let text = |q: &TplElement| q.cooked.as_ref().and_then(|c| c.as_str().map(str::to_string)).unwrap_or_else(|| q.raw.to_string());
        if t.exprs.is_empty() {
            if let Some(q) = t.quasis.first() {
                self.note_str(&text(q));
            }
        } else {
            for q in &t.quasis {
                self.note_part(&text(q));
            }
        }
        t.visit_children_with(self);
    }

    fn visit_bin_expr(&mut self, b: &BinExpr) {
        if b.op == BinaryOp::Add {
            for (lit, other) in [(&b.left, &b.right), (&b.right, &b.left)] {
                if let (Some(part), None) = (str_lit(lit), str_lit(other)) {
                    self.note_part(part);
                }
            }
        }
        b.visit_children_with(self);
    }

    /// Destructuring reads members by name exactly like `.name` does: `const { velocity } = body`
    /// invokes the `velocity` getter, `const { spin: s } = wheel` reads the `spin` method.
    fn visit_object_pat_prop(&mut self, p: &ObjectPatProp) {
        match p {
            ObjectPatProp::KeyValue(kv) => {
                if let Some(name) = simple_prop_name(&kv.key) {
                    self.out.push(Edge::Instance(name));
                }
            }
            ObjectPatProp::Assign(a) => self.out.push(Edge::Instance(a.key.sym.clone())),
            ObjectPatProp::Rest(_) => {}
        }
        p.visit_children_with(self);
    }
}

fn visit_args(c: &mut Collector, args: &Option<Vec<ExprOrSpread>>) {
    if let Some(args) = args {
        for a in args {
            a.visit_with(c);
        }
    }
}

fn edges_of(node_visit: impl Fn(&mut Collector), cx: &Cx) -> Vec<Edge> {
    let mut c = Collector { cx, out: Vec::new() };
    node_visit(&mut c);
    c.out
}

/// An *object facade*: `const X = { … }` — one declarator, an object literal with no spread whose
/// every property has a literal name and a pure value (a method, an accessor, a literal, an
/// identifier, an arrow, a nested pure object). Its properties become reachability units of their
/// own (`Unit::ObjMember`), pulled the way a class's instance members are: by `X.name` and by name
/// presence (`this.name`, destructuring). A getter + setter pair is one unit. Anything else that
/// touches `X` — a bare value (`Object.keys(X)`, `{ ...X }`, passing it on), a computed or unknown
/// member, `new X` — keeps it whole.
fn facade_of(decl: &Decl, ctxt: SyntaxContext) -> Option<(Key, Vec<Atom>)> {
    let Decl::Var(v) = decl else { return None };
    if v.decls.len() != 1 {
        return None;
    }
    let d = &v.decls[0];
    let Pat::Ident(b) = &d.name else { return None };
    let Expr::Object(obj) = unparen(d.init.as_deref()?) else { return None };
    let names = facade_members(obj)?;
    if names.is_empty() {
        return None;
    }
    Some(((b.id.sym.clone(), ctxt), names))
}

fn facade_members(obj: &ObjectLit) -> Option<Vec<Atom>> {
    let mut names: Vec<Atom> = Vec::new();
    for p in &obj.props {
        let PropOrSpread::Prop(prop) = p else { return None };
        if !prop_is_pure(prop) {
            return None;
        }
        let name = prop_key_name(prop)?;
        if !names.contains(&name) {
            names.push(name);
        }
    }
    Some(names)
}

fn simple_prop_name(p: &PropName) -> Option<Atom> {
    match p {
        PropName::Ident(i) => Some(i.sym.clone()),
        PropName::Str(s) => Some(Atom::from(s.value.as_str().unwrap_or(""))),
        _ => None,
    }
}

/// The literal (non-computed) key of an object-literal property, whatever its form.
fn prop_key_name(p: &Prop) -> Option<Atom> {
    match p {
        Prop::Shorthand(id) => Some(id.sym.clone()),
        Prop::Assign(a) => Some(a.key.sym.clone()),
        Prop::KeyValue(kv) => simple_prop_name(&kv.key),
        Prop::Getter(g) => simple_prop_name(&g.key),
        Prop::Setter(s) => simple_prop_name(&s.key),
        Prop::Method(m) => simple_prop_name(&m.key),
    }
}

/// `(name, is_static)` for an addressable class method, else `None` (constructor, field, private,
/// computed-name method, static block — all of which stay in the class core).
fn method_name(m: &ClassMember) -> Option<(Atom, bool)> {
    match m {
        ClassMember::Method(mm) => simple_prop_name(&mm.key).map(|n| (n, mm.is_static)),
        _ => None,
    }
}

// ---- top-level item classification (drives both seeding and emit) ------------------------------

/// How a top-level item participates in DCE.
enum ItemKind {
    /// import / re-export, or a side-effect statement in an SDK (`sideEffects: false`) module → strip.
    Drop,
    /// A side-effecting top-level statement in *user* code → always emitted; its refs seed roots.
    SideEffect,
    /// A pure declaration (function / class / simple-binding const with a pure initializer) → becomes
    /// a reachability unit, kept only if something live references it.
    PureDecl,
    /// An SDK top-level statement that *mutates one of this module's own top-level bindings*
    /// (`X.y = …`, `X.a.b = …`, `Object.defineProperty(X, …)`). Semantically it is part of `X`'s
    /// declaration, so it rides along with it: kept iff `X` is reached, dropped otherwise. Without
    /// this the `sideEffects: false` contract deletes it unconditionally and `X.y` is `undefined`
    /// at runtime — silently, and only in bundled builds.
    AttachedTo(Key),
}

fn classify_item(item: &ModuleItem, is_user: bool, ctxt: SyntaxContext) -> ItemKind {
    match item {
        ModuleItem::Stmt(Stmt::Decl(d)) => classify_decl(d, is_user),
        ModuleItem::ModuleDecl(ModuleDecl::ExportDecl(e)) => classify_decl(&e.decl, is_user),
        // imports + re-exports carry no runtime behavior we keep at this stage.
        ModuleItem::ModuleDecl(_) => ItemKind::Drop,
        // any other top-level statement (expr / if / loop / ...) is a side effect in user code. In
        // the SDK it is dead *unless* it mutates a binding this module declares, in which case it
        // is attached to that binding rather than dropped.
        ModuleItem::Stmt(_) => {
            if is_user {
                ItemKind::SideEffect
            } else {
                match attach_anchor(item, ctxt) {
                    Some(k) => ItemKind::AttachedTo(k),
                    None => ItemKind::Drop,
                }
            }
        }
    }
}

fn unparen(e: &Expr) -> &Expr {
    match e {
        Expr::Paren(p) => unparen(&p.expr),
        other => other,
    }
}

/// A top-level binding of *this* module (same `SyntaxContext`). Cross-module anchors are rejected:
/// after identity unification such an ident carries the origin module's ctxt, and attaching to it
/// would make emit order depend on module ordering rather than plain source order.
fn same_module_binding(e: &Expr, ctxt: SyntaxContext) -> Option<Key> {
    match unparen(e) {
        Expr::Ident(id) if id.ctxt == ctxt => Some((id.sym.clone(), id.ctxt)),
        _ => None,
    }
}

/// Base object of a (possibly nested) member expression: `X.a.b` → `X`.
fn member_base(m: &MemberExpr, ctxt: SyntaxContext) -> Option<Key> {
    match unparen(&m.obj) {
        Expr::Member(inner) => member_base(inner, ctxt),
        other => same_module_binding(other, ctxt),
    }
}

/// The binding a top-level statement mutates, if it is one of the recognised "this is really part of
/// the declaration" shapes. Deliberately narrow: only *mutation of the binding itself* qualifies. A
/// call that merely passes it somewhere (`register(X)`) does not — that is an ordinary side effect,
/// and honouring it would gut the `sideEffects: false` contract.
fn attach_anchor(item: &ModuleItem, ctxt: SyntaxContext) -> Option<Key> {
    let ModuleItem::Stmt(Stmt::Expr(es)) = item else { return None };
    match unparen(&es.expr) {
        // `X.y = …`, `X.a.b = …`, `X.y ??= …`
        Expr::Assign(a) => match &a.left {
            AssignTarget::Simple(SimpleAssignTarget::Member(m)) => member_base(m, ctxt),
            _ => None,
        },
        // `Object.defineProperty(X, …)` and friends — the standard way to attach a live accessor.
        Expr::Call(c) => {
            let Callee::Expr(callee) = &c.callee else { return None };
            let Expr::Member(m) = unparen(callee) else { return None };
            let Expr::Ident(obj) = unparen(&m.obj) else { return None };
            let MemberProp::Ident(name) = &m.prop else { return None };
            if &*obj.sym != "Object"
                || !matches!(
                    &*name.sym,
                    "defineProperty" | "defineProperties" | "assign" | "freeze" | "seal" | "setPrototypeOf"
                )
            {
                return None;
            }
            let first = c.args.first()?;
            if first.spread.is_some() {
                return None;
            }
            same_module_binding(&first.expr, ctxt)
        }
        _ => None,
    }
}

fn classify_decl(d: &Decl, is_user: bool) -> ItemKind {
    match d {
        Decl::Class(_) | Decl::Fn(_) => ItemKind::PureDecl,
        // In the SDK every decl is poolable (the `sideEffects: false` contract). In user code a `var`
        // is only droppable when it's a simple binding with a side-effect-free initializer; otherwise
        // its evaluation might matter, so keep it unconditionally.
        Decl::Var(v) => {
            if !is_user || var_is_pure(v) {
                ItemKind::PureDecl
            } else {
                ItemKind::SideEffect
            }
        }
        // TS-only decls are stripped before linking; treat anything else conservatively (e.g. `using`
        // disposes on scope exit) as a side effect in user code.
        _ => {
            if is_user {
                ItemKind::SideEffect
            } else {
                ItemKind::Drop
            }
        }
    }
}

/// A `var`/`const`/`let` is droppable-if-unused only when every declarator is a plain identifier
/// (destructuring can trigger getters/iterators and isn't catalogued as a binding) with an
/// initializer that has no side effects.
fn var_is_pure(v: &VarDecl) -> bool {
    v.decls
        .iter()
        .all(|d| matches!(d.name, Pat::Ident(_)) && d.init.as_deref().map_or(true, expr_is_pure))
}

/// Conservative "evaluating this expression has no observable side effect" check. Returns `false`
/// for anything that *could* run user code: calls, `new`, assignment, update, `await`/`yield`,
/// tagged templates, spreads, optional chains. Property access is treated as pure — the standard
/// bundler assumption that module-init reads don't hit side-effecting getters.
fn expr_is_pure(e: &Expr) -> bool {
    match e {
        Expr::Lit(_) | Expr::Ident(_) | Expr::This(_) | Expr::Arrow(_) | Expr::Fn(_) | Expr::Class(_) => true,
        Expr::Paren(p) => expr_is_pure(&p.expr),
        // `delete x.y` mutates; every other unary on a pure arg is pure.
        Expr::Unary(u) => !matches!(u.op, UnaryOp::Delete) && expr_is_pure(&u.arg),
        Expr::Bin(b) => expr_is_pure(&b.left) && expr_is_pure(&b.right),
        Expr::Cond(c) => expr_is_pure(&c.test) && expr_is_pure(&c.cons) && expr_is_pure(&c.alt),
        Expr::Seq(s) => s.exprs.iter().all(|x| expr_is_pure(x)),
        // an untagged template with pure interpolations; a *tagged* template is a call (`Expr::TaggedTpl`).
        Expr::Tpl(t) => t.exprs.iter().all(|x| expr_is_pure(x)),
        Expr::Member(m) => {
            expr_is_pure(&m.obj)
                && match &m.prop {
                    MemberProp::Ident(_) | MemberProp::PrivateName(_) => true,
                    MemberProp::Computed(c) => expr_is_pure(&c.expr),
                }
        }
        Expr::Array(a) => a.elems.iter().all(|el| match el {
            None => true,
            Some(ExprOrSpread { spread: None, expr }) => expr_is_pure(expr),
            Some(_) => false, // spread iterates → potential side effect
        }),
        Expr::Object(o) => o.props.iter().all(|p| match p {
            PropOrSpread::Spread(_) => false,
            PropOrSpread::Prop(prop) => prop_is_pure(prop),
        }),
        _ => false,
    }
}

fn prop_is_pure(p: &Prop) -> bool {
    match p {
        Prop::Shorthand(_) => true,
        Prop::KeyValue(kv) => prop_name_is_pure(&kv.key) && expr_is_pure(&kv.value),
        // defining a method / accessor is pure; it doesn't execute its body.
        Prop::Method(m) => prop_name_is_pure(&m.key),
        Prop::Getter(g) => prop_name_is_pure(&g.key),
        Prop::Setter(s) => prop_name_is_pure(&s.key),
        Prop::Assign(_) => false,
    }
}

fn prop_name_is_pure(k: &PropName) -> bool {
    match k {
        PropName::Computed(c) => expr_is_pure(&c.expr),
        _ => true,
    }
}

// ---- the linker --------------------------------------------------------------------------------

pub fn link(
    graph: &mut ModuleGraph,
    entry_id: usize,
    inject_ids: &[usize],
    fuse: bool,
    keep: &[String],
    vocab: &Vocab,
    names: &Names,
    cm: &Lrc<SourceMap>,
    diagnostics: &mut Vec<String>,
) -> anyhow::Result<Module> {
    let n = graph.modules.len();

    // --- SDK global map and per-module import maps (for Phase A). ---
    let mut sdk_map: HashMap<Atom, Target> = HashMap::new();
    for &ij in inject_ids {
        let names: Vec<String> = graph.modules[ij].exports.keys().cloned().collect();
        for name in names {
            if let Some((om, oname)) = graph::resolve_export(graph, ij, &name) {
                sdk_map.insert(name.into(), Target { sym: oname.into(), ctxt: graph.modules[om].top_level_ctxt });
            }
        }
    }
    let import_maps: Vec<HashMap<Atom, Target>> = (0..n)
        .map(|i| {
            let mut map = HashMap::new();
            let edges = graph.modules[i].imports.clone();
            for (target, specs) in &edges {
                for s in specs {
                    if let Some((om, oname)) = graph::resolve_export(graph, *target, &s.imported) {
                        map.insert(s.local.clone().into(), Target { sym: oname.into(), ctxt: graph.modules[om].top_level_ctxt });
                    }
                }
            }
            map
        })
        .collect();

    // --- component writes on SDK-owned vectors (`c.velocity.y = 7` → `__compWrite(c, …)`), before
    // Phase A so the helper references resolve like any other injected global. Off unless the SDK
    // exports both helpers, so an older SDK bundles exactly as before. ---
    let comp_live: HashSet<Atom> = if [&vocab.comp_write.write, &vocab.comp_write.op]
        .iter()
        .all(|h| sdk_map.contains_key(&Atom::from(h.as_str())))
    {
        crate::comp_write::apply(graph, &vocab.comp_write, diagnostics)
    } else {
        HashSet::new()
    };

    // --- Phase A. ---
    for i in 0..n {
        let top_ctxt = graph.modules[i].top_level_ctxt;
        let unresolved_ctxt = graph.modules[i].unresolved_ctxt;
        let mut relinker = Relinker { top_ctxt, unresolved_ctxt, import_map: &import_maps[i], sdk_map: &sdk_map };
        graph.modules[i].module.visit_mut_with(&mut relinker);
    }

    // --- math chain fusion (runs before DCE so fused-away methods become dead). ---
    if fuse {
        let names = &vocab.fusion;
        if let Some(t) = sdk_map.get(&Atom::from(names.vec3.class.as_str())) {
            let key = (t.sym.clone(), t.ctxt);
            for i in 0..n {
                crate::fusion::fuse_module(&mut graph.modules[i].module, key.clone(), &names.vec3);
            }
        }
        // Date chains: the `date()` value type. Enabled only when its module still declares the
        // internal helpers the lowering emits references to (so it degrades safely if datetime.ts
        // is refactored). All date-module top-levels share `date`'s top_level_ctxt.
        if let Some(t) = sdk_map.get(&Atom::from(names.date.factory.as_str())) {
            let ctxt = t.ctxt;
            let mut declared: HashSet<Atom> = HashSet::new();
            for m in &graph.modules {
                if m.top_level_ctxt != ctxt {
                    continue;
                }
                for item in &m.module.body {
                    if let Some(decl) = item_decl(item) {
                        for k in binding_keys_of_decl(decl, ctxt) {
                            declared.insert(k.0);
                        }
                    }
                }
            }
            let d = &names.date;
            let need = [&d.class, &d.to_ms, &d.format_impl, &d.time_ago_impl];
            if need.iter().all(|nm| declared.contains(&Atom::from(nm.as_str()))) {
                let ctx = crate::fusion::DateCtx {
                    date: (t.sym.clone(), ctxt),
                    date_value: (Atom::from(d.class.as_str()), ctxt),
                    to_ms: (Atom::from(d.to_ms.as_str()), ctxt),
                    format_impl: (Atom::from(d.format_impl.as_str()), ctxt),
                    time_ago_impl: (Atom::from(d.time_ago_impl.as_str()), ctxt),
                };
                for i in 0..n {
                    crate::fusion::fuse_dates_module(&mut graph.modules[i].module, &ctx, d);
                }
            }
        }
    }

    // --- particle curve chains (`curve()` / `colorCurve()`). Pure data assembly, so unlike the math
    // fusers this pass also *validates*: a literal chain native would reject at set time becomes a
    // build-time diagnostic. Validation runs always; the rewrite to a flat `_data` buffer only under
    // `fuse` (and, like the other fusers, before DCE so a fully-fused bundle drops the builders). ---
    {
        let names = &vocab.fusion.curve;
        let key_of = |name: &str| sdk_map.get(&Atom::from(name)).map(|t| (t.sym.clone(), t.ctxt));
        let ctx = crate::curve::CurveCtx { curve: key_of(&names.factory), color_curve: key_of(&names.color_factory) };
        if !ctx.is_empty() {
            for i in 0..n {
                crate::curve::fuse_curves_module(&mut graph.modules[i].module, &ctx, names, fuse, cm, diagnostics);
            }
        }
    }

    let user: HashSet<usize> = reachable_from(graph, entry_id);

    // --- catalog every top-level binding + per-class member names. ---
    let mut binding_keys: HashSet<Key> = HashSet::new();
    let mut class_keys: HashSet<Key> = HashSet::new();
    let mut class_statics: HashMap<Key, HashSet<Atom>> = HashMap::new();
    let mut class_instance: HashMap<Key, Vec<Atom>> = HashMap::new();
    let mut classes_with_instance: HashMap<Atom, Vec<Key>> = HashMap::new();
    // Instance setter names across all classes — the only member kind an object-literal key can reach.
    let mut setter_names: HashSet<Atom> = HashSet::new();
    // Object facades (`facade_of`): their property names, and the facades declaring each name.
    let mut obj_members: HashMap<Key, Vec<Atom>> = HashMap::new();
    let mut objects_with_member: HashMap<Atom, Vec<Key>> = HashMap::new();
    for m in &graph.modules {
        let ctxt = m.top_level_ctxt;
        for item in &m.module.body {
            if let Some(decl) = item_decl(item) {
                for k in binding_keys_of_decl(decl, ctxt) {
                    binding_keys.insert(k);
                }
                if let Some((key, names)) = facade_of(decl, ctxt) {
                    for name in &names {
                        objects_with_member.entry(name.clone()).or_default().push(key.clone());
                    }
                    obj_members.insert(key, names);
                }
                if let Decl::Class(cd) = decl {
                    let key = (cd.ident.sym.clone(), ctxt);
                    class_keys.insert(key.clone());
                    let statics = class_statics.entry(key.clone()).or_default();
                    let instance = class_instance.entry(key.clone()).or_default();
                    for mem in &cd.class.body {
                        if let Some((name, is_static)) = method_name(mem) {
                            if is_static {
                                statics.insert(name);
                            } else {
                                if let ClassMember::Method(mm) = mem {
                                    if mm.kind == MethodKind::Setter {
                                        setter_names.insert(name.clone());
                                    }
                                }
                                if !instance.contains(&name) {
                                    instance.push(name.clone());
                                    classes_with_instance.entry(name).or_default().push(key.clone());
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // --- name tables and key gates (`Input.names`): an entry that names no module of this bundle, a
    // binding that is not there or a table that is not a facade is skipped — the code bundles whole. ---
    let key_in = |module: &str, name: &str| -> Option<Key> {
        let m = &graph.modules[*graph.path_to_id.get(module)?];
        let k = (Atom::from(name), m.top_level_ctxt);
        binding_keys.contains(&k).then_some(k)
    };
    let mut tables: HashSet<Key> = HashSet::new();
    let mut table_names: HashSet<Atom> = HashSet::new();
    let mut tables_with_name: HashMap<Atom, Vec<Key>> = HashMap::new();
    for t in &names.tables {
        let Some(k) = key_in(&t.module, &t.name) else { continue };
        let Some(members) = obj_members.get(&k) else { continue };
        if tables.insert(k.clone()) {
            for name in members {
                table_names.insert(name.clone());
                tables_with_name.entry(name.clone()).or_default().push(k.clone());
            }
        }
    }
    // A table's members answer to strings alone, never to member-name presence.
    for facades in objects_with_member.values_mut() {
        facades.retain(|k| !tables.contains(k));
    }
    let mut gates: HashMap<Key, (Atom, Key)> = HashMap::new();
    let mut gate_keys: HashSet<Atom> = HashSet::new();
    for g in &names.gates {
        let (Some(f), Some(otherwise)) = (key_in(&g.module, &g.name), key_in(&g.module, &g.otherwise)) else { continue };
        if g.key.is_empty() || class_keys.contains(&f) || obj_members.contains_key(&f) {
            continue;
        }
        gate_keys.insert(Atom::from(g.key.as_str()));
        gates.insert(f, (Atom::from(g.key.as_str()), otherwise));
    }
    let cx = Cx {
        class_keys: &class_keys,
        binding_keys: &binding_keys,
        setter_names: &setter_names,
        obj_members: &obj_members,
        tables: &tables,
        table_names: &table_names,
        gates: &gates,
        gate_keys: &gate_keys,
    };

    // --- build reachability units + seed roots from user code. ---
    let mut unit_edges: HashMap<Unit, Vec<Edge>> = HashMap::new();
    let mut roots: Vec<Edge> = Vec::new();
    // Edges owed by SDK statements that mutate a binding — drained into the worklist if and when
    // that binding is reached (see `ItemKind::AttachedTo`).
    let mut pending: HashMap<Key, Vec<Edge>> = HashMap::new();

    for (mid, m) in graph.modules.iter().enumerate() {
        let is_user = user.contains(&mid);
        let ctxt = m.top_level_ctxt;
        for item in &m.module.body {
            match classify_item(item, is_user, ctxt) {
                ItemKind::Drop => continue,
                // A user side effect must run → its references are reachability roots.
                ItemKind::SideEffect => {
                    roots.extend(edges_of(|c| item.visit_with(c), &cx));
                }
                // Not a root: it only runs if its anchor survives, so its refs are held back until
                // then — otherwise attaching a statement would drag its whole reference set in.
                ItemKind::AttachedTo(anchor) => {
                    let e = edges_of(|c| item.visit_with(c), &cx);
                    pending.entry(anchor).or_default().extend(e);
                }
                // A pure decl becomes unit(s); its references are pulled only if the unit is reached.
                ItemKind::PureDecl => match item_decl(item) {
                    Some(Decl::Class(cd)) => {
                        let key = (cd.ident.sym.clone(), ctxt);
                        let mut core = Vec::new();
                        if let Some(sc) = &cd.class.super_class {
                            core.extend(edges_of(|c| sc.visit_with(c), &cx));
                        }
                        for mem in &cd.class.body {
                            match method_name(mem) {
                                Some((name, is_static)) => {
                                    if let ClassMember::Method(mm) = mem {
                                        let mut e = edges_of(|c| mm.function.visit_with(c), &cx);
                                        // An ACCESSOR of a gate's key: an instance handed over as the options
                                        // object answers the read. (A plain method of that name is no value.)
                                        if gate_keys.contains(&name) && mm.kind != MethodKind::Method {
                                            e.push(Edge::KeyWrite(name.clone()));
                                        }
                                        let unit = if is_static {
                                            Unit::Static(key.clone(), name)
                                        } else {
                                            Unit::Instance(key.clone(), name)
                                        };
                                        unit_edges.entry(unit).or_default().extend(e);
                                    }
                                }
                                None => core.extend(edges_of(|c| mem.visit_with(c), &cx)),
                            }
                        }
                        unit_edges.entry(Unit::Core(key)).or_default().extend(core);
                    }
                    Some(d) => match facade_of(d, ctxt) {
                        // A facade: the declaration itself owes nothing, each property is a unit.
                        Some((key, _)) => {
                            unit_edges.entry(Unit::Decl(key.clone())).or_default();
                            let Decl::Var(v) = d else { unreachable!() };
                            if let Some(Expr::Object(obj)) = v.decls[0].init.as_deref().map(unparen) {
                                for p in &obj.props {
                                    if let PropOrSpread::Prop(prop) = p {
                                        if let Some(name) = prop_key_name(prop) {
                                            let e = edges_of(|c| prop.visit_with(c), &cx);
                                            unit_edges.entry(Unit::ObjMember(key.clone(), name)).or_default().extend(e);
                                        }
                                    }
                                }
                            }
                        }
                        None => {
                            let e = edges_of(|c| item.visit_with(c), &cx);
                            for k in binding_keys_of_decl(d, ctxt) {
                                unit_edges.entry(Unit::Decl(k)).or_default().extend(e.clone());
                            }
                        }
                    },
                    None => {}
                },
            }
        }
    }

    // --- propagate (units + live instance-method names). ---
    let mut reached: HashSet<Unit> = HashSet::new();
    let mut live: HashSet<Atom> = HashSet::new();
    let mut all_instance = false;
    // The strings that name table members, the gate keys that are written, and the gated functions
    // referenced while their key was not.
    let mut live_strs: HashSet<Atom> = HashSet::new();
    let mut live_keys: HashSet<Atom> = HashSet::new();
    let mut gated_refs: HashSet<Key> = HashSet::new();
    let mut gates_settled = false;
    let mut work: Vec<Task> = Vec::new();
    for e in &roots {
        edge_task(e, &class_statics, &obj_members, &mut work);
    }
    // `keep` — method names the host calls by name (no in-bundle caller), so presence-gating can't
    // see them. Mark them live so any *reached* class that has them retains them (unreached classes
    // still drop entirely). An entry ending in `*` is a prefix (`_*` keeps every `_`-prefixed method).
    for pat in keep {
        match pat.strip_suffix('*') {
            Some(prefix) => {
                for name in classes_with_instance.keys() {
                    if name.starts_with(prefix) {
                        work.push(Task::Live(name.clone()));
                    }
                }
            }
            None => work.push(Task::Live(Atom::from(pat.as_str()))),
        }
    }
    // Getters the comp_write pass rewrote out of user code: the helper's fallback reads them by a
    // computed key, which DCE cannot see, so they stay live by name.
    for name in &comp_live {
        work.push(Task::Live(name.clone()));
    }
    loop {
    while let Some(t) = work.pop() {
        match t {
            Task::Reach(u) => {
                if !reached.insert(u.clone()) {
                    continue;
                }
                match &u {
                    Unit::Static(k, _) | Unit::Instance(k, _) => work.push(Task::Reach(Unit::Core(k.clone()))),
                    Unit::ObjMember(k, _) => work.push(Task::Reach(Unit::Decl(k.clone()))),
                    Unit::Core(k) => {
                        if let Some(names) = class_instance.get(k) {
                            for name in names {
                                if all_instance || live.contains(name) {
                                    work.push(Task::Reach(Unit::Instance(k.clone(), name.clone())));
                                }
                            }
                        }
                    }
                    // A facade going live brings the properties whose names are already live — a
                    // name table's by the strings that name them.
                    Unit::Decl(k) => {
                        if let Some(names) = obj_members.get(k) {
                            let table = tables.contains(k);
                            for name in names {
                                if if table { live_strs.contains(name) } else { all_instance || live.contains(name) } {
                                    work.push(Task::Reach(Unit::ObjMember(k.clone(), name.clone())));
                                }
                            }
                        }
                    }
                }
                // A binding going live pulls the top-level statements that mutate it (and, only
                // now, whatever those statements reference).
                if let Unit::Decl(k) | Unit::Core(k) = &u {
                    if let Some(edges) = pending.remove(k) {
                        for e in &edges {
                            edge_task(e, &class_statics, &obj_members, &mut work);
                        }
                    }
                }
                if let Some(edges) = unit_edges.get(&u) {
                    for e in edges.clone() {
                        edge_task(&e, &class_statics, &obj_members, &mut work);
                    }
                }
            }
            Task::Live(name) => {
                if all_instance || !live.insert(name.clone()) {
                    continue;
                }
                if let Some(classes) = classes_with_instance.get(&name) {
                    for k in classes {
                        if reached.contains(&Unit::Core(k.clone())) {
                            work.push(Task::Reach(Unit::Instance(k.clone(), name.clone())));
                        }
                    }
                }
                if let Some(objs) = objects_with_member.get(&name) {
                    for k in objs {
                        if reached.contains(&Unit::Decl(k.clone())) {
                            work.push(Task::Reach(Unit::ObjMember(k.clone(), name.clone())));
                        }
                    }
                }
            }
            Task::AllLive => {
                if all_instance {
                    continue;
                }
                all_instance = true;
                for (k, names) in &class_instance {
                    if reached.contains(&Unit::Core(k.clone())) {
                        for name in names {
                            work.push(Task::Reach(Unit::Instance(k.clone(), name.clone())));
                        }
                    }
                }
                for (k, names) in &obj_members {
                    if !tables.contains(k) && reached.contains(&Unit::Decl(k.clone())) {
                        for name in names {
                            work.push(Task::Reach(Unit::ObjMember(k.clone(), name.clone())));
                        }
                    }
                }
            }
            Task::Str(name) => {
                if !live_strs.insert(name.clone()) {
                    continue;
                }
                if let Some(owners) = tables_with_name.get(&name) {
                    for k in owners {
                        if reached.contains(&Unit::Decl(k.clone())) {
                            work.push(Task::Reach(Unit::ObjMember(k.clone(), name.clone())));
                        }
                    }
                }
            }
            Task::Key(key) => {
                if !live_keys.insert(key.clone()) {
                    continue;
                }
                for f in &gated_refs {
                    if gates[f].0 == key {
                        work.push(Task::Reach(Unit::Decl(f.clone())));
                    }
                }
            }
            Task::Gated(f) => {
                if live_keys.contains(&gates[&f].0) {
                    work.push(Task::Reach(Unit::Decl(f)));
                } else {
                    gated_refs.insert(f);
                }
            }
        }
    }
    // Nothing more is reachable: a gate whose key was never written closes — its stand-in comes in
    // (and whatever that references; were it to write the key, the gate opens after all).
    if gates_settled {
        break;
    }
    gates_settled = true;
    for f in &gated_refs {
        if !live_keys.contains(&gates[f].0) {
            work.push(Task::Reach(Unit::Decl(gates[f].1.clone())));
        }
    }
    }
    let closed_gates: HashMap<Key, Key> = gated_refs
        .iter()
        .filter(|f| !reached.contains(&Unit::Decl((*f).clone())))
        .map(|f| (f.clone(), gates[f].1.clone()))
        .collect();

    // --- module ordering (over-approximate deps from full item refs). ---
    let mut key_module: HashMap<Key, usize> = HashMap::new();
    for (mid, m) in graph.modules.iter().enumerate() {
        for k in top_binding_keys(&m.module, m.top_level_ctxt) {
            key_module.insert(k, mid);
        }
    }
    // Ordered sets: the DFS below visits a module's deps in this order, and emit order decides
    // hygiene's renames — a std HashSet (per-process random seed) made bundles differ run to run.
    let mut module_deps: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); n];
    for (mid, m) in graph.modules.iter().enumerate() {
        for item in m.module.body.iter() {
            if !item_is_kept(item, mid, m.top_level_ctxt, &user, &reached) {
                continue;
            }
            let mut rc = RefCollector { keys: &binding_keys, out: Vec::new() };
            item.visit_with(&mut rc);
            for r in rc.out {
                if let Some(&dm) = key_module.get(&r) {
                    if dm != mid {
                        module_deps[mid].insert(dm);
                    }
                }
            }
        }
    }
    let order = topo_order(&module_deps, n);

    // --- Phase C: emit. ---
    let mut body: Vec<ModuleItem> = Vec::new();
    for &mid in &order {
        let m = &graph.modules[mid];
        let is_user = user.contains(&mid);
        let ctxt = m.top_level_ctxt;
        for item in &m.module.body {
            match classify_item(item, is_user, ctxt) {
                ItemKind::Drop => continue,
                ItemKind::SideEffect => body.push(unwrap_export(item)),
                ItemKind::AttachedTo(k) => {
                    if anchor_reached(&k, &reached) {
                        body.push(unwrap_export(item));
                    }
                }
                ItemKind::PureDecl => match item_decl(item) {
                    Some(Decl::Class(cd)) => {
                        let key = (cd.ident.sym.clone(), ctxt);
                        if reached.contains(&Unit::Core(key.clone())) {
                            body.push(emit_class(item, &key, &reached));
                        }
                    }
                    Some(d) => {
                        let present = binding_keys_of_decl(d, ctxt).iter().any(|k| reached.contains(&Unit::Decl(k.clone())));
                        if present {
                            body.push(match facade_of(d, ctxt) {
                                Some((key, _)) => emit_facade(item, &key, &reached),
                                None => unwrap_export(item),
                            });
                        }
                    }
                    None => {}
                },
            }
        }
    }

    // A closed gate: its function was not emitted — every reference to it names the stand-in.
    if !closed_gates.is_empty() {
        let mut rewriter = GateRewriter { closed: &closed_gates };
        for item in body.iter_mut() {
            item.visit_mut_with(&mut rewriter);
        }
    }

    let mut merged = Module { span: DUMMY_SP, body, shebang: None };
    let mut program = Program::Module(std::mem::take(&mut merged));
    program.mutate(hygiene());
    Ok(match program {
        Program::Module(m) => m,
        Program::Script(_) => unreachable!(),
    })
}

fn edge_task(e: &Edge, class_statics: &HashMap<Key, HashSet<Atom>>, obj_members: &HashMap<Key, Vec<Atom>>, work: &mut Vec<Task>) {
    match e {
        Edge::Str(name) => work.push(Task::Str(name.clone())),
        Edge::KeyWrite(key) => work.push(Task::Key(key.clone())),
        Edge::Gated(f) => work.push(Task::Gated(f.clone())),
        Edge::ObjMember(k, name) => work.push(Task::Reach(Unit::ObjMember(k.clone(), name.clone()))),
        Edge::ObjEscape(k) => {
            work.push(Task::Reach(Unit::Decl(k.clone())));
            if let Some(names) = obj_members.get(k) {
                for name in names {
                    work.push(Task::Reach(Unit::ObjMember(k.clone(), name.clone())));
                }
            }
        }
        Edge::Decl(k) => work.push(Task::Reach(Unit::Decl(k.clone()))),
        Edge::Core(k) => work.push(Task::Reach(Unit::Core(k.clone()))),
        Edge::Escape(k) => {
            work.push(Task::Reach(Unit::Core(k.clone())));
            if let Some(statics) = class_statics.get(k) {
                for s in statics {
                    work.push(Task::Reach(Unit::Static(k.clone(), s.clone())));
                }
            }
        }
        Edge::Static(k, name) => {
            if class_statics.get(k).map_or(false, |s| s.contains(name)) {
                work.push(Task::Reach(Unit::Static(k.clone(), name.clone())));
            } else {
                work.push(Task::Reach(Unit::Core(k.clone())));
            }
        }
        Edge::Instance(name) => work.push(Task::Live(name.clone())),
        Edge::AllInstance => work.push(Task::AllLive),
    }
}

/// Rewrites the references to a closed gate's function into references to its stand-in.
struct GateRewriter<'a> {
    closed: &'a HashMap<Key, Key>,
}

impl VisitMut for GateRewriter<'_> {
    fn visit_mut_ident(&mut self, id: &mut Ident) {
        if let Some((sym, ctxt)) = self.closed.get(&(id.sym.clone(), id.ctxt)) {
            id.sym = sym.clone();
            id.ctxt = *ctxt;
        }
    }

    /// `{ f }` is key and value at once (see `Relinker`): only the value side is renamed.
    fn visit_mut_prop(&mut self, p: &mut Prop) {
        if let Prop::Shorthand(id) = p {
            if self.closed.contains_key(&(id.sym.clone(), id.ctxt)) {
                let key = PropName::Ident(IdentName::new(id.sym.clone(), id.span));
                let mut value = id.clone();
                self.visit_mut_ident(&mut value);
                *p = Prop::KeyValue(KeyValueProp { key, value: Box::new(Expr::Ident(value)) });
                return;
            }
        }
        p.visit_mut_children_with(self);
    }
}

// ---- ref collection for module ordering --------------------------------------------------------

struct RefCollector<'a> {
    keys: &'a HashSet<Key>,
    out: Vec<Key>,
}
impl Visit for RefCollector<'_> {
    fn visit_ident(&mut self, id: &Ident) {
        let k = (id.sym.clone(), id.ctxt);
        if self.keys.contains(&k) {
            self.out.push(k);
        }
    }
}

// ---- emit helpers ------------------------------------------------------------------------------

fn item_decl(item: &ModuleItem) -> Option<&Decl> {
    match item {
        ModuleItem::ModuleDecl(ModuleDecl::ExportDecl(e)) => Some(&e.decl),
        ModuleItem::Stmt(Stmt::Decl(d)) => Some(d),
        _ => None,
    }
}

fn unwrap_export(item: &ModuleItem) -> ModuleItem {
    match item {
        ModuleItem::ModuleDecl(ModuleDecl::ExportDecl(e)) => ModuleItem::Stmt(Stmt::Decl(e.decl.clone())),
        other => other.clone(),
    }
}

/// Emit a class keeping core members plus the static/instance methods whose units survived.
fn emit_class(item: &ModuleItem, key: &Key, reached: &HashSet<Unit>) -> ModuleItem {
    let mut decl = match item {
        ModuleItem::ModuleDecl(ModuleDecl::ExportDecl(e)) => e.decl.clone(),
        ModuleItem::Stmt(Stmt::Decl(d)) => d.clone(),
        _ => unreachable!(),
    };
    if let Decl::Class(cd) = &mut decl {
        cd.class.body.retain(|mem| match method_name(mem) {
            Some((name, true)) => reached.contains(&Unit::Static(key.clone(), name)),
            Some((name, false)) => reached.contains(&Unit::Instance(key.clone(), name)),
            None => true,
        });
    }
    ModuleItem::Stmt(Stmt::Decl(decl))
}

/// Emit an object facade keeping the properties whose units survived (a getter + setter pair
/// shares one unit, so both halves stay or go together).
fn emit_facade(item: &ModuleItem, key: &Key, reached: &HashSet<Unit>) -> ModuleItem {
    let mut decl = match item {
        ModuleItem::ModuleDecl(ModuleDecl::ExportDecl(e)) => e.decl.clone(),
        ModuleItem::Stmt(Stmt::Decl(d)) => d.clone(),
        _ => unreachable!(),
    };
    if let Decl::Var(v) = &mut decl {
        if let Some(init) = v.decls[0].init.as_deref_mut() {
            if let Expr::Object(obj) = unparen_mut(init) {
                obj.props.retain(|p| match p {
                    PropOrSpread::Prop(prop) => prop_key_name(prop).map_or(true, |n| reached.contains(&Unit::ObjMember(key.clone(), n))),
                    PropOrSpread::Spread(_) => true,
                });
            }
        }
    }
    ModuleItem::Stmt(Stmt::Decl(decl))
}

fn unparen_mut(e: &mut Expr) -> &mut Expr {
    match e {
        Expr::Paren(p) => unparen_mut(&mut p.expr),
        other => other,
    }
}

/// A binding is live if its decl unit is reached — or, for a class, its core.
fn anchor_reached(k: &Key, reached: &HashSet<Unit>) -> bool {
    reached.contains(&Unit::Decl(k.clone())) || reached.contains(&Unit::Core(k.clone()))
}

fn item_is_kept(item: &ModuleItem, mid: usize, ctxt: SyntaxContext, user: &HashSet<usize>, reached: &HashSet<Unit>) -> bool {
    match classify_item(item, user.contains(&mid), ctxt) {
        ItemKind::Drop => false,
        ItemKind::SideEffect => true,
        ItemKind::AttachedTo(k) => anchor_reached(&k, reached),
        ItemKind::PureDecl => match item_decl(item) {
            Some(Decl::Class(cd)) => reached.contains(&Unit::Core((cd.ident.sym.clone(), ctxt))),
            Some(d) => binding_keys_of_decl(d, ctxt).iter().any(|k| reached.contains(&Unit::Decl(k.clone()))),
            None => false,
        },
    }
}

// ---- shared graph helpers ----------------------------------------------------------------------

fn reachable_from(graph: &ModuleGraph, start: usize) -> HashSet<usize> {
    let mut seen = HashSet::new();
    let mut stack = vec![start];
    while let Some(id) = stack.pop() {
        if !seen.insert(id) {
            continue;
        }
        for (target, _) in &graph.modules[id].imports {
            stack.push(*target);
        }
    }
    seen
}

fn topo_order(deps: &[BTreeSet<usize>], n: usize) -> Vec<usize> {
    let mut visited = vec![false; n];
    let mut order = Vec::with_capacity(n);
    for start in 0..n {
        topo_visit(deps, start, &mut visited, &mut order);
    }
    order
}

fn topo_visit(deps: &[BTreeSet<usize>], id: usize, visited: &mut [bool], order: &mut Vec<usize>) {
    if visited[id] {
        return;
    }
    visited[id] = true;
    for &target in &deps[id] {
        topo_visit(deps, target, visited, order);
    }
    order.push(id);
}

fn binding_keys_of_decl(decl: &Decl, ctxt: SyntaxContext) -> Vec<Key> {
    let mut out = Vec::new();
    match decl {
        Decl::Class(c) => out.push((c.ident.sym.clone(), ctxt)),
        Decl::Fn(f) => out.push((f.ident.sym.clone(), ctxt)),
        Decl::Var(v) => {
            for d in &v.decls {
                if let Pat::Ident(b) = &d.name {
                    out.push((b.id.sym.clone(), ctxt));
                }
            }
        }
        _ => {}
    }
    out
}

fn top_binding_keys(module: &Module, ctxt: SyntaxContext) -> Vec<Key> {
    let mut out = Vec::new();
    for item in &module.body {
        if let Some(d) = item_decl(item) {
            out.extend(binding_keys_of_decl(d, ctxt));
        }
    }
    out
}
