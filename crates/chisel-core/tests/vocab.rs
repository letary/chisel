//! The vocabulary contract (`Input.vocab`, `vocab.rs`): the passes key on the names the SDK declares
//! and use ONLY them; without a manifest the built-in defaults apply; a newer schema is a hard error;
//! a group an older manifest lacks takes its default.

use std::collections::HashMap;

use chisel_core::{bundle, Format, Input, Output};
use serde_json::{json, Value};

mod common;

/// A miniature SDK carrying both the real names and the renamed ones the tests switch to.
const SDK: &str = r#"
export const UIColumn = (...args: any[]) => ({ args })
export const UIText = (t: any) => ({ t })
export const __UIColumn = (children: any) => ({ children })
export const Col = (...args: any[]) => ({ args })
export const rawCol = (children: any) => ({ children })
export class Vec3 {
  constructor(public x = 0, public y = 0, public z = 0) {}
  dot(v: Vec3) { return this.x * v.x + this.y * v.y + this.z * v.z }
  inner(v: Vec3) { return this.x * v.x + this.y * v.y + this.z * v.z }
}
"#;

fn run(main: &str, vocab: Option<Value>) -> Output {
    run_with_assets(main, vocab, Default::default())
}

fn run_with_assets(main: &str, vocab: Option<Value>, assets: HashMap<String, String>) -> Output {
    let files = [("/main.ts", main), ("/sdk/inject.ts", SDK)]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect::<HashMap<_, _>>();
    bundle(Input {
        files,
        entry: "/main.ts".into(),
        scan: false,
        inject: vec!["/sdk/inject.ts".into()],
        format: Format::Esm,
        minify: false,
        fuse: true,
        assets,
        define: Default::default(),
        sourcemap: false,
        keep: Default::default(),
        reactive_ui: false,
        flatten_ui: true,
        vocab,
    })
}

fn ok(main: &str, vocab: Option<Value>) -> String {
    let out = run(main, vocab);
    assert!(out.error.is_none(), "unexpected error: {:?}", out.error);
    out.code
}

fn has(code: &str, needle: &str) -> bool {
    let squish = |s: &str| s.chars().filter(|c| !c.is_whitespace()).collect::<String>();
    squish(code).contains(&squish(needle))
}

const UI_MAIN: &str = "console.log(UIColumn([UIText('a')]), Col([UIText('b')]))";
const VEC_MAIN: &str = "console.log(new Vec3(1, 2, 3).dot(new Vec3(4, 5, 6)), new Vec3(1, 2, 3).inner(new Vec3(4, 5, 6)))";

/// The fixture with `ui` renamed: `Col` is the only factory, `rawCol` its raw builder.
fn renamed_ui() -> Value {
    let mut v = common::sdk_vocab().unwrap();
    v["ui"]["factories"] = json!(["Col"]);
    v["ui"]["raw"] = json!({ "Col": "rawCol" });
    v
}

#[test]
fn the_fixture_is_a_complete_manifest_of_this_schema() {
    let v = common::sdk_vocab().unwrap();
    assert_eq!(v["schema"].as_u64(), Some(chisel_core::vocab::SCHEMA), "fixtures/vocab.json is not this chisel's schema");
    for group in ["ui", "signals", "fusion", "compWrite", "assets"] {
        assert!(v.get(group).is_some(), "fixtures/vocab.json lacks `{group}` — the tests would run on a default");
    }
    chisel_core::vocab::Vocab::from_input(Some(&v)).expect("the fixture parses");
}

#[test]
fn without_a_manifest_the_builtin_defaults_apply() {
    let code = ok(UI_MAIN, None);
    assert!(has(&code, "__UIColumn([UIText('a')])"), "the default factories were not lowered:\n{code}");
    assert!(has(&code, "Col([UIText('b')])") && !has(&code, "rawCol("), "a non-default name was touched:\n{code}");
    let code = ok(VEC_MAIN, None);
    assert!(!code.contains(".dot("), "the default `dot` was not fused:\n{code}");
    assert!(code.contains(".inner("), "a non-default method was fused:\n{code}");
}

#[test]
fn a_manifest_is_used_alone() {
    // The renamed factory is lowered through the manifest's raw builder; the default one is not
    // touched at all (not even spliced).
    let code = ok(UI_MAIN, Some(renamed_ui()));
    assert!(has(&code, "rawCol([UIText('b')])"), "the manifest's factory was not lowered:\n{code}");
    assert!(has(&code, "UIColumn([UIText('a')])") && !code.contains("__UIColumn"), "a default name was used:\n{code}");

    // A role renamed in the manifest: `inner` is now the dot product, `dot` is nothing.
    let mut v = common::sdk_vocab().unwrap();
    v["fusion"]["vec3"]["methods"]["dot"] = json!("inner");
    let code = ok(VEC_MAIN, Some(v));
    assert!(!code.contains(".inner("), "the manifest's method was not fused:\n{code}");
    assert!(code.contains(".dot("), "the default method name was still fused:\n{code}");
}

#[test]
fn a_newer_schema_is_a_hard_error() {
    let mut v = common::sdk_vocab().unwrap();
    v["schema"] = json!(chisel_core::vocab::SCHEMA + 1);
    let err = run(UI_MAIN, Some(v)).error.expect("a newer schema must fail");
    assert!(err.contains("upgrade @letary/chisel"), "no upgrade hint: {err}");
    assert!(err.contains(&format!("schema {}", chisel_core::vocab::SCHEMA + 1)), "the schema is not named: {err}");
}

#[test]
fn a_manifest_without_a_schema_is_an_error() {
    let mut v = common::sdk_vocab().unwrap();
    v.as_object_mut().unwrap().remove("schema");
    let err = run(UI_MAIN, Some(v)).error.expect("a manifest without `schema` must fail");
    assert!(err.contains("missing `schema`"), "{err}");
}

#[test]
fn a_schema_that_is_not_an_integer_is_named() {
    for bad in [json!("2"), json!(2.5), json!(-1)] {
        let mut v = common::sdk_vocab().unwrap();
        v["schema"] = bad.clone();
        let err = run(UI_MAIN, Some(v)).error.expect("a non-integer schema must fail");
        assert!(err.contains("must be a non-negative integer") && err.contains(&bad.to_string()), "{bad}: {err}");
    }
}

#[test]
fn the_asset_macro_follows_the_manifest() {
    let assets = HashMap::from([("./logo.png".to_string(), "https://cdn.example/logo.png".to_string())]);
    let main = "console.log(asset('./logo.png'), res('./logo.png'))";
    let code = run_with_assets(main, common::sdk_vocab(), assets.clone()).code;
    assert!(code.contains("\"https://cdn.example/logo.png\"") && code.contains("res('./logo.png')"), "the manifest's macro was not lowered alone:\n{code}");

    let mut v = common::sdk_vocab().unwrap();
    v["assets"]["macro"] = json!("res");
    let code = run_with_assets(main, Some(v), assets).code;
    assert!(code.contains("asset('./logo.png')") && code.contains("\"https://cdn.example/logo.png\""), "the renamed macro was not followed:\n{code}");
}

#[test]
fn a_role_mapped_to_nothing_is_off() {
    let mut v = common::sdk_vocab().unwrap();
    v["fusion"]["vec3"]["methods"]["dot"] = json!("");
    let code = ok(VEC_MAIN, Some(v));
    assert!(code.contains(".dot("), "a role mapped to \"\" still fused:\n{code}");
}

#[test]
fn a_missing_group_takes_its_default() {
    // A manifest without `fusion` (what a manifest of a schema older than the group looks like):
    // the UI group is the manifest's, Vec3 fusion the default.
    let mut v = renamed_ui();
    v.as_object_mut().unwrap().remove("fusion");
    let code = ok(&format!("{UI_MAIN}\n{VEC_MAIN}"), Some(v));
    assert!(has(&code, "rawCol([UIText('b')])"), "the manifest's ui group was not used:\n{code}");
    assert!(!code.contains(".dot("), "the missing fusion group did not fall back to the default:\n{code}");
}

#[test]
fn an_unknown_field_is_an_error() {
    let mut v = common::sdk_vocab().unwrap();
    v["ui"]["chian"] = json!(["style"]);
    let err = run(UI_MAIN, Some(v)).error.expect("an unknown field must fail");
    assert!(err.contains("chian"), "the field is not named: {err}");
}
