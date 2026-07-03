//! Reactive-UI desugaring (`Input.reactive_ui`) — two source transforms for the SDK's signals
//! feature, applied per module right after parse. Both key on *free* references to the SDK's
//! injected globals (`unresolved_ctxt`), so SDK-internal code — which imports these names instead
//! of referencing them as globals — is naturally untouched.
//!
//! 1. **Memoized children maps.** `list.map(fn)` lexically inside a children-position closure of a
//!    UI container factory (`UIColumn(() => …)`, `UIRow(style, () => …)`, `x.setContent(() => …)`)
//!    is rewritten to `__uiMap(list, fn, "<path>:<offset>")` — the SDK's per-call-site memoized map,
//!    so unchanged items keep their element identity across binding re-runs and the runtime
//!    reconciler leaves them mounted. The slot string only needs to be unique within a bundle.
//!
//! 2. **Auto-wrapped reactive expressions.** A `UIText` text argument, or a top-level style-object
//!    property value (factory style arg or `.style({...})` call), that reads `.value` of a
//!    signal-typed binding is wrapped in `() => expr`, turning it into a reactive binding. The SDK
//!    APIs accept both forms, so the wrap is type- and behavior-transparent for non-reactive code.
//!    Signal-typed bindings are found by a fixpoint rooted at free calls to `signal(…)` /
//!    `computed(…)` (mirroring fusion's Vec3 local inference).
//!
//! Both rewrites are safe to over-apply: `__uiMap` degrades to `Array.prototype.map` outside a
//! children binding / for non-element results, and a wrapped expression that never re-reads a
//! signal simply runs once.

use std::collections::HashSet;

use swc_core::atoms::{Atom, Wtf8Atom};
use swc_core::common::{Spanned, SyntaxContext, DUMMY_SP};
use swc_core::ecma::ast::*;
use swc_core::ecma::visit::{Visit, VisitMut, VisitMutWith, VisitWith};

/// Container factories whose function-valued children argument is a reactive children binding.
const CONTAINER_FACTORIES: &[&str] = &[
    "UIColumn", "UIRow", "UIBox", "UIButton", "UIScrollable", "UIScreen", "UIWidget",
];
/// Text factory: the (last) text argument is a binding position.
const TEXT_FACTORY: &str = "UIText";
/// Free calls that root the signal-type inference.
const SIGNAL_FACTORIES: &[&str] = &["signal", "computed"];
/// The SDK's memoized-map helper (an inject.ts export, referenced as a free global).
const MAP_HELPER: &str = "__uiMap";

/// Apply the pass to one module. `unresolved_ctxt` is the module's free-reference context.
pub fn apply(path: &str, unresolved_ctxt: SyntaxContext, module: &mut Module) {
    // Fixpoint: collect bindings initialized from `signal(...)` / `computed(...)` or aliases.
    let mut vars: HashSet<(Atom, SyntaxContext)> = HashSet::new();
    loop {
        let mut collect = SignalCollect { unresolved: unresolved_ctxt, vars: &mut vars, changed: false };
        module.visit_with(&mut collect);
        if !collect.changed {
            break;
        }
    }
    module.visit_mut_with(&mut ReactiveUi { path, unresolved: unresolved_ctxt, vars, in_children_fn: 0 });
}

// ---- signal-type inference ----------------------------------------------------------------------

struct SignalCollect<'a> {
    unresolved: SyntaxContext,
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
                    Expr::Ident(id) => id.ctxt == self.unresolved && SIGNAL_FACTORIES.contains(&id.sym.as_str()),
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
            if p.sym == "value" {
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
        (CONTAINER_FACTORIES.contains(&name) || name == TEXT_FACTORY).then_some(name)
    }

    fn reads_signal(&self, e: &Expr) -> bool {
        let mut chk = ReadsSignalValue { vars: &self.vars, found: false, blocked: false };
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
    /// (`onPressed`, `$class` blocks) are left alone — the runtime only binds top-level values.
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
            callee: Callee::Expr(Box::new(Expr::Ident(Ident::new(Atom::from(MAP_HELPER), DUMMY_SP, self.unresolved)))),
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
                let is_text = name == TEXT_FACTORY;
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

            // Member calls the pass understands: x.setContent(fn) and x.style({...}).
            if let Callee::Expr(callee) = &call.callee {
                if let Expr::Member(m) = &**callee {
                    if let MemberProp::Ident(p) = &m.prop {
                        if p.sym == "setContent"
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
                        if p.sym == "style" && call.args.len() == 1 && call.args[0].spread.is_none() {
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
