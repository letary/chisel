//! Component writes on SDK-owned vectors.
//!
//! `hero.controller.velocity.y = 7` is, as written, a write to a throwaway copy: the `velocity`
//! getter hands out a fresh vector. The natural spelling should reach the engine, so this pass
//! rewrites the exact shape `<owner>.<prop>.<axis> = v` (axis ∈ x/y/z/w) into a call the OWNER
//! answers — no getter, no allocation:
//!
//!   hero.controller.velocity.y = 7     →  __compWrite(hero.controller, "velocity", "y", 7)
//!   hero.controller.velocity.y += 3    →  __compOp(hero.controller, "velocity", "y", 0, 3)
//!   hero.controller.velocity.y++       →  __compOp(hero.controller, "velocity", "y", 0, 1)   (statement position only)
//!
//! Which `prop`s qualify is declared by the owner class itself, at compile time:
//!
//!   class CharacterController {
//!     static _comps = ["velocity"]                       // read here, then STRIPPED from the bundle
//!     _writeComp(prop, axis, v) { … }                    // the runtime hook the helpers call
//!   }
//!
//! Safety without types: the bundler cannot know `hero.controller` IS a controller, so the helpers
//! (SDK code) check for the hook at runtime — `o._writeComp ? o._writeComp(prop, axis, v) :
//! o[prop][axis] = v` — and a plain record `{ velocity: { x, y } }` behaves exactly as written.
//! Only listed names are rewritten, so ordinary `p.pos.x = …` on user records never pays for the
//! guard, and only the direct spelling is covered; `const v = c.velocity; v.y = 7` stays a copy, by
//! design. A class with the hook but no list is a build diagnostic (nothing is routed to it). The
//! pass is a no-op when the SDK does not export the helpers (older SDK), and the linker marks every
//! rewritten `prop` live — after the rewrite nothing references `.velocity` by name, but the
//! helper's fallback path does.

use std::collections::HashSet;

use swc_core::atoms::{Atom, Wtf8Atom};
use swc_core::common::{Span, SyntaxContext, DUMMY_SP};
use swc_core::ecma::ast::*;
use swc_core::ecma::visit::{VisitMut, VisitMutWith};

use crate::graph::ModuleGraph;

pub const WRITE_HELPER: &str = "__compWrite";
pub const OP_HELPER: &str = "__compOp";
const OWNER_HOOK: &str = "_writeComp";
const OWNER_LIST: &str = "_comps";
const AXES: &[&str] = &["x", "y", "z", "w"];

/// Harvest every owner's `static _comps` (stripping it), then rewrite every module. Returns the
/// property names that were rewritten (for the linker to keep).
pub fn apply(graph: &mut ModuleGraph, diagnostics: &mut Vec<String>) -> HashSet<Atom> {
    let props = owner_props(graph, diagnostics);
    let mut used = HashSet::new();
    if props.is_empty() {
        return used;
    }
    for m in graph.modules.iter_mut() {
        let mut rw = Rewriter { unresolved: m.unresolved_ctxt, props: &props, used: &mut used };
        m.module.visit_mut_with(&mut rw);
    }
    used
}

/// The union of `static _comps = ["…"]` over every class that declares an instance `_writeComp`.
/// The static is compile-time data: it is removed from the class once read.
fn owner_props(graph: &mut ModuleGraph, diagnostics: &mut Vec<String>) -> HashSet<Atom> {
    let mut out = HashSet::new();
    for m in graph.modules.iter_mut() {
        let path = m.path.clone();
        for item in m.module.body.iter_mut() {
            let cd = match item {
                ModuleItem::Stmt(Stmt::Decl(Decl::Class(c))) => c,
                ModuleItem::ModuleDecl(ModuleDecl::ExportDecl(ExportDecl { decl: Decl::Class(c), .. })) => c,
                _ => continue,
            };
            let has_hook = cd.class.body.iter().any(|mem| {
                matches!(mem, ClassMember::Method(mm) if !mm.is_static && mm.kind == MethodKind::Method && key_is(&mm.key, OWNER_HOOK))
            });
            if !has_hook {
                continue;
            }
            match take_comps(&mut cd.class) {
                Some(list) => out.extend(list),
                None => diagnostics.push(format!(
                    "{path}: class {} declares `{OWNER_HOOK}` but no `static {OWNER_LIST} = [\"prop\", …]` (string literals) — no component writes are routed to it",
                    cd.ident.sym
                )),
            }
        }
    }
    out
}

/// Remove `static _comps = ["a", "b"]` from the class body and return its names, if well-formed.
fn take_comps(class: &mut Class) -> Option<Vec<Atom>> {
    let idx = class.body.iter().position(|mem| matches!(mem, ClassMember::ClassProp(p) if p.is_static && key_is(&p.key, OWNER_LIST)))?;
    let ClassMember::ClassProp(p) = class.body.remove(idx) else { unreachable!() };
    let Some(value) = p.value else { return None };
    let Expr::Array(arr) = *value else { return None };
    let mut names = Vec::with_capacity(arr.elems.len());
    for el in arr.elems {
        let Some(ExprOrSpread { spread: None, expr }) = el else { return None };
        let Expr::Lit(Lit::Str(s)) = *expr else { return None };
        names.push(Atom::from(s.value.as_str()?));
    }
    Some(names)
}

fn key_is(k: &PropName, name: &str) -> bool {
    matches!(k, PropName::Ident(i) if i.sym == name)
}

struct Rewriter<'a> {
    unresolved: SyntaxContext,
    props: &'a HashSet<Atom>,
    used: &'a mut HashSet<Atom>,
}

/// `<owner>.<prop>.<axis>` with `prop` a listed name and `axis` a component → `(prop, axis)`.
fn split_target(m: &MemberExpr, props: &HashSet<Atom>) -> Option<(Atom, Atom)> {
    let MemberProp::Ident(axis) = &m.prop else { return None };
    if !AXES.contains(&axis.sym.as_str()) {
        return None;
    }
    let Expr::Member(inner) = &*m.obj else { return None };
    let MemberProp::Ident(prop) = &inner.prop else { return None };
    if !props.contains(&prop.sym) {
        return None;
    }
    Some((prop.sym.clone(), axis.sym.clone()))
}

/// `+=` → 0, `-=` → 1, `*=` → 2, `/=` → 3 (the helper's op codes). Anything else is left as written.
fn op_code(op: AssignOp) -> Option<f64> {
    match op {
        AssignOp::AddAssign => Some(0.0),
        AssignOp::SubAssign => Some(1.0),
        AssignOp::MulAssign => Some(2.0),
        AssignOp::DivAssign => Some(3.0),
        _ => None,
    }
}

fn arg(expr: Box<Expr>) -> ExprOrSpread {
    ExprOrSpread { spread: None, expr }
}

fn str_arg(s: &Atom) -> ExprOrSpread {
    arg(Box::new(Expr::Lit(Lit::Str(Str { span: DUMMY_SP, value: Wtf8Atom::from(s.as_str()), raw: None }))))
}

fn num_arg(v: f64) -> ExprOrSpread {
    arg(Box::new(Expr::Lit(Lit::Num(Number { span: DUMMY_SP, value: v, raw: None }))))
}

impl Rewriter<'_> {
    fn helper_call(&mut self, helper: &str, span: Span, owner: Box<Expr>, prop: Atom, axis: Atom, extra: Vec<ExprOrSpread>) -> Expr {
        self.used.insert(prop.clone());
        let mut args = vec![arg(owner), str_arg(&prop), str_arg(&axis)];
        args.extend(extra);
        Expr::Call(CallExpr {
            span,
            ctxt: SyntaxContext::empty(),
            callee: Callee::Expr(Box::new(Expr::Ident(Ident::new(Atom::from(helper), DUMMY_SP, self.unresolved)))),
            args,
            type_args: None,
        })
    }

    /// `<owner>.<prop>.<axis> (=|+=|-=|*=|/=) rhs` → helper call. Any other shape is left alone.
    fn rewrite_assign(&mut self, e: &mut Expr) {
        let Expr::Assign(a) = e else { return };
        let AssignTarget::Simple(SimpleAssignTarget::Member(m)) = &a.left else { return };
        let Some((prop, axis)) = split_target(m, self.props) else { return };
        let code = match a.op {
            AssignOp::Assign => None,
            other => match op_code(other) {
                Some(c) => Some(c),
                None => return,
            },
        };
        let span = a.span;
        let Expr::Assign(a) = std::mem::replace(e, Expr::Invalid(Invalid { span: DUMMY_SP })) else { unreachable!() };
        let AssignTarget::Simple(SimpleAssignTarget::Member(m)) = a.left else { unreachable!() };
        let Expr::Member(inner) = *m.obj else { unreachable!() };
        *e = match code {
            None => self.helper_call(WRITE_HELPER, span, inner.obj, prop, axis, vec![arg(a.right)]),
            Some(c) => self.helper_call(OP_HELPER, span, inner.obj, prop, axis, vec![num_arg(c), arg(a.right)]),
        };
    }

    /// Statement-position `<owner>.<prop>.<axis>++` / `--` → `__compOp(…, 0|1, 1)`. Only there: as a
    /// value, postfix `++` yields the OLD component while the helper returns the new one.
    fn rewrite_update_stmt(&mut self, s: &mut Stmt) {
        let Stmt::Expr(es) = s else { return };
        let Expr::Update(u) = &*es.expr else { return };
        let Expr::Member(m) = &*u.arg else { return };
        let Some((prop, axis)) = split_target(m, self.props) else { return };
        let code = if u.op == UpdateOp::PlusPlus { 0.0 } else { 1.0 };
        let span = u.span;
        let Expr::Update(u) = std::mem::replace(&mut *es.expr, Expr::Invalid(Invalid { span: DUMMY_SP })) else { unreachable!() };
        let Expr::Member(m) = *u.arg else { unreachable!() };
        let Expr::Member(inner) = *m.obj else { unreachable!() };
        *es.expr = self.helper_call(OP_HELPER, span, inner.obj, prop, axis, vec![num_arg(code), num_arg(1.0)]);
    }
}

impl VisitMut for Rewriter<'_> {
    fn visit_mut_expr(&mut self, e: &mut Expr) {
        e.visit_mut_children_with(self); // inner writes first (the RHS may contain one)
        self.rewrite_assign(e);
    }

    fn visit_mut_stmt(&mut self, s: &mut Stmt) {
        s.visit_mut_children_with(self);
        self.rewrite_update_stmt(s);
    }
}
