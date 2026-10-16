//! Reactive-UI desugaring (`Input.reactive_ui`) — two source transforms for the SDK's signals
//! feature, applied over the whole module graph once it is built. Both key on *free* references to
//! the SDK's injected globals (`unresolved_ctxt`), so SDK-internal code — which imports these names
//! instead of referencing them as globals — is naturally untouched.
//!
//! 1. **Memoized children maps.** `list.map(fn)` lexically inside a children-position closure of a
//!    UI container factory (`UIColumn(() => …)`, `UIRow(style, () => …)`, `x.setContent(() => …)`)
//!    is rewritten to `__uiMap(list, fn, "<path>:<offset>")` — the SDK's per-call-site memoized map,
//!    so unchanged items keep their element identity across binding re-runs and the runtime
//!    reconciler leaves them mounted. The slot string only needs to be unique within a bundle.
//!
//! 2. **Auto-wrapped reactive expressions.** A `UIText` text argument, or a top-level object
//!    property value in a binding-accepting call (factory style arg, `.style({...})`, or the
//!    style-class batch form `.class({...})`), that reads `.value` of a signal-typed binding is
//!    wrapped in `() => expr`, turning it into a reactive binding. The SDK APIs accept both forms,
//!    so the wrap is type- and behavior-transparent for non-reactive code. Signal-typed bindings
//!    are found by a fixpoint rooted at free calls to `signal(…)` / `computed(…)`, through the
//!    chain methods that return the signal (`signal(0).stored("key")`) and aliases (mirroring
//!    fusion's Vec3 local inference) — ACROSS MODULES: an imported binding whose origin (through
//!    any re-export chain) is a signal binding is a signal here too. A `store.ts` of signals read
//!    by the screens is the ordinary shape of an app; inferring per file left every such text
//!    unwrapped and silently stale.
//!
//! Both rewrites are safe to over-apply: `__uiMap` degrades to `Array.prototype.map` outside a
//! children binding / for non-element results, and a wrapped expression that never re-reads a
//! signal simply runs once.

use std::collections::HashSet;

use swc_core::atoms::{Atom, Wtf8Atom};
use swc_core::common::{Spanned, SyntaxContext, DUMMY_SP};
use swc_core::ecma::ast::*;
use swc_core::ecma::visit::{Visit, VisitMut, VisitMutWith, VisitWith};

use crate::graph::{resolve_export, ModuleGraph, ModuleInfo};
use crate::vocab::{has, Signals, Ui};

/// A signal-typed binding: its name and the scope it is bound in.
type Binding = (Atom, SyntaxContext);

/// Apply the pass to every module of the graph. The names it keys on — the container / text
/// factories, the children setter, the binding methods, the map helper, the signal factories and
/// their `.value` — are the SDK's vocabulary.
///
/// Signal inference runs graph-wide to a fixpoint: a module's imported bindings whose origin (a
/// real top-level binding, through re-exports) is a signal binding seed that module's own
/// inference, and what it exports feeds the modules importing from it. The rewrite then runs per
/// module with the bindings found.
pub fn apply_graph(graph: &mut ModuleGraph, ui: &Ui, signals: &Signals) {
    let n = graph.modules.len();
    let imported: Vec<Vec<ImportedLocal>> = graph.modules.iter().map(imported_locals).collect();
    let mut vars: Vec<HashSet<Binding>> = vec![HashSet::new(); n];
    loop {
        let mut changed = false;
        for i in 0..n {
            let m = &graph.modules[i];
            let mut seeds = vars[i].clone();
            for il in &imported[i] {
                let Some((origin, local)) = resolve_export(graph, il.target, &il.imported) else { continue };
                let origin_ctxt = graph.modules[origin].top_level_ctxt;
                if vars[origin].contains(&(Atom::from(local.as_str()), origin_ctxt)) {
                    seeds.insert((il.local.clone(), il.ctxt));
                }
            }
            let found = collect(&m.module, m.unresolved_ctxt, signals, seeds);
            if found != vars[i] {
                vars[i] = found;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    for (i, m) in graph.modules.iter_mut().enumerate() {
        let vars = std::mem::take(&mut vars[i]);
        m.module.visit_mut_with(&mut ReactiveUi { path: &m.path, unresolved: m.unresolved_ctxt, ui, signals, vars, in_children_fn: 0 });
    }
}

/// Fixpoint over one module: the bindings initialized from `signal(...)` / `computed(...)`, a
/// chain call on one, or an alias of one — starting from `seeds` (the imported signals).
fn collect(module: &Module, unresolved_ctxt: SyntaxContext, signals: &Signals, seeds: HashSet<Binding>) -> HashSet<Binding> {
    let mut vars = seeds;
    loop {
        let mut collect = SignalCollect { unresolved: unresolved_ctxt, signals, vars: &mut vars, changed: false };
        module.visit_with(&mut collect);
        if !collect.changed {
            break;
        }
    }
    vars
}

/// A value import of a module: the local binding (with the scope the resolver gave it, read off
/// the specifier itself) and where it comes from.
struct ImportedLocal {
    local: Atom,
    ctxt: SyntaxContext,
    target: usize,
    imported: String,
}

fn imported_locals(m: &ModuleInfo) -> Vec<ImportedLocal> {
    // local name → (target module, imported name), from the graph's resolved import edges
    let mut origin: std::collections::HashMap<&str, (usize, &str)> = std::collections::HashMap::new();
    for (tid, specs) in &m.imports {
        for s in specs {
            origin.insert(s.local.as_str(), (*tid, s.imported.as_str()));
        }
    }
    let mut out = Vec::new();
    for item in &m.module.body {
        let ModuleItem::ModuleDecl(ModuleDecl::Import(imp)) = item else { continue };
        for spec in &imp.specifiers {
            let local = match spec {
                ImportSpecifier::Named(s) => &s.local,
                ImportSpecifier::Default(d) => &d.local,
                ImportSpecifier::Namespace(_) => continue, // `ns.count.value`: a member read, not inferred
            };
            if let Some((target, imported)) = origin.get(local.sym.as_str()) {
                out.push(ImportedLocal { local: local.sym.clone(), ctxt: local.ctxt, target: *target, imported: (*imported).to_string() });
            }
        }
    }
    out
}

// ---- signal-type inference ----------------------------------------------------------------------

struct SignalCollect<'a> {
    unresolved: SyntaxContext,
    signals: &'a Signals,
    vars: &'a mut HashSet<(Atom, SyntaxContext)>,
    changed: bool,
}

impl SignalCollect<'_> {
    fn is_signal_expr(&self, e: &Expr) -> bool {
        match e {
            Expr::Paren(p) => self.is_signal_expr(&p.expr),
            Expr::Ident(id) => self.vars.contains(&(id.sym.clone(), id.ctxt)),
            Expr::Call(c) => match &c.callee {
                Callee::Expr(callee) => match &**callee {
                    Expr::Ident(id) => id.ctxt == self.unresolved && has(&self.signals.factories, &id.sym),
                    // `signal(0).stored("key")`: a chain method that returns the signal
                    Expr::Member(m) => match &m.prop {
                        MemberProp::Ident(p) => has(&self.signals.chain, &p.sym) && self.is_signal_expr(&m.obj),
                        _ => false,
                    },
                    _ => false,
                },
                _ => false,
            },
            _ => false,
        }
    }
}

impl Visit for SignalCollect<'_> {
    fn visit_var_declarator(&mut self, d: &VarDeclarator) {
        d.visit_children_with(self);
        if let Pat::Ident(b) = &d.name {
            if let Some(init) = &d.init {
                if self.is_signal_expr(init) && self.vars.insert((b.id.sym.clone(), b.id.ctxt)) {
                    self.changed = true;
                }
            }
        }
    }
}

/// Does `expr` read `<signal>.value` (outside any nested function)? Nested functions already defer
/// evaluation, so a read inside one must not trigger a wrap of the outer expression. `await`/`yield`
/// at the top level make the expression unwrappable (an arrow body can't host them).
struct ReadsSignalValue<'a> {
    vars: &'a HashSet<(Atom, SyntaxContext)>,
    /// The signal's read property (`value`).
    value: &'a str,
    found: bool,
    blocked: bool,
}

impl Visit for ReadsSignalValue<'_> {
    fn visit_function(&mut self, _: &Function) {}
    fn visit_arrow_expr(&mut self, _: &ArrowExpr) {}
    fn visit_await_expr(&mut self, a: &AwaitExpr) {
        self.blocked = true;
        a.visit_children_with(self);
    }
    fn visit_yield_expr(&mut self, y: &YieldExpr) {
        self.blocked = true;
        y.visit_children_with(self);
    }
    fn visit_member_expr(&mut self, m: &MemberExpr) {
        m.visit_children_with(self);
        if let MemberProp::Ident(p) = &m.prop {
            if p.sym == self.value {
                if let Expr::Ident(obj) = &*m.obj {
                    if self.vars.contains(&(obj.sym.clone(), obj.ctxt)) {
                        self.found = true;
                    }
                }
            }
        }
    }
}

// ---- the rewriting pass -------------------------------------------------------------------------

struct ReactiveUi<'a> {
    path: &'a str,
    unresolved: SyntaxContext,
    ui: &'a Ui,
    signals: &'a Signals,
    vars: HashSet<(Atom, SyntaxContext)>,
    /// Nesting depth of children-position closures (0 = not inside one).
    in_children_fn: usize,
}

impl ReactiveUi<'_> {
    /// The factory name if `call` is a free call to a UI factory this pass cares about.
    fn free_factory<'e>(&self, call: &'e CallExpr) -> Option<&'e str> {
        let Callee::Expr(callee) = &call.callee else { return None };
        let Expr::Ident(id) = &**callee else { return None };
        if id.ctxt != self.unresolved {
            return None;
        }
        let name = id.sym.as_str();
        (has(&self.ui.containers, name) || name == self.ui.text).then_some(name)
    }

    fn reads_signal(&self, e: &Expr) -> bool {
        let mut chk = ReadsSignalValue { vars: &self.vars, value: &self.signals.value, found: false, blocked: false };
        e.visit_with(&mut chk);
        chk.found && !chk.blocked
    }

    /// Wrap `expr` in `() => expr` when it reads a signal and isn't already deferred/static.
    fn wrap_if_reactive(&self, e: &mut Expr) {
        if matches!(e, Expr::Arrow(_) | Expr::Fn(_) | Expr::Object(_) | Expr::Lit(_) | Expr::Invalid(_)) {
            return;
        }
        if !self.reads_signal(e) {
            return;
        }
        let span = e.span();
        let taken = std::mem::replace(e, Expr::Invalid(Invalid { span: DUMMY_SP }));
        *e = Expr::Arrow(ArrowExpr {
            span,
            ctxt: SyntaxContext::empty(),
            params: vec![],
            is_async: false,
            is_generator: false,
            type_params: None,
            return_type: None,
            body: Box::new(BlockStmtOrExpr::Expr(Box::new(taken))),
        });
    }

    /// Wrap reactive top-level property values of a style object literal. Nested objects
    /// (`$pressed`, `$landscape`, any `$class` block) are left alone — the runtime only binds top-level values.
    fn wrap_style_object(&self, obj: &mut ObjectLit) {
        for prop in &mut obj.props {
            if let PropOrSpread::Prop(p) = prop {
                if let Prop::KeyValue(kv) = &mut **p {
                    self.wrap_if_reactive(&mut kv.value);
                }
            }
        }
    }

    /// Rewrite `list.map(fn)` → `__uiMap(list, fn, "<path>:<offset>")` (single-argument `.map`
    /// member calls only; anything else keeps plain-map semantics).
    fn maybe_rewrite_map(&self, e: &mut Expr) {
        let Expr::Call(call) = e else { return };
        if call.args.len() != 1 || call.args[0].spread.is_some() {
            return;
        }
        let Callee::Expr(callee) = &call.callee else { return };
        let Expr::Member(m) = &**callee else { return };
        let MemberProp::Ident(p) = &m.prop else { return };
        if p.sym != "map" {
            return;
        }
        let slot = format!("{}:{}", self.path, call.span.lo.0);
        let span = call.span;

        let Expr::Call(call) = std::mem::replace(e, Expr::Invalid(Invalid { span: DUMMY_SP })) else { unreachable!() };
        let Callee::Expr(callee) = call.callee else { unreachable!() };
        let Expr::Member(m) = *callee else { unreachable!() };
        let render = call.args.into_iter().next().unwrap();

        *e = Expr::Call(CallExpr {
            span,
            ctxt: SyntaxContext::empty(),
            callee: Callee::Expr(Box::new(Expr::Ident(Ident::new(Atom::from(self.ui.map_helper.as_str()), DUMMY_SP, self.unresolved)))),
            args: vec![
                ExprOrSpread { spread: None, expr: m.obj },
                render,
                ExprOrSpread {
                    spread: None,
                    expr: Box::new(Expr::Lit(Lit::Str(Str { span: DUMMY_SP, value: Wtf8Atom::from(slot.as_str()), raw: None }))),
                },
            ],
            type_args: None,
        });
    }
}

impl VisitMut for ReactiveUi<'_> {
    fn visit_mut_expr(&mut self, e: &mut Expr) {
        if let Expr::Call(call) = e {
            // A UI factory call: walk arguments with position awareness.
            if let Some(name) = self.free_factory(call) {
                let is_text = name == self.ui.text;
                let argc = call.args.len();
                for (i, arg) in call.args.iter_mut().enumerate() {
                    if arg.spread.is_some() {
                        arg.expr.visit_mut_with(self);
                        continue;
                    }
                    let expr = &mut *arg.expr;
                    if !is_text && matches!(expr, Expr::Arrow(_) | Expr::Fn(_)) {
                        // children-position closure → reactive children binding
                        self.in_children_fn += 1;
                        expr.visit_mut_with(self);
                        self.in_children_fn -= 1;
                        continue;
                    }
                    expr.visit_mut_with(self);
                    if is_text && i + 1 == argc && i <= 1 {
                        self.wrap_if_reactive(expr); // UIText text argument (last arg)
                    } else if i == 0 {
                        if let Expr::Object(obj) = expr {
                            self.wrap_style_object(obj); // factory style argument
                        }
                    }
                }
                return;
            }

            // Member calls the pass understands: x.setContent(fn), x.style({...}) and x.class({...}).
            if let Callee::Expr(callee) = &call.callee {
                if let Expr::Member(m) = &**callee {
                    if let MemberProp::Ident(p) = &m.prop {
                        if p.sym == self.ui.set_content.as_str()
                            && call.args.len() == 1
                            && call.args[0].spread.is_none()
                            && matches!(&*call.args[0].expr, Expr::Arrow(_) | Expr::Fn(_))
                        {
                            let Callee::Expr(callee) = &mut call.callee else { unreachable!() };
                            callee.visit_mut_with(self);
                            self.in_children_fn += 1;
                            call.args[0].expr.visit_mut_with(self);
                            self.in_children_fn -= 1;
                            return;
                        }
                        // `.class({...})` values are boolean | () => boolean — the same wrap rule
                        // as style values applies (the el.class proxy accepts both forms).
                        if has(&self.ui.binding_methods, &p.sym) && call.args.len() == 1 && call.args[0].spread.is_none() {
                            call.visit_mut_children_with(self);
                            if let Expr::Object(obj) = &mut *call.args[0].expr {
                                self.wrap_style_object(obj);
                            }
                            return;
                        }
                    }
                }
            }

            // Inside a children closure: memoize `.map` calls, then descend (nested maps inside the
            // render function get their own slots).
            if self.in_children_fn > 0 {
                self.maybe_rewrite_map(e);
                e.visit_mut_children_with(self);
                return;
            }
        }
        e.visit_mut_children_with(self);
    }
}
