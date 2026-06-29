//! Parsing + TS type-stripping. SWC gives us an owned (`'static`) AST, which is what we want for a
//! bundler that moves nodes between modules and mutates them freely (no arena lifetimes to fight).

use swc_core::common::sync::Lrc;
use swc_core::common::util::take::Take;
use swc_core::common::{FileName, Mark, SourceMap};
use swc_core::ecma::ast::{EsVersion, Module, Program};
use swc_core::ecma::parser::{lexer::Lexer, Parser, StringInput, Syntax, TsSyntax};
use swc_core::ecma::transforms::typescript::strip;

/// One shared source map for the whole bundle (spans / diagnostics resolve against it).
pub fn source_map() -> Lrc<SourceMap> {
    Default::default()
}

/// Parse one TS/TSX source into a `Module`.
pub fn parse_module(cm: &Lrc<SourceMap>, name: &str, src: &str) -> anyhow::Result<Module> {
    let fm = cm.new_source_file(Lrc::new(FileName::Custom(name.to_string())), src.to_string());
    let syntax = Syntax::Typescript(TsSyntax {
        tsx: name.ends_with("tsx") || name.ends_with("jsx"),
        ..Default::default()
    });
    let lexer = Lexer::new(syntax, EsVersion::Es2022, StringInput::from(&*fm), None);
    let mut parser = Parser::new_from(lexer);
    parser
        .parse_module()
        .map_err(|e| anyhow::anyhow!("parse error in {name}: {:?}", e.kind()))
}

/// Strip TypeScript types in place. Must run inside a `GLOBALS` scope, after `resolver`.
/// `strip` is a `Pass` (operates on `Program`), so we bridge the `Module` through one.
pub fn strip_types(module: &mut Module, unresolved: Mark, top_level: Mark) {
    let mut program = Program::Module(module.take());
    program.mutate(strip(unresolved, top_level));
    *module = match program {
        Program::Module(m) => m,
        Program::Script(_) => unreachable!("strip never turns a Module into a Script"),
    };
}
