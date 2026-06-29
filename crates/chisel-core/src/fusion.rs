//! Math chain fusion (scalar replacement).
//!
//! A `Vec3` value is "fused" into three scalar component expressions, so intermediate vectors in a
//! chain are never allocated — only the final escaping value (or nothing, for a scalar terminal like
//! `.dot()`) materializes. The lowering mirrors each method body operation-for-operation, so the
//! result is **bit-identical** to the un-fused code (JS numbers are all f64; we never reassociate).
//!
//! When a value is reused (a scalar reused 3×, `cross` components, the length in `normalize`) it is
//! hoisted into a `const` temp before the using statement, so nothing is recomputed and there is no
//! double-evaluation. Control-flow bodies are block-ified and arrow expression-bodies converted to
//! blocks first, so a hoisted temp always lands in the correct scope.
//!
//! Roots: `new Vec3(...)`, array literals, and **local variables proven to hold a Vec3** (so
//! per-frame code like `dir.scale(speed*dt)` fuses). Fusion runs before DCE, so methods that are only
//! ever fused away become unreferenced and the linker drops them.

use std::collections::HashSet;

use swc_core::atoms::Atom;
use swc_core::common::util::take::Take;
use swc_core::common::{SyntaxContext, DUMMY_SP};
use swc_core::ecma::ast::*;
use swc_core::ecma::visit::{Visit, VisitMut, VisitMutWith, VisitWith};

type Comps = [Expr; 3];
type Key = (Atom, SyntaxContext);

const VEC3_RETURNING: &[&str] = &[
    "add", "sub", "mul", "div", "scale", "negate", "scaleAndAdd", "cross", "lerp", "clamp", "min",
    "max", "reflect", "project", "rotateX", "rotateY", "rotateZ", "rotate", "transform", "withX",
    "withY", "withZ", "clone", "normalize", "copy", "set",
];
const VEC3_STATICS: &[&str] = &["up", "down", "left", "right", "forward", "back", "zero", "one", "from"];

pub struct Fuser {
    vec3: Key,
    vec_vars: HashSet<Key>,
    /// Temps to hoist before the statement currently being processed.
    temps: Vec<Stmt>,
    counter: usize,
    pub fused: usize,
}

// ---- statement-level temp splicing -------------------------------------------------------------

impl VisitMut for Fuser {
    fn visit_mut_module_items(&mut self, items: &mut Vec<ModuleItem>) {
        let saved = std::mem::take(&mut self.temps);
        let mut out = Vec::with_capacity(items.len());
        for mut it in items.drain(..) {
            it.visit_mut_with(self);
            for t in self.temps.drain(..) {
                out.push(ModuleItem::Stmt(t));
            }
            out.push(it);
        }
        *items = out;
        self.temps = saved;
    }

    fn visit_mut_stmts(&mut self, stmts: &mut Vec<Stmt>) {
        let saved = std::mem::take(&mut self.temps);
        let mut out = Vec::with_capacity(stmts.len());
        for mut s in stmts.drain(..) {
            s.visit_mut_with(self);
            out.append(&mut self.temps);
            out.push(s);
        }
        *stmts = out;
        self.temps = saved;
    }

    fn visit_mut_arrow_expr(&mut self, a: &mut ArrowExpr) {
        // Expression-bodied arrows have no statement list; if fusing the body needs a temp, convert
        // it to a block `{ <temps>; return <expr> }`.
        let saved = std::mem::take(&mut self.temps);
        let mut body = std::mem::replace(&mut a.body, Box::new(BlockStmtOrExpr::Expr(Box::new(Expr::Invalid(Invalid { span: DUMMY_SP })))));
        match &mut *body {
            BlockStmtOrExpr::BlockStmt(b) => b.visit_mut_with(self),
            BlockStmtOrExpr::Expr(e) => {
                e.visit_mut_with(self);
                if !self.temps.is_empty() {
                    let mut stmts: Vec<Stmt> = self.temps.drain(..).collect();
                    stmts.push(Stmt::Return(ReturnStmt { span: DUMMY_SP, arg: Some(e.take()) }));
                    body = Box::new(BlockStmtOrExpr::BlockStmt(BlockStmt { span: DUMMY_SP, ctxt: SyntaxContext::empty(), stmts }));
                }
            }
        }
        a.body = body;
        self.temps = saved;
    }

    fn visit_mut_expr(&mut self, e: &mut Expr) {
        // Transactional: a failed attempt rolls back any speculatively-hoisted temps.
        let mark = self.temps.len();
        if let Some(s) = self.scalar_terminal(e) {
            self.fused += 1;
            *e = paren(s);
            return;
        }
        self.temps.truncate(mark);
        if matches!(e, Expr::Call(_)) {
            if let Some(c) = self.fuse_vec(e) {
                self.fused += 1;
                *e = new_vec3(c, &self.vec3);
                return;
            }
            self.temps.truncate(mark);
        }
        e.visit_mut_children_with(self);
    }
}

// ---- fusion core -------------------------------------------------------------------------------

impl Fuser {
    /// Hoist `e` into a fresh `const` temp and return a reference to it (so a reused value is
    /// evaluated exactly once). Simple, side-effect-free expressions are returned as-is.
    fn simplify(&mut self, e: Expr) -> Expr {
        if is_simple(&e) {
            return e;
        }
        let id = Ident::new(Atom::from(format!("__chisel_{}", self.counter).as_str()), DUMMY_SP, SyntaxContext::empty());
        self.counter += 1;
        self.temps.push(const_decl(&id, e));
        Expr::Ident(id)
    }

    fn fuse_vec(&mut self, e: &Expr) -> Option<Comps> {
        match e {
            Expr::New(NewExpr { callee, args, .. }) if self.is_vec3_ident(callee) => {
                let args = args.as_deref().unwrap_or(&[]);
                match args.len() {
                    0 => Some([num(0.0), num(0.0), num(0.0)]),
                    3 if args.iter().all(|a| a.spread.is_none()) => {
                        Some([(*args[0].expr).clone(), (*args[1].expr).clone(), (*args[2].expr).clone()])
                    }
                    1 if args[0].spread.is_none() => self.fuse_vec(&args[0].expr),
                    _ => None,
                }
            }
            Expr::Array(ArrayLit { elems, .. }) if elems.len() == 3 && elems.iter().all(|x| x.as_ref().map_or(false, |s| s.spread.is_none())) => {
                let g = |i: usize| (*elems[i].as_ref().unwrap().expr).clone();
                Some([g(0), g(1), g(2)])
            }
            Expr::Paren(p) => self.fuse_vec(&p.expr),
            Expr::Ident(id) if self.vec_vars.contains(&(id.sym.clone(), id.ctxt)) => {
                Some([field(id, "x"), field(id, "y"), field(id, "z")])
            }
            Expr::Call(call) => {
                let (obj, method, args) = as_method_call(call)?;
                let o = self.fuse_vec(obj)?;
                self.apply_vec_method(o, &method, args)
            }
            _ => None,
        }
    }

    fn apply_vec_method(&mut self, o: Comps, method: &str, args: &[ExprOrSpread]) -> Option<Comps> {
        let arg = |i: usize| args.get(i).filter(|a| a.spread.is_none()).map(|a| (*a.expr).clone());
        let [o0, o1, o2] = o;
        match method {
            "add" | "sub" | "mul" => {
                let a0 = arg(0)?;
                let [v0, v1, v2] = self.fuse_vec(&a0)?;
                let op = match method { "add" => BinaryOp::Add, "sub" => BinaryOp::Sub, _ => BinaryOp::Mul };
                Some([bin(o0, op, v0), bin(o1, op, v1), bin(o2, op, v2)])
            }
            "scale" => {
                let s = self.simplify(arg(0)?);
                Some([bin(o0, BinaryOp::Mul, s.clone()), bin(o1, BinaryOp::Mul, s.clone()), bin(o2, BinaryOp::Mul, s)])
            }
            "negate" => Some([neg(o0), neg(o1), neg(o2)]),
            "scaleAndAdd" => {
                let a0 = arg(0)?;
                let [v0, v1, v2] = self.fuse_vec(&a0)?;
                let s = self.simplify(arg(1)?);
                Some([
                    bin(o0, BinaryOp::Add, bin(v0, BinaryOp::Mul, s.clone())),
                    bin(o1, BinaryOp::Add, bin(v1, BinaryOp::Mul, s.clone())),
                    bin(o2, BinaryOp::Add, bin(v2, BinaryOp::Mul, s)),
                ])
            }
            "cross" => {
                let a0 = arg(0)?;
                let v = self.fuse_vec(&a0)?;
                let (ox, oy, oz) = (self.simplify(o0), self.simplify(o1), self.simplify(o2));
                let [v0, v1, v2] = v;
                let (vx, vy, vz) = (self.simplify(v0), self.simplify(v1), self.simplify(v2));
                Some([
                    bin(bin(oy.clone(), BinaryOp::Mul, vz.clone()), BinaryOp::Sub, bin(oz.clone(), BinaryOp::Mul, vy.clone())),
                    bin(bin(oz, BinaryOp::Mul, vx.clone()), BinaryOp::Sub, bin(ox.clone(), BinaryOp::Mul, vz)),
                    bin(bin(ox, BinaryOp::Mul, vy), BinaryOp::Sub, bin(oy, BinaryOp::Mul, vx)),
                ])
            }
            "normalize" => {
                // const l = Math.hypot(x,y,z); each component is `l === 0 ? 0 : c / l`.
                let (ox, oy, oz) = (self.simplify(o0), self.simplify(o1), self.simplify(o2));
                let l = self.simplify(math_hypot(vec![ox.clone(), oy.clone(), oz.clone()]));
                Some([norm_comp(ox, &l), norm_comp(oy, &l), norm_comp(oz, &l)])
            }
            _ => None,
        }
    }

    fn scalar_terminal(&mut self, e: &Expr) -> Option<Expr> {
        let Expr::Call(call) = e else { return None };
        let (obj, method, args) = as_method_call(call)?;
        let o = self.fuse_vec(obj)?;
        let arg = |i: usize| args.get(i).filter(|a| a.spread.is_none()).map(|a| (*a.expr).clone());
        let [o0, o1, o2] = o;
        match method.as_str() {
            "dot" => {
                let a0 = arg(0)?;
                let [v0, v1, v2] = self.fuse_vec(&a0)?;
                Some(sum3(bin(o0, BinaryOp::Mul, v0), bin(o1, BinaryOp::Mul, v1), bin(o2, BinaryOp::Mul, v2)))
            }
            "lengthSq" => {
                let (ox, oy, oz) = (self.simplify(o0), self.simplify(o1), self.simplify(o2));
                Some(sum3(bin(ox.clone(), BinaryOp::Mul, ox), bin(oy.clone(), BinaryOp::Mul, oy), bin(oz.clone(), BinaryOp::Mul, oz)))
            }
            "length" => Some(math_hypot(vec![o0, o1, o2])),
            "distanceTo" => {
                let a0 = arg(0)?;
                let [v0, v1, v2] = self.fuse_vec(&a0)?;
                Some(math_hypot(vec![bin(o0, BinaryOp::Sub, v0), bin(o1, BinaryOp::Sub, v1), bin(o2, BinaryOp::Sub, v2)]))
            }
            _ => None,
        }
    }

    fn is_vec3_ident(&self, e: &Expr) -> bool {
        matches!(e, Expr::Ident(id) if id.sym == self.vec3.0 && id.ctxt == self.vec3.1)
    }
}

fn as_method_call(call: &CallExpr) -> Option<(&Expr, String, &[ExprOrSpread])> {
    let Callee::Expr(callee) = &call.callee else { return None };
    let Expr::Member(MemberExpr { obj, prop: MemberProp::Ident(name), .. }) = &**callee else { return None };
    Some((obj, name.sym.to_string(), &call.args))
}

/// Side-effect-free and cheap to duplicate.
fn is_simple(e: &Expr) -> bool {
    match e {
        Expr::Lit(_) | Expr::Ident(_) | Expr::This(_) => true,
        Expr::Paren(p) => is_simple(&p.expr),
        Expr::Unary(u) if !matches!(u.op, UnaryOp::Delete) => is_simple(&u.arg),
        Expr::Member(m) => {
            (matches!(&m.prop, MemberProp::Ident(_)) && is_simple(&m.obj))
                || (matches!(&m.prop, MemberProp::Computed(c) if matches!(&*c.expr, Expr::Lit(Lit::Num(_)))) && is_simple(&m.obj))
        }
        _ => false,
    }
}

// ---- AST constructors --------------------------------------------------------------------------

fn num(v: f64) -> Expr {
    Expr::Lit(Lit::Num(Number { span: DUMMY_SP, value: v, raw: None }))
}

fn paren(e: Expr) -> Expr {
    match e {
        Expr::Bin(_) | Expr::Unary(_) | Expr::Cond(_) | Expr::Assign(_) | Expr::Seq(_) => {
            Expr::Paren(ParenExpr { span: DUMMY_SP, expr: Box::new(e) })
        }
        other => other,
    }
}

fn bin(left: Expr, op: BinaryOp, right: Expr) -> Expr {
    Expr::Bin(BinExpr { span: DUMMY_SP, op, left: Box::new(paren(left)), right: Box::new(paren(right)) })
}

fn neg(arg: Expr) -> Expr {
    Expr::Unary(UnaryExpr { span: DUMMY_SP, op: UnaryOp::Minus, arg: Box::new(paren(arg)) })
}

fn sum3(a: Expr, b: Expr, c: Expr) -> Expr {
    bin(bin(a, BinaryOp::Add, b), BinaryOp::Add, c)
}

/// `l === 0 ? 0 : c / l`
fn norm_comp(c: Expr, l: &Expr) -> Expr {
    Expr::Cond(CondExpr {
        span: DUMMY_SP,
        test: Box::new(bin(l.clone(), BinaryOp::EqEqEq, num(0.0))),
        cons: Box::new(num(0.0)),
        alt: Box::new(bin(c, BinaryOp::Div, l.clone())),
    })
}

fn plain_ident(name: &str) -> Ident {
    Ident::new(Atom::from(name), DUMMY_SP, SyntaxContext::empty())
}

fn field(id: &Ident, name: &str) -> Expr {
    Expr::Member(MemberExpr {
        span: DUMMY_SP,
        obj: Box::new(Expr::Ident(id.clone())),
        prop: MemberProp::Ident(IdentName::new(Atom::from(name), DUMMY_SP)),
    })
}

fn math_hypot(args: Vec<Expr>) -> Expr {
    let callee = Expr::Member(MemberExpr {
        span: DUMMY_SP,
        obj: Box::new(Expr::Ident(plain_ident("Math"))),
        prop: MemberProp::Ident(IdentName::new(Atom::from("hypot"), DUMMY_SP)),
    });
    Expr::Call(CallExpr {
        span: DUMMY_SP,
        ctxt: SyntaxContext::empty(),
        callee: Callee::Expr(Box::new(callee)),
        args: args.into_iter().map(|e| ExprOrSpread { spread: None, expr: Box::new(e) }).collect(),
        type_args: None,
    })
}

fn new_vec3(c: Comps, key: &Key) -> Expr {
    let id = Ident::new(key.0.clone(), DUMMY_SP, key.1);
    let [x, y, z] = c;
    Expr::New(NewExpr {
        span: DUMMY_SP,
        ctxt: SyntaxContext::empty(),
        callee: Box::new(Expr::Ident(id)),
        args: Some(vec![
            ExprOrSpread { spread: None, expr: Box::new(x) },
            ExprOrSpread { spread: None, expr: Box::new(y) },
            ExprOrSpread { spread: None, expr: Box::new(z) },
        ]),
        type_args: None,
    })
}

fn const_decl(id: &Ident, init: Expr) -> Stmt {
    Stmt::Decl(Decl::Var(Box::new(VarDecl {
        span: DUMMY_SP,
        ctxt: SyntaxContext::empty(),
        kind: VarDeclKind::Const,
        declare: false,
        decls: vec![VarDeclarator {
            span: DUMMY_SP,
            name: Pat::Ident(BindingIdent { id: id.clone(), type_ann: None }),
            init: Some(Box::new(init)),
            definite: false,
        }],
    })))
}

// ---- block-ification (so hoisted temps land in the right scope) --------------------------------

struct BlockIfy;
impl VisitMut for BlockIfy {
    fn visit_mut_stmt(&mut self, s: &mut Stmt) {
        s.visit_mut_children_with(self);
        match s {
            Stmt::If(i) => {
                ensure_block(&mut i.cons);
                if let Some(alt) = &mut i.alt {
                    ensure_block(alt);
                }
            }
            Stmt::For(f) => ensure_block(&mut f.body),
            Stmt::ForIn(f) => ensure_block(&mut f.body),
            Stmt::ForOf(f) => ensure_block(&mut f.body),
            Stmt::While(w) => ensure_block(&mut w.body),
            Stmt::DoWhile(w) => ensure_block(&mut w.body),
            _ => {}
        }
    }
}

fn ensure_block(s: &mut Box<Stmt>) {
    if !matches!(**s, Stmt::Block(_)) {
        let inner = std::mem::replace(&mut **s, Stmt::Empty(EmptyStmt { span: DUMMY_SP }));
        **s = Stmt::Block(BlockStmt { span: DUMMY_SP, ctxt: SyntaxContext::empty(), stmts: vec![inner] });
    }
}

// ---- Vec3 type inference for local variables ---------------------------------------------------

fn is_vec3_typed(e: &Expr, vec3: &Key, vars: &HashSet<Key>) -> bool {
    match e {
        Expr::New(NewExpr { callee, .. }) => matches!(&**callee, Expr::Ident(id) if (id.sym.clone(), id.ctxt) == *vec3),
        Expr::Paren(p) => is_vec3_typed(&p.expr, vec3, vars),
        Expr::Ident(id) => vars.contains(&(id.sym.clone(), id.ctxt)),
        Expr::Member(MemberExpr { obj, prop: MemberProp::Ident(name), .. }) => {
            matches!(&**obj, Expr::Ident(id) if (id.sym.clone(), id.ctxt) == *vec3) && VEC3_STATICS.contains(&name.sym.as_str())
        }
        Expr::Call(call) => {
            if let Callee::Expr(callee) = &call.callee {
                if let Expr::Member(MemberExpr { obj, prop: MemberProp::Ident(name), .. }) = &**callee {
                    let static_call = matches!(&**obj, Expr::Ident(id) if (id.sym.clone(), id.ctxt) == *vec3) && VEC3_STATICS.contains(&name.sym.as_str());
                    let method_call = VEC3_RETURNING.contains(&name.sym.as_str()) && is_vec3_typed(obj, vec3, vars);
                    return static_call || method_call;
                }
            }
            false
        }
        _ => false,
    }
}

struct VarScan {
    decls: Vec<(Key, Expr)>,
}
impl Visit for VarScan {
    fn visit_var_declarator(&mut self, d: &VarDeclarator) {
        if let (Pat::Ident(b), Some(init)) = (&d.name, &d.init) {
            self.decls.push(((b.id.sym.clone(), b.id.ctxt), (**init).clone()));
        }
        d.visit_children_with(self);
    }
}

fn collect_vec3_vars(module: &Module, vec3: &Key) -> HashSet<Key> {
    let mut scan = VarScan { decls: Vec::new() };
    module.visit_with(&mut scan);
    let mut vars = HashSet::new();
    loop {
        let mut changed = false;
        for (k, init) in &scan.decls {
            if !vars.contains(k) && is_vec3_typed(init, vec3, &vars) {
                vars.insert(k.clone());
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    vars
}

/// Run fusion over a module. Returns the number of chains fused.
pub fn fuse_module(module: &mut Module, vec3: Key) -> usize {
    module.visit_mut_with(&mut BlockIfy);
    let vec_vars = collect_vec3_vars(module, &vec3);
    let mut f = Fuser { vec3, vec_vars, temps: Vec::new(), counter: 0, fused: 0 };
    module.visit_mut_with(&mut f);
    f.fused
}
