//! Particle curve chain fusion (`curve()` / `colorCurve()`), against the real SDK builders in
//! `fixtures/curve-sdk/`. These lock the emitted payload and the diagnostics; the *bit-exactness*
//! of fused vs un-fused output over a broad corpus is `scripts/curve-exact.mjs`.

use std::collections::HashMap;

use chisel_core::{bundle, Format, Input, Output};

const CURVE_SDK: &str = include_str!("../../../fixtures/curve-sdk/curve.ts");
const CURVE_INJECT: &str = include_str!("../../../fixtures/curve-sdk/inject.ts");

/// Bundle `main` with the curve SDK injected. `keep: ["_*"]` mirrors lecodes-cli (the `_data` getter
/// is read by the host, so nothing in the bundle references it).
fn build(main: &str, fuse: bool) -> Output {
    let files = [("/main.ts", main), ("/__sdk/inject.ts", CURVE_INJECT), ("/__sdk/curve.ts", CURVE_SDK)]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect::<HashMap<_, _>>();
    let out = bundle(Input {
        files,
        entry: "/main.ts".into(),
        scan: false,
        inject: vec!["/__sdk/inject.ts".into()],
        format: Format::Esm,
        minify: false,
        fuse,
        assets: Default::default(),
        define: Default::default(),
        sourcemap: false,
        keep: vec!["_*".into()],
        reactive_ui: false, flatten_ui: false,
    });
    assert!(out.error.is_none(), "error: {:?}", out.error);
    out
}

/// The emitted `new Float32Array([…])` payload as a comma-joined string, for readable assertions.
fn payload(code: &str) -> String {
    let start = code.find("new Float32Array([").expect(&format!("no fused buffer in:\n{code}")) + "new Float32Array([".len();
    let end = code[start..].find("])").expect("unterminated buffer") + start;
    code[start..end].split_whitespace().collect::<Vec<_>>().join("")
}

#[test]
fn curve_chain_lowers_to_the_native_payload() {
    // [kind, baseLo, baseHi, n, (t, lo, hi) × n] — the PARAM_CURVE record, straight from the bundle.
    let out = build("sink(curve(0.3).fade(0.15))", true);
    assert_eq!(payload(&out.code), "0,0.3,0.3,4,0,0,0,0.15,1,1,0.85,1,1,1,0,0");
    // Nothing of the builder survives: the chain was the only reference to it.
    assert!(!out.code.contains("class CurveBuilder"), "builder should be DCE'd after fusion:\n{}", out.code);
    assert!(!out.code.contains(".fade("), "fade call fused away:\n{}", out.code);
    assert!(out.diagnostics.is_empty(), "unexpected diagnostics: {:?}", out.diagnostics);
}

#[test]
fn late_first_stop_gets_the_implicit_identity_stop() {
    // No stop at t=0 → the builder prepends (t=0, identity) so the curve holds the identity from
    // birth: 1 for a multiply curve, 0 for an `add` one.
    let mul = build("sink(curve(1).via(0.3, 0).to(1))", true);
    assert_eq!(payload(&mul.code), "0,1,1,3,0,1,1,0.3,0,0,1,1,1");
    let add = build("sink(curve(0,'add').via(0.5, 0).to(2))", true);
    assert_eq!(payload(&add.code), "1,0,0,3,0,0,0,0.5,0,0,1,2,2");
    // `.from(v)` IS the t=0 stop (`v` is the value at birth), so nothing is prepended.
    let from = build("sink(curve(1).from(0.3).to(1))", true);
    assert_eq!(payload(&from.code), "0,1,1,2,0,0.3,0.3,1,1,1");
}

#[test]
fn ranges_split_into_the_lo_hi_slots() {
    let out = build("sink(curve({ min: 1, max: 1.6 }).from({ min: 0, max: 0.5 }))", true);
    assert_eq!(payload(&out.code), "0,1,1.6,1,0,0,0.5");
    // Same, one stop later — plus the implicit identity start.
    let late = build("sink(curve({ min: 1, max: 1.6 }).to({ min: 0, max: 0.5 }))", true);
    assert_eq!(payload(&late.code), "0,1,1.6,2,0,1,1,1,0,0.5");
}

#[test]
fn color_literals_are_parsed_at_compile_time() {
    // The hex strings never reach the bundle, and neither does the parser.
    let out = build("sink(colorCurve('#ff8000').to('#000000'))", true);
    assert!(!out.code.contains("#ff8000"), "hex literal should be gone:\n{}", out.code);
    assert!(!out.code.contains("parseHexString") && !out.code.contains("class ColorCurveBuilder"), "color parser should be DCE'd:\n{}", out.code);
    // [baseLo rgba, baseHi rgba, n, (t, lo rgba, hi rgba) × n]: #ff8000 → 1, 128/255, 0, 1 in both
    // base slots, then the implicit white start stop and opaque black at death.
    assert_eq!(
        payload(&out.code),
        "1,0.5019607843137255,0,1,1,0.5019607843137255,0,1,2,0,1,1,1,1,1,1,1,1,1,0,0,0,1,0,0,0,1"
    );
}

#[test]
fn non_literal_values_inline_the_lohi_test() {
    // A `Range<number>` that isn't a literal still fuses: `lohi()` is mirrored in place, so the same
    // expression can be either a number or a { min, max }.
    let out = build("const s = 2\nsink(curve(s).to(0))", true);
    assert!(out.code.contains("typeof s === \"number\" ? s : s.min"), "lohi should be inlined:\n{}", out.code);
    assert!(out.code.contains("typeof s === \"number\" ? s : s.max"), "lohi should be inlined:\n{}", out.code);
    assert!(!out.code.contains("class CurveBuilder"), "builder still fused away:\n{}", out.code);
}

#[test]
fn a_chain_bound_to_a_variable_is_left_alone() {
    // The builder is mutable, so a binding may be extended later — never fuse one.
    let out = build("const c = curve(1)\nc.to(0)\nsink(c)", true);
    assert!(out.code.contains("class CurveBuilder"), "builder must be kept:\n{}", out.code);
    assert!(out.code.contains(".to(0)"), "the real call must survive:\n{}", out.code);
    assert!(!out.code.contains("new Float32Array(["), "nothing should be fused:\n{}", out.code);
}

#[test]
fn a_partly_dynamic_chain_is_left_whole() {
    // The `t` isn't a literal → bail. Crucially the *inner* `curve(1)` must not fuse either, or the
    // surviving `.via(t, 2)` would land on a plain object.
    let out = build("const t = 0.4\nsink(curve(1).via(t, 2).to(0))", true);
    assert!(!out.code.contains("new Float32Array(["), "sub-chain must not fuse:\n{}", out.code);
    assert!(out.code.contains(".via(t, 2)"), "chain left as real calls:\n{}", out.code);
}

#[test]
fn invalid_stops_are_reported_and_not_fused() {
    let out = build("sink(curve(1).via(0.8, 1).via(0.2, 0))", true);
    assert_eq!(out.diagnostics.len(), 1, "{:?}", out.diagnostics);
    let d = &out.diagnostics[0];
    assert!(d.starts_with("/main.ts:1:"), "diagnostic should carry file:line:col — got {d}");
    assert!(d.contains("curve()") && d.contains("ascend"), "unexpected message: {d}");
    // Left as real calls: the runtime keeps behaving exactly as it did before the pass existed.
    assert!(!out.code.contains("new Float32Array(["), "invalid chain must not fuse:\n{}", out.code);
}

#[test]
fn t_outside_the_unit_range_and_overlong_curves_are_reported() {
    let range = build("sink(curve(1).via(1.5, 1).to(0))", true);
    assert!(range.diagnostics.iter().any(|d| d.contains("outside 0..1")), "{:?}", range.diagnostics);

    // 8 explicit stops + the implicit identity stop = 9, one more than native holds.
    let long = build("sink(curve(1).via(0.1,1).via(0.2,2).via(0.3,3).via(0.4,4).via(0.5,5).via(0.6,6).via(0.7,7).to(8))", true);
    assert!(long.diagnostics.iter().any(|d| d.contains("at most 8")), "{:?}", long.diagnostics);
}

#[test]
fn from_after_another_stop_is_reported() {
    // The builder throws at runtime; the compile-time report names the file and line instead.
    let out = build("sink(curve(1).to(0).from(1))", true);
    assert!(out.diagnostics.iter().any(|d| d.contains(".from() must come")), "{:?}", out.diagnostics);
    assert!(out.code.contains(".from(1)"), "the throwing call must survive:\n{}", out.code);
}

#[test]
fn an_unparsable_color_is_reported() {
    let out = build("sink(colorCurve('#nope!!').to('#000'))", true);
    assert!(out.diagnostics.iter().any(|d| d.contains("not a color")), "{:?}", out.diagnostics);
    assert!(out.code.contains("#nope!!"), "chain left alone:\n{}", out.code);
}

#[test]
fn validation_runs_without_fusion() {
    // `fuse: false` still validates — the rewrite is the opt-in part, not the diagnostics.
    let out = build("sink(curve(1).via(0.8, 1).via(0.2, 0))", false);
    assert!(out.diagnostics.iter().any(|d| d.contains("ascend")), "{:?}", out.diagnostics);
    assert!(!out.code.contains("new Float32Array(["), "no rewrite without fuse:\n{}", out.code);
    // A valid chain produces no noise.
    let ok = build("sink(curve(0.3).fade(0.15))", false);
    assert!(ok.diagnostics.is_empty(), "{:?}", ok.diagnostics);
    assert!(ok.code.contains("class CurveBuilder"), "builder kept without fuse:\n{}", ok.code);
}
