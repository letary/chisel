//! Codegen — AST back to JS. When `minify` is set we run the full SWC minifier (compress + mangle,
//! including top-level mangling — the whole bundle is one closed scope with no exports, so every
//! top-level name is private) and then print compactly. Without it, a readable pretty-print.

use swc_core::common::source_map::DefaultSourceMapGenConfig;
use swc_core::common::sync::Lrc;
use swc_core::common::{BytePos, LineCol, Mark, SourceMap};
use swc_core::ecma::ast::{Module, Program};
use swc_core::ecma::codegen::text_writer::JsWriter;
use swc_core::ecma::codegen::{Config, Emitter};
use swc_core::ecma::minifier::optimize;
use swc_core::ecma::minifier::option::{ExtraOptions, MangleOptions, MinifyOptions};
use swc_core::ecma::transforms::base::fixer::fixer;
use swc_core::ecma::transforms::base::resolver;
use swc_core::ecma::visit::VisitMutWith;

/// Emit a `Module` to JS source. When `source_map` is set, also returns the source-map JSON (its
/// `sources` are the original module paths, mapped against the shared `SourceMap`).
pub fn codegen(cm: &Lrc<SourceMap>, module: &Module, minify: bool, source_map: bool) -> anyhow::Result<(String, Option<String>)> {
    // `fixer` inserts the parentheses the printer needs to stay correct — e.g. an IIFE at statement
    // position `(function(){…})()` (how TS namespaces/enums lower) would otherwise print as a
    // nameless function *declaration*. The minify path runs it inside `minify_module`; the readable
    // path needs it too.
    let module = if minify { minify_module(cm, module.clone()) } else { fix_module(module.clone()) };

    let mut buf = Vec::new();
    let mut mappings: Vec<(BytePos, LineCol)> = Vec::new();
    {
        let map_arg = if source_map { Some(&mut mappings) } else { None };
        let writer = JsWriter::new(cm.clone(), "\n", &mut buf, map_arg);
        let mut emitter = Emitter {
            cfg: Config::default().with_minify(minify),
            cm: cm.clone(),
            comments: None,
            wr: writer,
        };
        emitter
            .emit_module(&module)
            .map_err(|e| anyhow::anyhow!("codegen error: {e}"))?;
    }
    let code = String::from_utf8(buf)?;

    let map = if source_map {
        let sm = cm.build_source_map(&mappings, None, DefaultSourceMapGenConfig);
        let mut out = Vec::new();
        sm.to_writer(&mut out).map_err(|e| anyhow::anyhow!("source map: {e}"))?;
        Some(String::from_utf8(out)?)
    } else {
        None
    };
    Ok((code, map))
}

/// Add required parentheses (without minifying) so the readable output is always valid JS.
fn fix_module(module: Module) -> Module {
    let mut program = Program::Module(module);
    program.visit_mut_with(&mut fixer(None));
    match program {
        Program::Module(m) => m,
        Program::Script(_) => unreachable!("a Module stays a Module"),
    }
}

/// Run the SWC minifier over the fully-linked bundle. The bundle is a single closed scope (all
/// imports/exports were linked away), so free identifiers are exactly the host globals
/// (`_creator*`, `console`, `Math`, `setLoop`, …) — the resolver marks those `unresolved`, and the
/// mangler leaves them alone while renaming every (now-private) top-level binding.
fn minify_module(cm: &Lrc<SourceMap>, module: Module) -> Module {
    let unresolved_mark = Mark::new();
    let top_level_mark = Mark::new();

    let mut program = Program::Module(module);
    // A fresh resolver pass over the merged module so the minifier sees consistent scopes/marks.
    program.visit_mut_with(&mut resolver(unresolved_mark, top_level_mark, false));

    let mut program = optimize(
        program,
        cm.clone(),
        None,
        None,
        &MinifyOptions {
            compress: Some(Default::default()),
            // `top_level: true` is safe and is the bulk of the win — nothing references these names
            // from outside the bundle.
            mangle: Some(MangleOptions { top_level: Some(true), ..Default::default() }),
            ..Default::default()
        },
        &ExtraOptions { unresolved_mark, top_level_mark, mangle_name_cache: None },
    );

    program.visit_mut_with(&mut fixer(None));

    match program {
        Program::Module(m) => m,
        Program::Script(_) => unreachable!("a Module stays a Module"),
    }
}
