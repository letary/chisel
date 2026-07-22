//! Scan mode: per-module import/export facts without an entry.

use std::collections::HashMap;

use chisel_core::scan::scan;

fn files(entries: &[(&str, &str)]) -> HashMap<String, String> {
    entries.iter().map(|(p, s)| (p.to_string(), s.to_string())).collect()
}

#[test]
fn reports_imports_and_exports() {
    let out = scan(&files(&[
        ("/main.ts", "import './register'\nimport { helper } from './util'\nconsole.log(helper)"),
        ("/register.ts", "console.log('side effect')"),
        ("/util.ts", "export const helper = 1"),
    ]));
    let main = &out["/main.ts"];
    assert_eq!(main.imports, vec!["/register.ts".to_string(), "/util.ts".to_string()]);
    assert!(!main.has_exports);
    assert!(!out["/register.ts"].has_exports);
    assert!(out["/register.ts"].imports.is_empty());
    assert!(out["/util.ts"].has_exports);
}

#[test]
fn type_only_exports_dont_count_but_their_import_edges_do() {
    let out = scan(&files(&[
        ("/types.ts", "export interface Config { a: number }\nexport type B = string"),
        ("/hub.ts", "export type { Config } from './types'"),
        ("/enum.ts", "export enum E { A }"),
    ]));
    assert!(!out["/types.ts"].has_exports, "interface/type exports are not value exports");
    assert!(!out["/hub.ts"].has_exports);
    assert_eq!(out["/hub.ts"].imports, vec!["/types.ts".to_string()], "type re-export still records the edge");
    assert!(out["/enum.ts"].has_exports, "enums are values");
}

#[test]
fn skips_non_code_and_tolerates_errors() {
    let out = scan(&files(&[
        ("/main.ts", "import hero from './hero.png'\nimport 'acme-ui'\nconsole.log(hero)"),
        ("/hero.png", "\u{0}binary"),
        ("/globals.d.ts", "declare const X: number"),
        ("/broken.ts", "import { from"),
    ]));
    assert!(out["/main.ts"].imports.is_empty(), "assets and bare imports are not module edges");
    assert!(!out.contains_key("/hero.png"));
    assert!(!out.contains_key("/globals.d.ts"), ".d.ts files are not scanned");
    assert!(out["/broken.ts"].error.is_some());
    assert!(out["/broken.ts"].imports.is_empty());
}

#[test]
fn re_exports_and_star_exports() {
    let out = scan(&files(&[
        ("/index.ts", "export * from './a'\nexport { b } from './b'"),
        ("/a.ts", "export const a = 1"),
        ("/b.ts", "export const b = 2"),
    ]));
    let idx = &out["/index.ts"];
    assert!(idx.has_exports);
    assert_eq!(idx.imports, vec!["/a.ts".to_string(), "/b.ts".to_string()]);
}

#[test]
fn scan_via_bundle_input() {
    let input: chisel_core::Input = serde_json::from_str(
        r#"{ "scan": true, "files": { "/main.ts": "import './x'", "/x.ts": "console.log(1)" } }"#,
    )
    .unwrap();
    let out = chisel_core::bundle(input);
    assert!(out.error.is_none());
    let scan = out.scan.expect("scan result present");
    assert_eq!(scan["/main.ts"].imports, vec!["/x.ts".to_string()]);
}
