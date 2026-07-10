//! The scope-hoisting linker + method-granular dead-code elimination.
//!
//! Phase A — **unify identities.** Rewrite each `import`-local and injected free SDK reference to the
//! origin binding's `(name, SyntaxContext)`, so every reference to a logical binding shares one
//! identity across modules.
//! Phase B — **member-aware reachability.** Each class splits into a *core* unit (constructor +
//! instance fields + static fields + superclass), one unit per *static method*, and one per
//! *instance method*. Statics are pulled by `Class.name`; instance methods are pulled by *name
//! presence* — a `.name(...)` access anywhere in reachable code (a computed `obj[x]` access keeps
//! all instance methods, conservatively). So an unused `Mesh.sphere`, an unused `Vec3.reflect`, and
//! everything only they pulled in all fall away. User modules get the *same* demand-driven DCE — an
//! unused pure declaration / unused export is dropped, its own class members are method-DCE'd — with
//! one difference: a **side-effecting** top-level statement in user code is a root (always kept and
//! emitted), whereas in the SDK (`sideEffects: false`) it is dropped.
//! Phase C — **concatenate** kept items/members in dependency order, unwrapping `export`.
//! Phase D — **hygiene()** finalizes globally-unique names; emit one `Module`.

use std::collections::{HashMap, HashSet};

use swc_core::atoms::Atom;
use swc_core::common::{SyntaxContext, DUMMY_SP};
use swc_core::ecma::ast::*;
use swc_core::ecma::transforms::base::hygiene::hygiene;
use swc_core::ecma::visit::{Visit, VisitMut, VisitMutWith, VisitWith};

use crate::graph::{self, ModuleGraph};

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

impl VisitMut for Relinker<'_> {
    fn visit_mut_ident(&mut self, id: &mut Ident) {
        if id.ctxt == self.top_ctxt {
            if let Some(t) = self.import_map.get(&id.sym) {
                id.sym = t.sym.clone();
                id.ctxt = t.ctxt;
            }
        } else if id.ctxt == self.unresolved_ctxt {
            if let Some(t) = self.sdk_map.get(&id.sym) {
                id.sym = t.sym.clone();
                id.ctxt = t.ctxt;
            }
        }
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
}

#[derive(Clone, PartialEq, Eq, Hash)]
enum Unit {
    Decl(Key),
    Core(Key),
    Static(Key, Atom),
    Instance(Key, Atom),
}

enum Task {
    Reach(Unit),
    Live(Atom),
    AllLive,
}

struct Collector<'a> {
    class_keys: &'a HashSet<Key>,
    binding_keys: &'a HashSet<Key>,
    out: Vec<Edge>,
}

impl Visit for Collector<'_> {
    fn visit_ident(&mut self, id: &Ident) {
        let k = (id.sym.clone(), id.ctxt);
        if self.class_keys.contains(&k) {
            self.out.push(Edge::Escape(k));
        } else if self.binding_keys.contains(&k) {
            self.out.push(Edge::Decl(k));
        }
    }

    fn visit_new_expr(&mut self, n: &NewExpr) {
        if let Expr::Ident(callee) = &*n.callee {
            let k = (callee.sym.clone(), callee.ctxt);
            if self.class_keys.contains(&k) {
                self.out.push(Edge::Core(k));
                visit_args(self, &n.args);
                return;
            } else if self.binding_keys.contains(&k) {
                self.out.push(Edge::Decl(k));
                visit_args(self, &n.args);
                return;
            }
        }
        n.visit_children_with(self);
    }

    fn visit_member_expr(&mut self, m: &MemberExpr) {
        if let Expr::Ident(obj) = &*m.obj {
            let k = (obj.sym.clone(), obj.ctxt);
            if self.class_keys.contains(&k) {
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
}

fn visit_args(c: &mut Collector, args: &Option<Vec<ExprOrSpread>>) {
    if let Some(args) = args {
        for a in args {
            a.visit_with(c);
        }
    }
}

fn edges_of(node_visit: impl Fn(&mut Collector), class_keys: &HashSet<Key>, binding_keys: &HashSet<Key>) -> Vec<Edge> {
    let mut c = Collector { class_keys, binding_keys, out: Vec::new() };
    node_visit(&mut c);
    c.out
}

fn simple_prop_name(p: &PropName) -> Option<Atom> {
    match p {
        PropName::Ident(i) => Some(i.sym.clone()),
        PropName::Str(s) => Some(Atom::from(s.value.as_str().unwrap_or(""))),
        _ => None,
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
}

fn classify_item(item: &ModuleItem, is_user: bool) -> ItemKind {
    match item {
        ModuleItem::Stmt(Stmt::Decl(d)) => classify_decl(d, is_user),
        ModuleItem::ModuleDecl(ModuleDecl::ExportDecl(e)) => classify_decl(&e.decl, is_user),
        // imports + re-exports carry no runtime behavior we keep at this stage.
        ModuleItem::ModuleDecl(_) => ItemKind::Drop,
        // any other top-level statement (expr / if / loop / ...) is a side effect in user code,
        // and dead in the SDK.
        ModuleItem::Stmt(_) => {
            if is_user {
                ItemKind::SideEffect
            } else {
                ItemKind::Drop
            }
        }
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

pub fn link(graph: &mut ModuleGraph, entry_id: usize, inject_ids: &[usize], fuse: bool, keep: &[String]) -> anyhow::Result<Module> {
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

    // --- Phase A. ---
    for i in 0..n {
        let top_ctxt = graph.modules[i].top_level_ctxt;
        let unresolved_ctxt = graph.modules[i].unresolved_ctxt;
        let mut relinker = Relinker { top_ctxt, unresolved_ctxt, import_map: &import_maps[i], sdk_map: &sdk_map };
        graph.modules[i].module.visit_mut_with(&mut relinker);
    }

    // --- math chain fusion (runs before DCE so fused-away methods become dead). ---
    if fuse {
        if let Some(t) = sdk_map.get(&Atom::from("Vec3")) {
            let key = (t.sym.clone(), t.ctxt);
            for i in 0..n {
                crate::fusion::fuse_module(&mut graph.modules[i].module, key.clone());
            }
        }
        // Date chains: the `date()` value type. Enabled only when its module still declares the
        // internal helpers the lowering emits references to (so it degrades safely if datetime.ts
        // is refactored). All date-module top-levels share `date`'s top_level_ctxt.
        if let Some(t) = sdk_map.get(&Atom::from("date")) {
            let ctxt = t.ctxt;
            let mut names: HashSet<Atom> = HashSet::new();
            for m in &graph.modules {
                if m.top_level_ctxt != ctxt {
                    continue;
                }
                for item in &m.module.body {
                    if let Some(decl) = item_decl(item) {
                        for k in binding_keys_of_decl(decl, ctxt) {
                            names.insert(k.0);
                        }
                    }
                }
            }
            let need = ["DateValue", "toMs", "formatImpl", "timeAgoImpl"];
            if need.iter().all(|nm| names.contains(&Atom::from(*nm))) {
                let ctx = crate::fusion::DateCtx {
                    date: (t.sym.clone(), ctxt),
                    date_value: (Atom::from("DateValue"), ctxt),
                    to_ms: (Atom::from("toMs"), ctxt),
                    format_impl: (Atom::from("formatImpl"), ctxt),
                    time_ago_impl: (Atom::from("timeAgoImpl"), ctxt),
                };
                for i in 0..n {
                    crate::fusion::fuse_dates_module(&mut graph.modules[i].module, &ctx);
                }
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
    for m in &graph.modules {
        let ctxt = m.top_level_ctxt;
        for item in &m.module.body {
            if let Some(decl) = item_decl(item) {
                for k in binding_keys_of_decl(decl, ctxt) {
                    binding_keys.insert(k);
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
                            } else if !instance.contains(&name) {
                                instance.push(name.clone());
                                classes_with_instance.entry(name).or_default().push(key.clone());
                            }
                        }
                    }
                }
            }
        }
    }

    // --- build reachability units + seed roots from user code. ---
    let mut unit_edges: HashMap<Unit, Vec<Edge>> = HashMap::new();
    let mut roots: Vec<Edge> = Vec::new();

    for (mid, m) in graph.modules.iter().enumerate() {
        let is_user = user.contains(&mid);
        let ctxt = m.top_level_ctxt;
        for item in &m.module.body {
            match classify_item(item, is_user) {
                ItemKind::Drop => continue,
                // A user side effect must run → its references are reachability roots.
                ItemKind::SideEffect => {
                    roots.extend(edges_of(|c| item.visit_with(c), &class_keys, &binding_keys));
                }
                // A pure decl becomes unit(s); its references are pulled only if the unit is reached.
                ItemKind::PureDecl => match item_decl(item) {
                    Some(Decl::Class(cd)) => {
                        let key = (cd.ident.sym.clone(), ctxt);
                        let mut core = Vec::new();
                        if let Some(sc) = &cd.class.super_class {
                            core.extend(edges_of(|c| sc.visit_with(c), &class_keys, &binding_keys));
                        }
                        for mem in &cd.class.body {
                            match method_name(mem) {
                                Some((name, is_static)) => {
                                    if let ClassMember::Method(mm) = mem {
                                        let e = edges_of(|c| mm.function.visit_with(c), &class_keys, &binding_keys);
                                        let unit = if is_static {
                                            Unit::Static(key.clone(), name)
                                        } else {
                                            Unit::Instance(key.clone(), name)
                                        };
                                        unit_edges.entry(unit).or_default().extend(e);
                                    }
                                }
                                None => core.extend(edges_of(|c| mem.visit_with(c), &class_keys, &binding_keys)),
                            }
                        }
                        unit_edges.entry(Unit::Core(key)).or_default().extend(core);
                    }
                    Some(d) => {
                        let e = edges_of(|c| item.visit_with(c), &class_keys, &binding_keys);
                        for k in binding_keys_of_decl(d, ctxt) {
                            unit_edges.entry(Unit::Decl(k)).or_default().extend(e.clone());
                        }
                    }
                    None => {}
                },
            }
        }
    }

    // --- propagate (units + live instance-method names). ---
    let mut reached: HashSet<Unit> = HashSet::new();
    let mut live: HashSet<Atom> = HashSet::new();
    let mut all_instance = false;
    let mut work: Vec<Task> = Vec::new();
    for e in &roots {
        edge_task(e, &class_statics, &mut work);
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
    while let Some(t) = work.pop() {
        match t {
            Task::Reach(u) => {
                if !reached.insert(u.clone()) {
                    continue;
                }
                match &u {
                    Unit::Static(k, _) | Unit::Instance(k, _) => work.push(Task::Reach(Unit::Core(k.clone()))),
                    Unit::Core(k) => {
                        if let Some(names) = class_instance.get(k) {
                            for name in names {
                                if all_instance || live.contains(name) {
                                    work.push(Task::Reach(Unit::Instance(k.clone(), name.clone())));
                                }
                            }
                        }
                    }
                    Unit::Decl(_) => {}
                }
                if let Some(edges) = unit_edges.get(&u) {
                    for e in edges.clone() {
                        edge_task(&e, &class_statics, &mut work);
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
            }
        }
    }

    // --- module ordering (over-approximate deps from full item refs). ---
    let mut key_module: HashMap<Key, usize> = HashMap::new();
    for (mid, m) in graph.modules.iter().enumerate() {
        for k in top_binding_keys(&m.module, m.top_level_ctxt) {
            key_module.insert(k, mid);
        }
    }
    let mut module_deps: Vec<HashSet<usize>> = vec![HashSet::new(); n];
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
            match classify_item(item, is_user) {
                ItemKind::Drop => continue,
                ItemKind::SideEffect => body.push(unwrap_export(item)),
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
                            body.push(unwrap_export(item));
                        }
                    }
                    None => {}
                },
            }
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

fn edge_task(e: &Edge, class_statics: &HashMap<Key, HashSet<Atom>>, work: &mut Vec<Task>) {
    match e {
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

fn item_is_kept(item: &ModuleItem, mid: usize, ctxt: SyntaxContext, user: &HashSet<usize>, reached: &HashSet<Unit>) -> bool {
    match classify_item(item, user.contains(&mid)) {
        ItemKind::Drop => false,
        ItemKind::SideEffect => true,
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

fn topo_order(deps: &[HashSet<usize>], n: usize) -> Vec<usize> {
    let mut visited = vec![false; n];
    let mut order = Vec::with_capacity(n);
    for start in 0..n {
        topo_visit(deps, start, &mut visited, &mut order);
    }
    order
}

fn topo_visit(deps: &[HashSet<usize>], id: usize, visited: &mut [bool], order: &mut Vec<usize>) {
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
