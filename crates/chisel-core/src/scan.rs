//! Scan mode: parse every JS/TS module and report its resolved local imports and whether it has
//! value-level exports — the dependency facts entry detection needs before an entry is known.
//! Advisory by design: a per-file parse failure lands in that file's `error` instead of failing
//! the scan, and unresolvable specifiers (bare imports, missing files, assets) are skipped.

use std::collections::HashMap;

use serde::Serialize;
use swc_core::ecma::ast::*;

use crate::parse;
use crate::resolve;

#[derive(Debug, Serialize)]
pub struct ScanModule {
    /// Resolved paths of the JS/TS modules this file imports or re-exports from (deduped, in
    /// order of appearance). Type-only imports count — the target is still a dependency.
    pub imports: Vec<String>,
    /// True when the module has any value-level export (type-only exports don't count).
    #[serde(rename = "hasExports")]
    pub has_exports: bool,
    /// Parse failure message (`imports`/`hasExports` are then empty/false).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// A path that parses as a JS/TS module — both scan candidates and counted import targets.
fn is_code(path: &str) -> bool {
    if path.ends_with(".d.ts") {
        return false;
    }
    [".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs"].iter().any(|e| path.ends_with(e))
}

fn as_str(a: &swc_core::atoms::Wtf8Atom) -> &str {
    a.as_str().unwrap_or("")
}

pub fn scan(files: &HashMap<String, String>) -> HashMap<String, ScanModule> {
    let cm = parse::source_map();
    let exists = |p: &str| files.contains_key(p);
    let mut out = HashMap::new();

    for (path, src) in files {
        if !is_code(path) {
            continue;
        }
        let module = match parse::parse_module(&cm, path, src) {
            Ok(m) => m,
            Err(e) => {
                out.insert(path.clone(), ScanModule { imports: vec![], has_exports: false, error: Some(format!("{e:#}")) });
                continue;
            }
        };

        let mut imports: Vec<String> = Vec::new();
        let mut has_exports = false;
        let mut add = |imports: &mut Vec<String>, spec: &str| {
            if let Ok(target) = resolve::resolve(&exists, path, spec) {
                if is_code(&target) && !imports.contains(&target) {
                    imports.push(target);
                }
            }
        };

        for item in &module.body {
            let ModuleItem::ModuleDecl(decl) = item else { continue };
            match decl {
                ModuleDecl::Import(imp) => add(&mut imports, as_str(&imp.src.value)),
                ModuleDecl::ExportDecl(e) => {
                    // `export interface` / `export type` are type-only; enums/namespaces are values.
                    has_exports |= matches!(e.decl, Decl::Class(_) | Decl::Fn(_) | Decl::Var(_) | Decl::TsEnum(_) | Decl::TsModule(_));
                }
                ModuleDecl::ExportNamed(e) => {
                    if let Some(src) = &e.src {
                        add(&mut imports, as_str(&src.value));
                    }
                    if e.type_only {
                        continue;
                    }
                    has_exports |= e.specifiers.iter().any(|s| match s {
                        ExportSpecifier::Named(n) => !n.is_type_only,
                        _ => true,
                    });
                }
                ModuleDecl::ExportAll(e) => {
                    add(&mut imports, as_str(&e.src.value));
                    if !e.type_only {
                        has_exports = true;
                    }
                }
                ModuleDecl::ExportDefaultDecl(d) => {
                    has_exports |= !matches!(d.decl, DefaultDecl::TsInterfaceDecl(_));
                }
                ModuleDecl::ExportDefaultExpr(_) => has_exports = true,
                _ => {}
            }
        }
        out.insert(path.clone(), ScanModule { imports, has_exports, error: None });
    }
    out
}
