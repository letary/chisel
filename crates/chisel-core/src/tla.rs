//! Top-level-await rejection. Hosts run the bundle as a plain script (QuickJS `JS_EVAL_TYPE_GLOBAL`,
//! `new Function(code)` on the web), where `await` at module scope is a SyntaxError that takes the
//! whole app down at load time. Catch it at compile time instead, per module, with a source position.
//!
//! Runs on the parsed module before any transform, so spans still point at the user's source.

use swc_core::common::sync::Lrc;
use swc_core::common::{SourceMap, Span};
use swc_core::ecma::ast::*;
use swc_core::ecma::visit::{Visit, VisitWith};

/// The first `await` at module scope of `module` (an `await expr`, `for await`, or `await using`),
/// or `None`. Anything inside a function / arrow / method / accessor body is fine and skipped.
pub fn find_top_level_await(module: &Module) -> Option<Span> {
    let mut v = Finder { found: None };
    module.visit_with(&mut v);
    v.found
}

/// Reject a module with a top-level `await`: `path:line:col: top-level await …`.
pub fn check_module(cm: &Lrc<SourceMap>, path: &str, module: &Module) -> anyhow::Result<()> {
    let Some(span) = find_top_level_await(module) else { return Ok(()) };
    let pos = match cm.try_lookup_char_pos(span.lo) {
        Ok(l) => format!("{path}:{}:{}", l.line, l.col_display + 1),
        Err(_) => path.to_string(),
    };
    anyhow::bail!(
        "{pos}: top-level await is not supported — the app bundle runs as a plain script, not a module. \
         Move the awaiting code into an async function and call it (e.g. `const main = async () => {{ … }}; main()`)."
    )
}

struct Finder {
    found: Option<Span>,
}

impl Finder {
    fn hit(&mut self, span: Span) {
        if self.found.is_none() {
            self.found = Some(span);
        }
    }
}

impl Visit for Finder {
    // `await` is only legal inside these; a top-level check must not descend into them.
    fn visit_function(&mut self, _: &Function) {}
    fn visit_arrow_expr(&mut self, _: &ArrowExpr) {}

    fn visit_await_expr(&mut self, a: &AwaitExpr) {
        self.hit(a.span);
    }

    fn visit_for_of_stmt(&mut self, f: &ForOfStmt) {
        if f.is_await {
            self.hit(f.span);
        }
        f.visit_children_with(self);
    }

    fn visit_using_decl(&mut self, u: &UsingDecl) {
        if u.is_await {
            self.hit(u.span);
        }
        u.visit_children_with(self);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse;

    fn has_tla(src: &str) -> bool {
        let cm = parse::source_map();
        let m = parse::parse_module(&cm, "/t.ts", src).expect("parse");
        find_top_level_await(&m).is_some()
    }

    #[test]
    fn detects_top_level_forms() {
        assert!(has_tla("const x = await f()"));
        assert!(has_tla("for await (const x of xs) {}"));
        assert!(has_tla("if (a) { const x = await f() }"));
        assert!(has_tla("export const v = await Promise.resolve(1)"));
        assert!(has_tla("console.log((await f()).x)"));
        assert!(has_tla("await using r = open()"));
    }

    #[test]
    fn ignores_awaits_inside_functions() {
        assert!(!has_tla("async function f() { await g() }"));
        assert!(!has_tla("const f = async () => { await g() }; f()"));
        assert!(!has_tla("class C { async m() { await g() } static async s() { for await (const x of xs) {} } }"));
        assert!(!has_tla("const o = { async m() { await g() } }"));
        assert!(!has_tla("(async () => { await g() })()"));
        assert!(!has_tla("const await_ = 1; console.log(await_)"));
    }
}
