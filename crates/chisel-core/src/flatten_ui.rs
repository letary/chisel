//! UI factory call optimization (`Input.flatten_ui`) — two rewrites over free UI factory calls,
//! both keyed on `unresolved_ctxt` like `reactive_ui` (so a user-defined `UIColumn`, or
//! SDK-internal code which imports the name, is never touched):
//!
//! 1. **Array splicing.** Literal array arguments become variadic arguments:
//!    `UIColumn([a, b])` → `UIColumn(a, b)`, `UIColumn(x, [y, z])` → `UIColumn(x, y, z)`. The
//!    SDK's runtime dispatch (`buildUI` in UINode.ts) flattens array arguments one level either
//!    way, so the rewrite is behavior-preserving while saving one array allocation per call site.
//!    A literal is left alone whenever splicing could change what the dispatcher sees: spreads,
//!    holes, nested array literals, function elements (a lone function argument means reactive
//!    children), a leading object literal in first position (the legacy style bag), and an empty
//!    first array followed by more arguments.
//!
//! 2. **Raw lowering.** When *every* argument is provably a plain child — a call to a known
//!    node-returning factory (possibly through a builder-method chain like `.style()`/`.onClick()`),
//!    a `const` initialized from one (fixpoint, mirroring reactive_ui's signal inference), a falsy
//!    literal, or `&&`/`?:`/parens over those — the call skips runtime dispatch entirely:
//!    `UIColumn(a, b)` → `__UIColumn([a, b])`, the SDK's dispatch-free construction path (an
//!    inject export, like `__uiMap`). The array literal it introduces IS the children storage the
//!    element keeps, so allocation count is unchanged — only the per-call dispatch (style probe,
//!    per-argument `Array.isArray` scan) disappears. The reactive form lowers too: a lone literal
//!    closure (`UIColumn(() => […])`) becomes `__UIColumn(() => […])` — the Element constructor
//!    branches on `typeof children === "function"` either way. Anything not provable — unknown
//!    identifiers, a style object, a function behind an identifier — falls back to the
//!    still-correct dispatch call.
//!
//! Requires an SDK with variadic factory dispatch + `__UI*` raw builders (shipped in lockstep by
//! the CLI); the flag stays off by default.

use std::collections::HashSet;

use swc_core::atoms::Atom;
use swc_core::common::{SyntaxContext, DUMMY_SP};
use swc_core::ecma::ast::*;
use swc_core::ecma::visit::{Visit, VisitMut, VisitMutWith, VisitWith};

type Key = (Atom, SyntaxContext);

/// Every factory whose runtime goes through the SDK's variadic `buildUI` dispatch (plus `UIPager`,
/// which flattens tab arguments the same way). All are splice targets.
const FLATTEN_FACTORIES: &[&str] = &[
    "UIColumn", "UIRow", "UIBox", "UIButton", "UIScrollable", "UIScreen", "UIWidget",
    "UIModal", "UIPopover", "UIBottomSheet", "UIPager",
];

/// Factories with a dispatch-free `__UI*` raw builder in the SDK (raw-lowering targets).
const RAW_FACTORIES: &[&str] = &[
    "UIColumn", "UIRow", "UIBox", "UIButton", "UIScrollable", "UIScreen", "UIWidget",
];

/// Free calls that provably return a UI node (argument provability). Includes the already-lowered
/// `__UI*` names so inner calls rewritten first keep proving the outer call.
const NODE_FACTORIES: &[&str] = &[
    "UIColumn", "UIRow", "UIBox", "UIButton", "UIScrollable", "UIScreen", "UIWidget",
    "UIText", "UIImage", "UIVideo", "UIInput", "UITextArea", "UISpacer",
    "UIModal", "UIPopover", "UIBottomSheet", "UIPager",
    "__UIColumn", "__UIRow", "__UIBox", "__UIButton", "__UIScrollable", "__UIScreen", "__UIWidget",
];

/// Builder methods that return the node (`this`) — a chain rooted at a node factory stays a node.
/// Conservative whitelist: an unlisted method (e.g. `getBoundingClientRect`) blocks provability.
const CHAIN_METHODS: &[&str] = &[
    "style", "animateTo", "animateFrom", "class",
    "onClick", "onTouchStart", "onLongPress", "onLayout", "onOpen", "onClose", "onBackPressed",
    "onScroll", "onScrollRelease", "onOverscroll", "onRefresh", "onInput", "onSubmit",
    "onSelect", "onChange", "onOverlayTap",
    "append", "insert", "remove", "setContent", "keepAlive",
];

/// Apply the pass to one module; returns the number of rewritten call sites.
pub fn apply(unresolved_ctxt: SyntaxContext, module: &mut Module) -> usize {
    // Fixpoint: `const` bindings initialized from a provable node expression are node-typed.
    let mut vars: HashSet<Key> = HashSet::new();
    loop {
        let mut collect = NodeCollect { unresolved: unresolved_ctxt, vars: &mut vars, changed: false };
        module.visit_with(&mut collect);
        if !collect.changed {
            break;
        }
    }
    let mut v = FlattenUi { unresolved: unresolved_ctxt, vars, count: 0 };
    module.visit_mut_with(&mut v);
    v.count
}

/// Is `e` provably a plain child value (a node, or falsy for conditionals)?
fn is_node_expr(unresolved: SyntaxContext, vars: &HashSet<Key>, e: &Expr) -> bool {
    match e {
        Expr::Paren(p) => is_node_expr(unresolved, vars, &p.expr),
        Expr::Lit(Lit::Null(_)) => true,
        Expr::Lit(Lit::Bool(b)) => !b.value,
        Expr::Ident(id) => {
            (id.sym == "undefined" && id.ctxt == unresolved)
                || vars.contains(&(id.sym.clone(), id.ctxt))
        }
        Expr::Call(c) => match &c.callee {
            Callee::Expr(callee) => match &**callee {
                Expr::Ident(id) => id.ctxt == unresolved && NODE_FACTORIES.contains(&id.sym.as_str()),
                Expr::Member(m) => match &m.prop {
                    MemberProp::Ident(p) if CHAIN_METHODS.contains(&p.sym.as_str()) => {
                        is_node_expr(unresolved, vars, &m.obj)
                    }
                    _ => false,
                },
                _ => false,
            },
            _ => false,
        },
        Expr::Bin(b) if b.op == BinaryOp::LogicalAnd => is_node_expr(unresolved, vars, &b.right),
        Expr::Cond(c) => {
            is_node_expr(unresolved, vars, &c.cons) && is_node_expr(unresolved, vars, &c.alt)
        }
        _ => false,
    }
}

// ---- node-type inference ------------------------------------------------------------------------

struct NodeCollect<'a> {
    unresolved: SyntaxContext,
    vars: &'a mut HashSet<Key>,
    changed: bool,
}

impl Visit for NodeCollect<'_> {
    fn visit_var_decl(&mut self, d: &VarDecl) {
        d.visit_children_with(self);
        if d.kind != VarDeclKind::Const {
            return; // `let`/`var` can be reassigned to anything — never proven
        }
        for decl in &d.decls {
            let Pat::Ident(b) = &decl.name else { continue };
            let Some(init) = &decl.init else { continue };
            if is_node_expr(self.unresolved, self.vars, init)
                && self.vars.insert((b.id.sym.clone(), b.id.ctxt))
            {
                self.changed = true;
            }
        }
    }
}

// ---- the rewriting pass -------------------------------------------------------------------------

struct FlattenUi {
    unresolved: SyntaxContext,
    vars: HashSet<Key>,
    count: usize,
}

impl FlattenUi {
    /// The factory name if `call` is a free call to a factory this pass rewrites.
    fn factory_name<'e>(&self, call: &'e CallExpr) -> Option<&'e str> {
        let Callee::Expr(callee) = &call.callee else { return None };
        let Expr::Ident(id) = &**callee else { return None };
        if id.ctxt != self.unresolved {
            return None;
        }
        let name = id.sym.as_str();
        FLATTEN_FACTORIES.contains(&name).then_some(name)
    }

    /// May the array literal at argument position `i` (of `argc` args) be spliced in place?
    fn can_splice(i: usize, argc: usize, arr: &ArrayLit) -> bool {
        if i == 0 && arr.elems.is_empty() && argc > 1 {
            return false; // would shift the next argument into style position
        }
        for (el_idx, el) in arr.elems.iter().enumerate() {
            let Some(el) = el else { return false }; // hole
            if el.spread.is_some() {
                return false;
            }
            match &*el.expr {
                Expr::Array(_) | Expr::Arrow(_) | Expr::Fn(_) => return false,
                Expr::Object(_) if i == 0 && el_idx == 0 => return false,
                _ => {}
            }
        }
        true
    }
}

impl VisitMut for FlattenUi {
    fn visit_mut_call_expr(&mut self, call: &mut CallExpr) {
        call.visit_mut_children_with(self); // factory calls nested in the arguments rewrite first
        // Owned facts up front — the &str name must not outlive the arg mutations below.
        let raw: Option<String> = {
            let Some(name) = self.factory_name(call) else { return };
            RAW_FACTORIES.contains(&name).then(|| format!("__{name}"))
        };

        // Stage 1: splice literal array arguments.
        let argc = call.args.len();
        let splice_at = |i: usize, a: &ExprOrSpread| {
            a.spread.is_none() && matches!(&*a.expr, Expr::Array(arr) if Self::can_splice(i, argc, arr))
        };
        if call.args.iter().enumerate().any(|(i, a)| splice_at(i, a)) {
            let old = std::mem::take(&mut call.args);
            let mut out: Vec<ExprOrSpread> = Vec::with_capacity(old.len());
            for (i, a) in old.into_iter().enumerate() {
                if splice_at(i, &a) {
                    let Expr::Array(arr) = *a.expr else { unreachable!() };
                    for el in arr.elems.into_iter().flatten() {
                        out.push(el);
                    }
                    self.count += 1;
                } else {
                    out.push(a);
                }
            }
            call.args = out;
        }

        // Stage 2: raw lowering — skip runtime dispatch.
        let Some(raw) = raw else { return };

        // Reactive form: a lone literal closure is the ChildrenFn — same raw builder, the Element
        // constructor branches on `typeof children === "function"` (a function behind an
        // identifier stays on dispatch, mirroring reactive_ui's literal-closure detection).
        if call.args.len() == 1
            && call.args[0].spread.is_none()
            && matches!(&*call.args[0].expr, Expr::Arrow(_) | Expr::Fn(_))
        {
            call.callee = Callee::Expr(Box::new(Expr::Ident(Ident::new(Atom::from(raw.as_str()), DUMMY_SP, self.unresolved))));
            self.count += 1;
            return;
        }

        // Static form: every argument provably a plain child → `__UIX([args])`.
        if !call.args.iter().all(|a| a.spread.is_none() && is_node_expr(self.unresolved, &self.vars, &a.expr)) {
            return;
        }
        let elems: Vec<Option<ExprOrSpread>> = std::mem::take(&mut call.args).into_iter().map(Some).collect();
        call.callee = Callee::Expr(Box::new(Expr::Ident(Ident::new(Atom::from(raw.as_str()), DUMMY_SP, self.unresolved))));
        call.args = vec![ExprOrSpread {
            spread: None,
            expr: Box::new(Expr::Array(ArrayLit { span: DUMMY_SP, elems })),
        }];
        self.count += 1;
    }
}
