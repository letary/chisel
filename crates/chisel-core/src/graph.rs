//! The module graph: parse every reachable module (resolver + TS-strip), and extract its imports,
//! exports, and the hygiene contexts (`SyntaxContext`) the linker keys on.
//!
//! Each module gets fresh resolver marks, so a top-level binding is globally identified by
//! `(name, top_level_ctxt)` and a free/global reference by `(name, unresolved_ctxt)`.

use std::collections::HashMap;

use swc_core::atoms::{Atom, Wtf8Atom};
use swc_core::common::sync::Lrc;
use swc_core::common::util::take::Take;
use swc_core::common::{Mark, SourceMap, SyntaxContext, DUMMY_SP};
use swc_core::ecma::ast::*;
use swc_core::ecma::transforms::base::resolver;
use swc_core::ecma::visit::{VisitMut, VisitMutWith};

use crate::flatten_ui;
use crate::parse;
use crate::reactive_ui;
use crate::resolve;

/// A specifier that imports a non-JS/TS file is an asset (`./hero.png`, `./model.glb`, `./font.ttf`).
fn is_asset_specifier(spec: &str) -> bool {
    let file = spec.rsplit('/').next().unwrap_or(spec);
    match file.rfind('.') {
        Some(i) => !matches!(&file[i + 1..], "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs"),
        None => false,
    }
}

/// Resolve an asset path to its URL (mapped, else the path itself).
fn asset_url(assets: &HashMap<String, String>, path: &str) -> String {
    assets.get(path).cloned().unwrap_or_else(|| path.to_string())
}

fn string_expr(s: &str) -> Expr {
    Expr::Lit(Lit::Str(Str { span: DUMMY_SP, value: Wtf8Atom::from(s), raw: None }))
}

/// `const <name> = <expr>` with the given binding context (so references resolve to it).
fn const_expr_item(name: &str, expr: Expr, ctxt: SyntaxContext) -> ModuleItem {
    let id = Ident::new(Atom::from(name), DUMMY_SP, ctxt);
    ModuleItem::Stmt(Stmt::Decl(Decl::Var(Box::new(VarDecl {
        span: DUMMY_SP,
        ctxt: SyntaxContext::empty(),
        kind: VarDeclKind::Const,
        declare: false,
        decls: vec![VarDeclarator {
            span: DUMMY_SP,
            name: Pat::Ident(BindingIdent { id, type_ann: None }),
            init: Some(Box::new(expr)),
            definite: false,
        }],
    }))))
}

/// `const <name> = "<url>"`.
fn const_string_item(name: &str, url: &str, ctxt: SyntaxContext) -> ModuleItem {
    const_expr_item(name, string_expr(url), ctxt)
}

/// The synthetic binding name an `export default <…>` is lowered to (one default per module, so a
/// fixed name is fine — `hygiene()` makes it globally unique across modules).
const DEFAULT_LOCAL: &str = "__chisel_default";

/// Lower an `export default` declaration to a normal top-level item: a named class/function keeps its
/// own binding; an anonymous one is bound to the synthetic default name.
fn default_decl_to_item(decl: DefaultDecl, ctxt: SyntaxContext) -> ModuleItem {
    match decl {
        DefaultDecl::Class(c) => match c.ident {
            Some(ident) => ModuleItem::Stmt(Stmt::Decl(Decl::Class(ClassDecl { ident, declare: false, class: c.class }))),
            None => const_expr_item(DEFAULT_LOCAL, Expr::Class(ClassExpr { ident: None, class: c.class }), ctxt),
        },
        DefaultDecl::Fn(f) => match f.ident {
            Some(ident) => ModuleItem::Stmt(Stmt::Decl(Decl::Fn(FnDecl { ident, declare: false, function: f.function }))),
            None => const_expr_item(DEFAULT_LOCAL, Expr::Fn(FnExpr { ident: None, function: f.function }), ctxt),
        },
        DefaultDecl::TsInterfaceDecl(_) => ModuleItem::Stmt(Stmt::Empty(EmptyStmt { span: DUMMY_SP })),
    }
}

/// Pre-parse `define` values into replacement expressions (numeric or boolean literals).
fn parse_define(define: &HashMap<String, String>) -> HashMap<String, Expr> {
    define
        .iter()
        .filter_map(|(k, v)| {
            let v = v.trim();
            let expr = match v {
                "true" | "false" => Some(Expr::Lit(Lit::Bool(Bool { span: DUMMY_SP, value: v == "true" }))),
                _ => v
                    .parse::<f64>()
                    .ok()
                    .map(|n| Expr::Lit(Lit::Num(Number { span: DUMMY_SP, value: n, raw: None }))),
            };
            expr.map(|e| (k.clone(), e))
        })
        .collect()
}

/// Replaces free identifiers matching a `define` key with the substituted value (esbuild `define:`).
struct DefineSubst<'a> {
    define: &'a HashMap<String, Expr>,
    unresolved_ctxt: SyntaxContext,
}
impl VisitMut for DefineSubst<'_> {
    fn visit_mut_expr(&mut self, e: &mut Expr) {
        e.visit_mut_children_with(self);
        if let Expr::Ident(id) = e {
            // Only a *free* reference (a top-level/local `DEG2RAD` const must be left alone).
            if id.ctxt == self.unresolved_ctxt {
                if let Some(repl) = self.define.get(id.sym.as_str()) {
                    *e = repl.clone();
                }
            }
        }
    }
}

/// The literal boolean value of an expression, if it is one (through parens).
fn bool_lit(e: &Expr) -> Option<bool> {
    match e {
        Expr::Lit(Lit::Bool(b)) => Some(b.value),
        Expr::Paren(p) => bool_lit(&p.expr),
        _ => None,
    }
}

/// Folds boolean-literal branches after `define` substitution, so `if (EDITOR) { … }` under
/// `EDITOR: "false"` disappears from the AST *before* linking — anything referenced only from the
/// dead branch loses its last edge and is tree-shaken like ordinary unused code. Handles `!b`,
/// `b && x` / `b || x`, `b ? a : c`, and `if (b)` statements (bottom-up, so `if (!EDITOR)` folds
/// too). Caveat: `var`/function hoisting out of a dropped branch is not preserved — the SDK and
/// project code are `let`/`const` TS, where this cannot be observed.
struct ConstFold;

impl VisitMut for ConstFold {
    fn visit_mut_expr(&mut self, e: &mut Expr) {
        e.visit_mut_children_with(self);
        let folded = match e {
            Expr::Unary(u) if u.op == UnaryOp::Bang => {
                bool_lit(&u.arg).map(|b| Expr::Lit(Lit::Bool(Bool { span: u.span, value: !b })))
            }
            Expr::Bin(b) if b.op == BinaryOp::LogicalAnd => {
                bool_lit(&b.left).map(|l| *(if l { b.right.take() } else { b.left.take() }))
            }
            Expr::Bin(b) if b.op == BinaryOp::LogicalOr => {
                bool_lit(&b.left).map(|l| *(if l { b.left.take() } else { b.right.take() }))
            }
            Expr::Cond(c) => bool_lit(&c.test).map(|t| *(if t { c.cons.take() } else { c.alt.take() })),
            _ => None,
        };
        if let Some(f) = folded {
            *e = f;
        }
    }

    fn visit_mut_stmt(&mut self, s: &mut Stmt) {
        s.visit_mut_children_with(self);
        if let Stmt::If(i) = s {
            if let Some(t) = bool_lit(&i.test) {
                *s = if t {
                    *i.cons.take()
                } else {
                    match i.alt.take() {
                        Some(alt) => *alt,
                        None => Stmt::Empty(EmptyStmt { span: i.span }),
                    }
                };
            }
        }
    }

    // Prune the empty statements folding leaves behind (statement lists + module top level).
    fn visit_mut_stmts(&mut self, stmts: &mut Vec<Stmt>) {
        stmts.visit_mut_children_with(self);
        stmts.retain(|s| !matches!(s, Stmt::Empty(_)));
    }

    fn visit_mut_module_items(&mut self, items: &mut Vec<ModuleItem>) {
        items.visit_mut_children_with(self);
        items.retain(|i| !matches!(i, ModuleItem::Stmt(Stmt::Empty(_))));
    }
}

/// Replaces `asset("./x")` calls with the asset's URL string (compile-time macro).
struct AssetDesugar<'a> {
    assets: &'a HashMap<String, String>,
    unresolved_ctxt: SyntaxContext,
}
impl VisitMut for AssetDesugar<'_> {
    fn visit_mut_expr(&mut self, e: &mut Expr) {
        e.visit_mut_children_with(self);
        if let Expr::Call(call) = e {
            if let Callee::Expr(callee) = &call.callee {
                if let Expr::Ident(id) = &**callee {
                    if id.sym == "asset" && id.ctxt == self.unresolved_ctxt && call.args.len() == 1 && call.args[0].spread.is_none() {
                        if let Expr::Lit(Lit::Str(s)) = &*call.args[0].expr {
                            let url = asset_url(self.assets, s.value.as_str().unwrap_or(""));
                            *e = string_expr(&url);
                        }
                    }
                }
            }
        }
    }
}

/// A single value-level import specifier (`import { imported as local }`).
#[derive(Debug, Clone)]
pub struct ImportSpec {
    pub local: String,
    pub imported: String,
}

/// One export entry, before cross-module origin resolution.
#[derive(Debug, Clone)]
pub enum RawExport {
    /// `export <decl>` or `export { local as name }` — a binding defined in this module.
    Local { local: String },
    /// `export { imported as name } from "./x"` — re-export from another module (resolved path).
    ReExport { source: String, imported: String },
}

/// `export * from "./x"` (resolved path).
#[derive(Debug, Clone)]
pub struct StarReExport {
    pub source: String,
}

pub struct ModuleInfo {
    pub id: usize,
    pub path: String,
    pub module: Module,
    pub unresolved_ctxt: SyntaxContext,
    pub top_level_ctxt: SyntaxContext,
    /// Resolved import edges: (target module id, specifiers).
    pub imports: Vec<(usize, Vec<ImportSpec>)>,
    /// exportName → where it comes from.
    pub exports: HashMap<String, RawExport>,
    /// `export * from` sources (resolved paths).
    pub star_exports: Vec<StarReExport>,
}

pub struct ModuleGraph {
    pub modules: Vec<ModuleInfo>,
    pub path_to_id: HashMap<String, usize>,
}

impl ModuleGraph {
    pub fn get(&self, id: usize) -> &ModuleInfo {
        &self.modules[id]
    }
}

/// A module specifier / string literal as `&str`. Module paths are always valid UTF-8, so the
/// WTF-8 fallback never fires in practice.
fn as_str(a: &swc_core::atoms::Wtf8Atom) -> &str {
    a.as_str().unwrap_or("")
}

fn export_name(n: &ModuleExportName) -> String {
    match n {
        ModuleExportName::Ident(i) => i.sym.to_string(),
        ModuleExportName::Str(s) => as_str(&s.value).to_string(),
    }
}

/// The `SyntaxContext` resolver assigned to this module's top-level scope, read off the first
/// top-level binding (robust against any drift in how marks map to contexts). `None` if the module
/// declares nothing (e.g. a pure re-export hub like `inject.ts`), in which case it's never the
/// origin of a reference and the value is unused.
fn first_top_binding_ctxt(module: &Module) -> Option<SyntaxContext> {
    for item in &module.body {
        let decl = match item {
            ModuleItem::ModuleDecl(ModuleDecl::ExportDecl(e)) => Some(&e.decl),
            ModuleItem::Stmt(Stmt::Decl(d)) => Some(d),
            _ => None,
        };
        if let Some(d) = decl {
            match d {
                Decl::Class(c) => return Some(c.ident.ctxt),
                Decl::Fn(f) => return Some(f.ident.ctxt),
                Decl::Var(v) => {
                    for dd in &v.decls {
                        if let Pat::Ident(b) = &dd.name {
                            return Some(b.id.ctxt);
                        }
                    }
                }
                _ => {}
            }
        }
    }
    None
}

/// Names bound by a top-level `Decl` (the simple cases the SDK / user code use).
fn decl_names(decl: &Decl) -> Vec<String> {
    match decl {
        Decl::Class(c) => vec![c.ident.sym.to_string()],
        Decl::Fn(f) => vec![f.ident.sym.to_string()],
        Decl::Var(v) => v
            .decls
            .iter()
            .filter_map(|d| match &d.name {
                Pat::Ident(b) => Some(b.id.sym.to_string()),
                _ => None,
            })
            .collect(),
        _ => vec![],
    }
}

/// Build the graph from `entries` (the user entry first, then any SDK inject entries).
pub fn build(
    cm: &Lrc<SourceMap>,
    files: &HashMap<String, String>,
    entries: &[String],
    assets: &HashMap<String, String>,
    define: &HashMap<String, String>,
    reactive_ui: bool,
    flatten_ui: bool,
) -> anyhow::Result<ModuleGraph> {
    let exists = |p: &str| files.contains_key(p);
    let define_exprs = parse_define(define);

    let mut modules: Vec<ModuleInfo> = Vec::new();
    let mut path_to_id: HashMap<String, usize> = HashMap::new();
    // Each entry holds the raw (string-source) imports/exports until all paths are interned.
    let mut raw_imports: Vec<Vec<(String, Vec<ImportSpec>)>> = Vec::new();

    let mut queue: Vec<String> = Vec::new();
    let enqueue = |q: &mut Vec<String>, seen: &HashMap<String, usize>, p: String| {
        if !seen.contains_key(&p) && !q.contains(&p) {
            q.push(p);
        }
    };
    for e in entries {
        enqueue(&mut queue, &path_to_id, e.clone());
    }

    while let Some(path) = queue.pop() {
        if path_to_id.contains_key(&path) {
            continue;
        }
        let src = files
            .get(&path)
            .ok_or_else(|| anyhow::anyhow!("Module not found: {path}"))?;

        let mut module = parse::parse_module(cm, &path, src)?;
        let unresolved_mark = Mark::new();
        let top_level_mark = Mark::new();
        module.visit_mut_with(&mut resolver(unresolved_mark, top_level_mark, true));
        parse::strip_types(&mut module, unresolved_mark, top_level_mark);

        let unresolved_ctxt = SyntaxContext::empty().apply_mark(unresolved_mark);
        let top_level_ctxt =
            first_top_binding_ctxt(&module).unwrap_or_else(|| SyntaxContext::empty().apply_mark(top_level_mark));

        // Desugar `asset("./x")` calls to their URL string (compile-time macro).
        module.visit_mut_with(&mut AssetDesugar { assets, unresolved_ctxt });
        // Substitute `define`d free globals (DEG2RAD/RAD2DEG → numeric literals, EDITOR → a
        // boolean), then fold the branches boolean defines decide.
        if !define_exprs.is_empty() {
            module.visit_mut_with(&mut DefineSubst { define: &define_exprs, unresolved_ctxt });
            if define_exprs.values().any(|e| matches!(e, Expr::Lit(Lit::Bool(_)))) {
                module.visit_mut_with(&mut ConstFold);
            }
        }
        // Reactive-UI desugaring (memoized children maps + auto-wrapped signal reads). Keys on
        // free (unresolved) references to the injected globals, so SDK modules are inert.
        if reactive_ui {
            reactive_ui::apply(&path, unresolved_ctxt, &mut module);
        }
        // Flatten literal array args of UI factory calls into variadic args (the runtime flattens
        // array arguments one level either way; this saves an allocation per call site). Runs
        // after reactive_ui so its position-based decisions see the original call shapes.
        if flatten_ui {
            flatten_ui::apply(unresolved_ctxt, &mut module);
        }

        let mut imports: Vec<(String, Vec<ImportSpec>)> = Vec::new();
        let mut exports: HashMap<String, RawExport> = HashMap::new();
        let mut star_exports: Vec<StarReExport> = Vec::new();
        // Asset default-imports (`import hero from './hero.png'`) with no backing module become
        // `const hero = "<url>"`.
        let mut asset_consts: Vec<(usize, String, String)> = Vec::new();
        // `export default <…>` item indices to lower to a named binding after the scan.
        let mut default_decls: Vec<usize> = Vec::new();

        for (item_idx, item) in module.body.iter().enumerate() {
            let ModuleItem::ModuleDecl(decl) = item else { continue };
            match decl {
                ModuleDecl::Import(imp) => {
                    if imp.type_only {
                        continue;
                    }
                    let spec_str = as_str(&imp.src.value);
                    let resolved = resolve::resolve(&exists, &path, spec_str).ok();
                    // An asset specifier with NO backing module (standalone / local testing) → a
                    // compile-time URL constant. In production the asset is a real `export default
                    // "<url>"` module (resolved below as a normal default import).
                    if resolved.is_none() && is_asset_specifier(spec_str) {
                        if let [ImportSpecifier::Default(d)] = imp.specifiers.as_slice() {
                            asset_consts.push((item_idx, d.local.sym.to_string(), asset_url(assets, spec_str)));
                            continue;
                        }
                    }
                    let mut specs = Vec::new();
                    for s in &imp.specifiers {
                        match s {
                            ImportSpecifier::Named(n) if !n.is_type_only => {
                                let imported = n.imported.as_ref().map(export_name).unwrap_or_else(|| n.local.sym.to_string());
                                specs.push(ImportSpec { local: n.local.sym.to_string(), imported });
                            }
                            ImportSpecifier::Named(_) => {}
                            // `import x from './m'` → bind `x` to module `m`'s default export.
                            ImportSpecifier::Default(d) => {
                                specs.push(ImportSpec { local: d.local.sym.to_string(), imported: "default".to_string() });
                            }
                            ImportSpecifier::Namespace(_) => {
                                anyhow::bail!("{path}: namespace imports (`import * as ns`) are not supported: {}", spec_str)
                            }
                        }
                    }
                    // `resolved` is Some unless the specifier is bare/unknown — re-run to surface the error.
                    let target = match resolved {
                        Some(t) => t,
                        None => resolve::resolve(&exists, &path, spec_str)?,
                    };
                    enqueue(&mut queue, &path_to_id, target.clone());
                    // Record the edge even for a side-effect-only import (`import './setup'`, no
                    // specifiers) so the target stays reachable from the entry and is treated as user
                    // code (its top-level side effects are kept, not dropped as if it were the SDK).
                    imports.push((target, specs));
                }
                ModuleDecl::ExportDecl(e) => {
                    for name in decl_names(&e.decl) {
                        exports.insert(name.clone(), RawExport::Local { local: name });
                    }
                }
                ModuleDecl::ExportNamed(e) => {
                    if e.type_only {
                        continue;
                    }
                    let src = e.src.as_ref().map(|s| as_str(&s.value).to_string());
                    let resolved_src = match &src {
                        Some(s) => Some(resolve::resolve(&exists, &path, s)?),
                        None => None,
                    };
                    if let Some(rs) = &resolved_src {
                        enqueue(&mut queue, &path_to_id, rs.clone());
                    }
                    for s in &e.specifiers {
                        match s {
                            ExportSpecifier::Named(n) if !n.is_type_only => {
                                let orig = export_name(&n.orig);
                                let name = n.exported.as_ref().map(export_name).unwrap_or_else(|| orig.clone());
                                match &resolved_src {
                                    Some(rs) => {
                                        exports.insert(name, RawExport::ReExport { source: rs.clone(), imported: orig });
                                    }
                                    None => {
                                        exports.insert(name, RawExport::Local { local: orig });
                                    }
                                }
                            }
                            ExportSpecifier::Named(_) => {}
                            ExportSpecifier::Namespace(_) | ExportSpecifier::Default(_) => {
                                anyhow::bail!("{path}: namespace/default export specifiers are not supported")
                            }
                        }
                    }
                }
                ModuleDecl::ExportAll(e) => {
                    if e.type_only {
                        continue;
                    }
                    let rs = resolve::resolve(&exists, &path, as_str(&e.src.value))?;
                    enqueue(&mut queue, &path_to_id, rs.clone());
                    star_exports.push(StarReExport { source: rs });
                }
                // `export default <expr>` → `const __chisel_default = <expr>`; the value can be a
                // runtime expression (a shader URL template reading `_creator.backend`, `SvgSource(…)`).
                ModuleDecl::ExportDefaultExpr(_) => {
                    exports.insert("default".to_string(), RawExport::Local { local: DEFAULT_LOCAL.to_string() });
                    default_decls.push(item_idx);
                }
                // `export default class/function …` → a named decl keeps its name; an anonymous one
                // is bound to the synthetic default name.
                ModuleDecl::ExportDefaultDecl(d) => {
                    let local = match &d.decl {
                        DefaultDecl::Class(c) => Some(c.ident.as_ref().map(|i| i.sym.to_string()).unwrap_or_else(|| DEFAULT_LOCAL.to_string())),
                        DefaultDecl::Fn(f) => Some(f.ident.as_ref().map(|i| i.sym.to_string()).unwrap_or_else(|| DEFAULT_LOCAL.to_string())),
                        DefaultDecl::TsInterfaceDecl(_) => None, // type-only, dropped on strip
                    };
                    if let Some(local) = local {
                        exports.insert("default".to_string(), RawExport::Local { local });
                        default_decls.push(item_idx);
                    }
                }
                ModuleDecl::TsImportEquals(_) | ModuleDecl::TsExportAssignment(_) | ModuleDecl::TsNamespaceExport(_) => {}
            }
        }

        // Rewrite asset default-imports in place: `import hero from './x.png'` → `const hero = "<url>"`.
        for (idx, name, url) in asset_consts {
            module.body[idx] = const_string_item(&name, &url, top_level_ctxt);
        }

        // Lower `export default <…>` items to plain top-level bindings.
        for idx in default_decls {
            let taken = std::mem::replace(&mut module.body[idx], ModuleItem::Stmt(Stmt::Empty(EmptyStmt { span: DUMMY_SP })));
            module.body[idx] = match taken {
                ModuleItem::ModuleDecl(ModuleDecl::ExportDefaultExpr(e)) => const_expr_item(DEFAULT_LOCAL, *e.expr, top_level_ctxt),
                ModuleItem::ModuleDecl(ModuleDecl::ExportDefaultDecl(d)) => default_decl_to_item(d.decl, top_level_ctxt),
                other => other, // unreachable, but keep the body intact
            };
        }

        let id = modules.len();
        path_to_id.insert(path.clone(), id);
        raw_imports.push(imports);
        modules.push(ModuleInfo {
            id,
            path,
            module,
            unresolved_ctxt,
            top_level_ctxt,
            imports: Vec::new(),
            exports,
            star_exports,
        });
    }

    // Second pass: intern import-target paths to module ids.
    for (i, raw) in raw_imports.into_iter().enumerate() {
        let mut resolved = Vec::new();
        for (src, specs) in raw {
            let target = *path_to_id
                .get(&src)
                .ok_or_else(|| anyhow::anyhow!("internal: unresolved module {src}"))?;
            resolved.push((target, specs));
        }
        modules[i].imports = resolved;
    }

    Ok(ModuleGraph { modules, path_to_id })
}

/// If `local` is an imported name in `m`, return its (target module id, imported name).
fn import_origin(m: &ModuleInfo, local: &str) -> Option<(usize, String)> {
    for (tid, specs) in &m.imports {
        for s in specs {
            if s.local == local {
                return Some((*tid, s.imported.clone()));
            }
        }
    }
    None
}

/// Follow `export`/re-export chains to the concrete binding that defines `name` in `module`.
/// Returns `(module_id, local_name)` — a real top-level binding.
pub fn resolve_export(graph: &ModuleGraph, module: usize, name: &str) -> Option<(usize, String)> {
    resolve_export_inner(graph, module, name, 0)
}

fn resolve_export_inner(graph: &ModuleGraph, module: usize, name: &str, depth: usize) -> Option<(usize, String)> {
    if depth > 64 {
        return None; // cycle guard
    }
    let m = &graph.modules[module];
    if let Some(exp) = m.exports.get(name) {
        return match exp {
            RawExport::Local { local } => {
                // `export { x }` where `x` is itself imported (`import { x } from "./y"`): follow on.
                if let Some((tid, imported)) = import_origin(m, local) {
                    return resolve_export_inner(graph, tid, &imported, depth + 1);
                }
                Some((module, local.clone()))
            }
            RawExport::ReExport { source, imported } => {
                let src_id = *graph.path_to_id.get(source)?;
                resolve_export_inner(graph, src_id, imported, depth + 1)
            }
        };
    }
    // Fall back to `export *` sources.
    for star in &m.star_exports {
        let src_id = *graph.path_to_id.get(&star.source)?;
        if let Some(found) = resolve_export_inner(graph, src_id, name, depth + 1) {
            return Some(found);
        }
    }
    None
}
