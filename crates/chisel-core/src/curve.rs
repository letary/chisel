//! Particle curve chain fusion (pure data assembly) + compile-time validation.
//!
//! The SDK's `curve()` / `colorCurve()` builders exist to make particle config readable:
//!
//! ```js
//! new Particles({ opacity: curve(0.3).fade(0.15), color: colorCurve('#ffaa33').to('#000000') })
//! ```
//!
//! A builder's entire state is one flat `Float32Array` behind its `_data` getter — the exact payload
//! the native `setParticleSystemConfig` record expects. So a chain whose arguments are literals is
//! *known at compile time*: this pass lowers it to `{ _data: new Float32Array([…]) }`, the same duck
//! type the runtime builder presents, so the `Particles` setters have one consumption path either
//! way. When every chain in a bundle fuses, the builder classes and the hex-color parser become
//! unreferenced and the linker drops them (fusion runs before DCE, like the Vec3/date passes).
//!
//! Unlike Vec3/date fusion this is not about allocation — curves are cold config. The wins are
//! (1) the bundle shipping the exact native buffer, and (2) **validation with file/line
//! diagnostics**: `t` out of order or outside 0..1, more than 8 stops, `.from()` after another stop,
//! an unparsable color literal. Native would only warn about those at runtime, from the device.
//! Validation therefore runs *always*; the rewrite only when `fuse` is on.
//!
//! Two deliberate restrictions, both because `CurveBuilder` is **mutable** (`.to()` returns `this`):
//!
//! * A chain is only rewritten where its value is **immediately consumed** — a call argument, an
//!   object-literal property, or the right side of a member assignment (`p.opacity = curve(…)`),
//!   plus the pass-throughs that forward a value to the same consumer (`??`, `?:`, parens). A chain
//!   bound to a variable is left alone: `const c = curve(1)` … `c.to(0)` must keep working.
//! * A chain that can't be lowered (a non-literal stop `t`, a computed color, an unknown method) is
//!   left **entirely** as real calls — we never rewrite a *sub-chain*, or `curve(1).via(x, 2)` would
//!   turn into `{_data}.via(x, 2)`.

use swc_core::atoms::Atom;
use swc_core::common::sync::Lrc;
use swc_core::common::{SourceMap, Span, Spanned, SyntaxContext, DUMMY_SP};
use swc_core::ecma::ast::*;
use swc_core::ecma::visit::{VisitMut, VisitMutWith};

use crate::fusion::{is_simple, nth, num, plain_ident, str_lit, str_lit_of};

type Key = (Atom, SyntaxContext);

/// `kPsMaxCurveStops` in `creator-gl/src/particles.h` — a longer curve is rejected native-side.
const MAX_STOPS: usize = 8;

/// Builder methods that add a stop. Every one of them mutates and returns `this`.
const STOP_METHODS: &[&str] = &["from", "via", "to", "fade"];

/// The two factory identities the pass keys on, resolved by the linker from the injected SDK globals
/// (`packages/sdk/src/gl/Particles.ts`). Either may be absent — a project's SDK need not export both.
pub struct CurveCtx {
    pub curve: Option<Key>,
    pub color_curve: Option<Key>,
}

impl CurveCtx {
    pub fn is_empty(&self) -> bool {
        self.curve.is_none() && self.color_curve.is_none()
    }
}

/// One stop: `t` must be a literal (we validate ordering and the payload layout is static anyway).
/// `vals` holds the lo/hi expression pair — 2 floats for a numeric curve, 8 (lo rgba + hi rgba) for
/// a color one.
struct Stop {
    t: f64,
    vals: Vec<Expr>,
}

/// A fully-lowered chain, mirroring the builder's own fields.
struct Chain {
    color: bool,
    kind: f64,       // 0 multiply / 1 add (numeric curves only)
    base: Vec<Expr>, // 2 floats (numeric) or 8 (color lo/hi rgba)
    stops: Vec<Stop>,
}

impl Chain {
    fn factory(&self) -> &'static str {
        if self.color {
            "colorCurve"
        } else {
            "curve"
        }
    }

    /// `true` when the builder prepends an implicit identity stop at t = 0 (its `_data` getter does
    /// this whenever the first explicit stop starts late).
    fn prepends(&self) -> bool {
        self.stops.first().map_or(false, |s| s.t > 0.0)
    }

    fn stop_count(&self) -> usize {
        self.stops.len() + self.prepends() as usize
    }

    /// What native would reject at set time, if anything (`applyRecord` + `validStops`).
    fn invalid(&self) -> Option<String> {
        let n = self.stop_count();
        if n > MAX_STOPS {
            return Some(format!("{n} stops, but a curve holds at most {MAX_STOPS}"));
        }
        // The implicit stop sits at t = 0, so an explicit t = 0 after it would not ascend either.
        let mut prev = if self.prepends() { 0.0 } else { -1.0 };
        for s in &self.stops {
            if !(0.0..=1.0).contains(&s.t) {
                return Some(format!("stop t={} is outside 0..1", s.t));
            }
            if s.t <= prev {
                return Some(format!("stop t={} does not come after t={prev} (stops must ascend)", s.t));
            }
            prev = s.t;
        }
        None
    }

    /// The record payload, element for element as the builder's `_data` getter writes it.
    fn data(self) -> Vec<Expr> {
        let Chain { color, kind, base, stops } = self;
        let prepend = stops.first().map_or(false, |s| s.t > 0.0);
        let n = stops.len() + prepend as usize;
        let mut out = base;
        if !color {
            out.insert(0, num(kind));
        }
        out.push(num(n as f64));
        if prepend {
            // A late-starting curve holds the identity from birth: 1 for multiply / white for a
            // color multiply, 0 for add. Same value in the lo and hi slots (no per-particle lerp).
            let identity = if color {
                1.0
            } else if kind == 1.0 {
                0.0
            } else {
                1.0
            };
            out.push(num(0.0));
            for _ in 0..if color { 8 } else { 2 } {
                out.push(num(identity));
            }
        }
        for s in stops {
            out.push(num(s.t));
            out.extend(s.vals);
        }
        out
    }
}

pub struct CurveFuser<'a> {
    ctx: &'a CurveCtx,
    cm: &'a Lrc<SourceMap>,
    /// Rewrite chains, or only validate them (`fuse` off).
    rewrite: bool,
    diags: Vec<String>,
    /// Set by the parent hooks below while descending into a position whose value is immediately
    /// consumed. `visit_mut_expr` takes it, so it never leaks past one expression.
    allow: bool,
    pub fused: usize,
}

// ---- traversal ---------------------------------------------------------------------------------

impl VisitMut for CurveFuser<'_> {
    fn visit_mut_expr(&mut self, e: &mut Expr) {
        let allow = std::mem::take(&mut self.allow);
        if self.chain_root(e).is_some() {
            self.handle_chain(e, allow);
            return; // never descend into a chain: a fused sub-chain would break the outer call
        }
        match e {
            // Value pass-throughs: the result reaches the same consumer, so it inherits `allow`.
            Expr::Paren(p) => {
                self.allow = allow;
                p.expr.visit_mut_with(self);
            }
            Expr::Cond(c) => {
                c.test.visit_mut_with(self);
                self.allow = allow;
                c.cons.visit_mut_with(self);
                self.allow = allow;
                c.alt.visit_mut_with(self);
            }
            Expr::Bin(b) if matches!(b.op, BinaryOp::NullishCoalescing | BinaryOp::LogicalOr | BinaryOp::LogicalAnd) => {
                self.allow = allow;
                b.left.visit_mut_with(self);
                self.allow = allow;
                b.right.visit_mut_with(self);
            }
            Expr::Seq(s) => {
                let last = s.exprs.len().saturating_sub(1);
                for (i, x) in s.exprs.iter_mut().enumerate() {
                    self.allow = allow && i == last;
                    x.visit_mut_with(self);
                }
            }
            other => {
                other.visit_mut_children_with(self);
            }
        }
        self.allow = false;
    }

    fn visit_mut_call_expr(&mut self, c: &mut CallExpr) {
        c.callee.visit_mut_with(self);
        self.visit_args(&mut c.args);
    }

    fn visit_mut_new_expr(&mut self, n: &mut NewExpr) {
        n.callee.visit_mut_with(self);
        if let Some(args) = &mut n.args {
            self.visit_args(args);
        }
    }

    fn visit_mut_prop(&mut self, p: &mut Prop) {
        if let Prop::KeyValue(kv) = p {
            kv.key.visit_mut_with(self);
            self.allow = true;
            kv.value.visit_mut_with(self);
            self.allow = false;
            return;
        }
        p.visit_mut_children_with(self);
    }

    fn visit_mut_assign_expr(&mut self, a: &mut AssignExpr) {
        a.left.visit_mut_with(self);
        // `p.opacity = curve(…)` / `p.custom[0] = curve(…)` — the setter consumes it immediately.
        // An assignment to a plain identifier does NOT qualify: the binding could be extended later.
        self.allow = a.op == AssignOp::Assign && matches!(&a.left, AssignTarget::Simple(SimpleAssignTarget::Member(_)));
        a.right.visit_mut_with(self);
        self.allow = false;
    }
}

impl CurveFuser<'_> {
    fn visit_args(&mut self, args: &mut [ExprOrSpread]) {
        for a in args.iter_mut() {
            self.allow = a.spread.is_none();
            a.expr.visit_mut_with(self);
            self.allow = false;
        }
    }

    /// Validate the chain and (when allowed) replace it with its flat payload. Either way the chain
    /// is consumed — the caller must not descend into it.
    fn handle_chain(&mut self, e: &mut Expr, allow: bool) {
        let span = e.span();
        let Some(chain) = self.build(e) else { return };
        if let Some(problem) = chain.invalid() {
            self.warn(span, chain.factory(), &problem);
            return;
        }
        if self.rewrite && allow {
            self.fused += 1;
            *e = data_object(chain.data());
        }
    }

    fn warn(&mut self, span: Span, factory: &str, msg: &str) {
        let at = loc(self.cm, span);
        self.diags.push(format!("{at}: {factory}(): {msg}"));
    }

    /// `Some(is_color)` when this expression is a curve chain: a factory call, or a stop method on
    /// one. Cheap — it only walks the callee spine.
    fn chain_root(&self, e: &Expr) -> Option<bool> {
        let Expr::Call(call) = unparen(e) else { return None };
        let Callee::Expr(callee) = &call.callee else { return None };
        match unparen(callee) {
            Expr::Ident(id) => {
                let k = (id.sym.clone(), id.ctxt);
                if self.ctx.curve.as_ref() == Some(&k) {
                    Some(false)
                } else if self.ctx.color_curve.as_ref() == Some(&k) {
                    Some(true)
                } else {
                    None
                }
            }
            Expr::Member(MemberExpr { obj, prop: MemberProp::Ident(name), .. }) if STOP_METHODS.contains(&name.sym.as_str()) => {
                self.chain_root(obj)
            }
            _ => None,
        }
    }

    // ---- lowering (mirrors the builder method bodies) ------------------------------------------

    fn build(&mut self, e: &Expr) -> Option<Chain> {
        let Expr::Call(call) = unparen(e) else { return None };
        let Callee::Expr(callee) = &call.callee else { return None };
        match unparen(callee) {
            Expr::Ident(_) => {
                let color = self.chain_root(e)?;
                if color {
                    self.color_root(call)
                } else {
                    self.num_root(call)
                }
            }
            Expr::Member(MemberExpr { obj, prop: MemberProp::Ident(name), .. }) if STOP_METHODS.contains(&name.sym.as_str()) => {
                let mut chain = self.build(obj)?;
                self.apply(&mut chain, &name.sym, &call.args, call.span)?;
                Some(chain)
            }
            _ => None,
        }
    }

    /// `curve(base = 1, mode = 'multiply')`.
    fn num_root(&mut self, call: &CallExpr) -> Option<Chain> {
        let base = match nth(&call.args, 0) {
            Some(b) => num_pair(&b)?,
            None => vec![num(1.0), num(1.0)],
        };
        let kind = match nth(&call.args, 1) {
            None => 0.0,
            Some(m) => match str_lit_of(&m)?.as_str() {
                "add" => 1.0,
                "multiply" => 0.0,
                other => {
                    self.warn(call.span, "curve", &format!("unknown mode '{other}' (expected 'multiply' or 'add')"));
                    return None;
                }
            },
        };
        Some(Chain { color: false, kind, base, stops: Vec::new() })
    }

    /// `colorCurve(base = '#ffffff')` — always a multiply.
    fn color_root(&mut self, call: &CallExpr) -> Option<Chain> {
        let base = match nth(&call.args, 0) {
            Some(b) => self.color_pair(&b, call.span)?,
            None => vec![num(1.0); 8],
        };
        Some(Chain { color: true, kind: 0.0, base, stops: Vec::new() })
    }

    fn apply(&mut self, chain: &mut Chain, method: &str, args: &[ExprOrSpread], span: Span) -> Option<()> {
        match method {
            "from" => {
                if !chain.stops.is_empty() {
                    // The builder throws on this; leave the call in place so it still does.
                    self.warn(span, chain.factory(), ".from() must come before any other stop");
                    return None;
                }
                let vals = self.stop_value(chain, &nth(args, 0)?, span)?;
                chain.stops.push(Stop { t: 0.0, vals });
            }
            "via" => {
                let t = num_lit_of(&nth(args, 0)?)?;
                let vals = self.stop_value(chain, &nth(args, 1)?, span)?;
                chain.stops.push(Stop { t, vals });
            }
            "to" => {
                let vals = self.stop_value(chain, &nth(args, 0)?, span)?;
                chain.stops.push(Stop { t: 1.0, vals });
            }
            // `.fade(fadeIn = 0.15, fadeOut = fadeIn)` — numeric curves only, and the stop *count*
            // depends on the clamped values, so both must be literals.
            "fade" => {
                if chain.color {
                    return None;
                }
                let fade_in = match nth(args, 0) {
                    Some(a) => num_lit_of(&a)?,
                    None => 0.15,
                };
                let fade_out = match nth(args, 1) {
                    Some(a) => num_lit_of(&a)?,
                    None => fade_in,
                };
                let up = fade_in.max(0.0).min(1.0);
                let down = fade_out.max(0.0).min(1.0 - up);
                let mut push = |t: f64, v: f64| chain.stops.push(Stop { t, vals: vec![num(v), num(v)] });
                if up > 0.0 {
                    push(0.0, 0.0);
                    push(up, 1.0);
                } else {
                    push(0.0, 1.0);
                }
                if down > 0.0 {
                    if 1.0 - down > up {
                        push(1.0 - down, 1.0);
                    }
                    push(1.0, 0.0);
                }
            }
            _ => return None,
        }
        Some(())
    }

    fn stop_value(&mut self, chain: &Chain, v: &Expr, span: Span) -> Option<Vec<Expr>> {
        if chain.color {
            self.color_pair(v, span)
        } else {
            num_pair(v)
        }
    }

    /// A color stop: `'#hex'` / packed number / `[r,g,b,a?]`, or a `{ min, max }` range of those.
    /// Colors must be literal — the runtime writes the same parsed rgba into *both* the lo and hi
    /// slots, so a computed value would have to be evaluated twice.
    fn color_pair(&mut self, e: &Expr, span: Span) -> Option<Vec<Expr>> {
        if let Expr::Object(o) = unparen(e) {
            let (min, max) = obj_min_max(o)?;
            let lo = self.rgba(&min, span)?;
            let hi = self.rgba(&max, span)?;
            return Some(lo.iter().chain(hi.iter()).map(|v| num(*v)).collect());
        }
        let c = self.rgba(e, span)?;
        Some(c.iter().chain(c.iter()).map(|v| num(*v)).collect())
    }

    /// Parse a literal color to four 0..1 components — the compile-time twin of the SDK's
    /// `Color.toRgba01` (`packages/sdk/src/core/color.ts`).
    fn rgba(&mut self, e: &Expr, span: Span) -> Option<[f64; 4]> {
        match unparen(e) {
            Expr::Lit(Lit::Str(s)) => {
                let raw = s.value.as_str()?;
                match parse_hex(raw) {
                    Some(c) => Some(c),
                    None => {
                        self.warn(span, "colorCurve", &format!("'{raw}' is not a color (#rgb / #rgba / #rrggbb / #rrggbbaa)"));
                        None
                    }
                }
            }
            // A packed int, e.g. 0xff8800 — `((c >> 16) & 255) / 255`, …, alpha 1.
            other if num_lit_of(other).is_some() => {
                let v = to_int32(num_lit_of(other)?);
                Some([
                    (((v >> 16) & 255) as f64) / 255.0,
                    (((v >> 8) & 255) as f64) / 255.0,
                    ((v & 255) as f64) / 255.0,
                    1.0,
                ])
            }
            // Normalized components — passed through as-is by the runtime.
            Expr::Array(a) if (3..=4).contains(&a.elems.len()) => {
                let mut out = [1.0; 4];
                for (i, el) in a.elems.iter().enumerate() {
                    let el = el.as_ref().filter(|x| x.spread.is_none())?;
                    out[i] = num_lit_of(&el.expr)?;
                }
                Some(out)
            }
            _ => None,
        }
    }
}

// ---- numeric values ----------------------------------------------------------------------------

/// A `Range<number>` (`number | { min, max }`) as the (lo, hi) pair the builder's `lohi` produces.
fn num_pair(e: &Expr) -> Option<Vec<Expr>> {
    if let Expr::Object(o) = unparen(e) {
        let (min, max) = obj_min_max(o)?;
        return Some(vec![min, max]);
    }
    if let Some(v) = num_lit_of(e) {
        return Some(vec![num(v), num(v)]);
    }
    // Not a literal: mirror `lohi` inline. Only for expressions that are free to evaluate twice —
    // the pair reads the value in both slots (and a `typeof` test on each).
    if is_simple(e) {
        return Some(vec![lohi_pick(e, "min"), lohi_pick(e, "max")]);
    }
    None
}

/// `typeof v === "number" ? v : v.min` — `lohi()`, inlined.
fn lohi_pick(e: &Expr, field: &str) -> Expr {
    let test = Expr::Bin(BinExpr {
        span: DUMMY_SP,
        op: BinaryOp::EqEqEq,
        left: Box::new(Expr::Unary(UnaryExpr { span: DUMMY_SP, op: UnaryOp::TypeOf, arg: Box::new(e.clone()) })),
        right: Box::new(str_lit("number")),
    });
    Expr::Cond(CondExpr {
        span: DUMMY_SP,
        test: Box::new(test),
        cons: Box::new(e.clone()),
        alt: Box::new(Expr::Member(MemberExpr {
            span: DUMMY_SP,
            obj: Box::new(e.clone()),
            prop: MemberProp::Ident(IdentName::new(Atom::from(field), DUMMY_SP)),
        })),
    })
}

/// The `min` / `max` values of an object literal. Both must be present (the runtime reads both), and
/// out-of-order props are only accepted for expressions that are safe to reorder — the payload
/// always evaluates min before max.
fn obj_min_max(o: &ObjectLit) -> Option<(Expr, Expr)> {
    let mut min: Option<(usize, Expr)> = None;
    let mut max: Option<(usize, Expr)> = None;
    for (i, p) in o.props.iter().enumerate() {
        let PropOrSpread::Prop(prop) = p else { return None };
        let (name, value) = match &**prop {
            Prop::KeyValue(kv) => (prop_key(&kv.key)?, (*kv.value).clone()),
            Prop::Shorthand(id) => (id.sym.to_string(), Expr::Ident(id.clone())),
            _ => return None,
        };
        match name.as_str() {
            "min" => min = Some((i, value)),
            "max" => max = Some((i, value)),
            _ => return None,
        }
    }
    let (mi, min) = min?;
    let (xi, max) = max?;
    if mi > xi && !(is_simple(&min) && is_simple(&max)) {
        return None;
    }
    Some((min, max))
}

fn prop_key(k: &PropName) -> Option<String> {
    match k {
        PropName::Ident(i) => Some(i.sym.to_string()),
        PropName::Str(s) => Some(s.value.as_str()?.to_string()),
        _ => None,
    }
}

// ---- literals ----------------------------------------------------------------------------------

fn unparen(e: &Expr) -> &Expr {
    match e {
        Expr::Paren(p) => unparen(&p.expr),
        other => other,
    }
}

/// A numeric literal, through parens and a unary sign (`-0.5`).
fn num_lit_of(e: &Expr) -> Option<f64> {
    match e {
        Expr::Lit(Lit::Num(n)) => Some(n.value),
        Expr::Paren(p) => num_lit_of(&p.expr),
        Expr::Unary(u) => match u.op {
            UnaryOp::Minus => num_lit_of(&u.arg).map(|v| -v),
            UnaryOp::Plus => num_lit_of(&u.arg),
            _ => None,
        },
        _ => None,
    }
}

/// `ToInt32` — what JS `>>` / `&` do to a number before shifting.
fn to_int32(v: f64) -> i32 {
    if !v.is_finite() {
        return 0;
    }
    (v.trunc().rem_euclid(4294967296.0) as u32) as i32
}

/// `'#rgb' | '#rgba' | '#rrggbb' | '#rrggbbaa'` → four 0..1 components, exactly as the SDK's
/// `parseHexString` does. `None` for anything else — the runtime silently falls back to black
/// (or to `parseInt`'s prefix parse), which is the mistake worth catching at build time.
fn parse_hex(input: &str) -> Option<[f64; 4]> {
    let s = input.trim();
    let s = s.strip_prefix('#').unwrap_or(s);
    let s = match s.len() {
        3 | 4 => s.chars().flat_map(|c| [c, c]).collect::<String>(),
        _ => s.to_string(),
    };
    if !s.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let int = u32::from_str_radix(&s, 16).ok()?;
    let b = match s.len() {
        8 => [(int >> 24) & 255, (int >> 16) & 255, (int >> 8) & 255, int & 255],
        6 => [(int >> 16) & 255, (int >> 8) & 255, int & 255, 255],
        _ => return None,
    };
    Some([b[0] as f64 / 255.0, b[1] as f64 / 255.0, b[2] as f64 / 255.0, b[3] as f64 / 255.0])
}

// ---- output ------------------------------------------------------------------------------------

/// `{ _data: new Float32Array([…]) }` — the duck type the runtime builder presents through its
/// `_data` getter, so `Particles`' record assembly has one consumption path.
fn data_object(values: Vec<Expr>) -> Expr {
    let arr = Expr::Array(ArrayLit {
        span: DUMMY_SP,
        elems: values.into_iter().map(|e| Some(ExprOrSpread { spread: None, expr: Box::new(e) })).collect(),
    });
    let buf = Expr::New(NewExpr {
        span: DUMMY_SP,
        ctxt: SyntaxContext::empty(),
        callee: Box::new(Expr::Ident(plain_ident("Float32Array"))),
        args: Some(vec![ExprOrSpread { spread: None, expr: Box::new(arr) }]),
        type_args: None,
    });
    Expr::Object(ObjectLit {
        span: DUMMY_SP,
        props: vec![PropOrSpread::Prop(Box::new(Prop::KeyValue(KeyValueProp {
            key: PropName::Ident(IdentName::new(Atom::from("_data"), DUMMY_SP)),
            value: Box::new(buf),
        })))],
    })
}

/// `path:line:col` for a diagnostic, or a placeholder for synthesized nodes.
fn loc(cm: &Lrc<SourceMap>, span: Span) -> String {
    if span.is_dummy() {
        return "<generated>".to_string();
    }
    match cm.try_lookup_char_pos(span.lo) {
        Ok(l) => format!("{}:{}:{}", l.file.name, l.line, l.col_display + 1),
        Err(_) => "<unknown>".to_string(),
    }
}

/// Run curve validation over a module, rewriting fusable chains when `rewrite` is set. Diagnostics
/// are appended to `diags`; the return value is the number of chains fused.
pub fn fuse_curves_module(
    module: &mut Module,
    ctx: &CurveCtx,
    rewrite: bool,
    cm: &Lrc<SourceMap>,
    diags: &mut Vec<String>,
) -> usize {
    let mut f = CurveFuser { ctx, cm, rewrite, diags: Vec::new(), allow: false, fused: 0 };
    module.visit_mut_with(&mut f);
    diags.append(&mut f.diags);
    f.fused
}
