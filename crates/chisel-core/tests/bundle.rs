//! Integration tests for the chisel bundler. Grows with each milestone.

use std::collections::HashMap;

use chisel_core::{bundle, Format, Input};

/// Bundle a set of files. `entry` defaults to `/main.ts`.
fn run(files: &[(&str, &str)], minify: bool) -> chisel_core::Output {
    let files = files.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect::<HashMap<_, _>>();
    bundle(Input { files, entry: "/main.ts".into(), inject: vec![], format: Format::Esm, minify, fuse: false, assets: Default::default(), define: Default::default(), sourcemap: false, keep: Default::default() })
}

fn ok(files: &[(&str, &str)]) -> String {
    let out = run(files, false);
    assert!(out.error.is_none(), "unexpected error: {:?}", out.error);
    out.code
}

// ---- M0: parse → strip → codegen --------------------------------------------------------------

#[test]
fn m0_strips_types() {
    let code = ok(&[(
        "/main.ts",
        "const x: number = 1\ninterface I { a: string }\ntype T = I\nfunction f(p: I): number { return p.a.length }\nconsole.log(x, f({ a: 'hi' }))",
    )]);
    assert!(!code.contains("interface"), "interface not stripped:\n{code}");
    assert!(!code.contains(": number"), "type annotation not stripped:\n{code}");
    assert!(!code.contains("type T"), "type alias not stripped:\n{code}");
    assert!(code.contains("console.log"), "runtime code missing:\n{code}");
}

#[test]
fn m0_minify() {
    let out = run(&[("/main.ts", "const a: number = 1 + 2\nconsole.log(a)")], true);
    assert!(out.error.is_none());
    assert!(!out.code.contains('\n'), "minified output should be one line:\n{}", out.code);
}

// ---- M1: linking, injection, whole-decl DCE ---------------------------------------------------

fn run_inject(files: &[(&str, &str)], inject: &[&str]) -> chisel_core::Output {
    let files = files.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect::<HashMap<_, _>>();
    bundle(Input {
        files,
        entry: "/main.ts".into(),
        inject: inject.iter().map(|s| s.to_string()).collect(),
        format: Format::Esm,
        minify: false,
        fuse: false,
        assets: Default::default(),
        define: Default::default(),
        sourcemap: false,
        keep: Default::default(),
    })
}

#[test]
fn m1_links_relative_imports() {
    let code = ok(&[
        ("/main.ts", "import { greet, CONST } from './util'\nconsole.log(greet('world'), CONST)"),
        ("/util.ts", "export const CONST = 42\nexport function greet(name: string): string { return 'hi ' + name }"),
    ]);
    assert!(!code.contains("import"), "imports should be linked away:\n{code}");
    assert!(!code.contains("export"), "exports should be unwrapped:\n{code}");
    assert!(code.contains("function greet"), "linked fn missing:\n{code}");
    assert!(code.contains("console.log"), "entry side effect missing:\n{code}");
}

#[test]
fn m1_injects_sdk_and_dce_unused_decls() {
    let out = run_inject(
        &[
            ("/main.ts", "const v = new Vec(1).add(new Vec(2))\nconsole.log(v.x)"),
            ("/sdk/inject.ts", "export { Vec } from './vec'\nexport { Other } from './other'"),
            ("/sdk/vec.ts", "export class Vec { x: number\n  constructor(x: number) { this.x = x }\n  add(v: Vec): Vec { return new Vec(this.x + v.x) } }"),
            ("/sdk/other.ts", "export class Other { hello(): string { return 'unused' } }"),
        ],
        &["/sdk/inject.ts"],
    );
    assert!(out.error.is_none(), "unexpected error: {:?}", out.error);
    let code = out.code;
    assert!(code.contains("class Vec"), "injected Vec should be linked:\n{code}");
    assert!(!code.contains("class Other"), "unused SDK class should be DCE'd:\n{code}");
    assert!(!code.contains("import"), "no imports should remain:\n{code}");
    assert!(code.contains("new Vec("), "entry should construct Vec:\n{code}");
}

// ---- M2: method-granular DCE (statics) + transitive drop --------------------------------------

#[test]
fn m2_static_method_dce_is_transitive() {
    let out = run_inject(
        &[
            ("/main.ts", "const m = Mesh.box()\nconsole.log(m.g)"),
            ("/sdk/inject.ts", "export { Mesh } from './mesh'\nexport { Geometry } from './geometry'"),
            (
                "/sdk/mesh.ts",
                "import { Geometry } from './geometry'\nimport { fetchThing } from './net'\nexport class Mesh {\n  g: number\n  constructor(g: number) { this.g = g }\n  static box(): Mesh { return new Mesh(Geometry.box()) }\n  static sphere(): Mesh { return new Mesh(Geometry.sphere()) }\n  static load(): Mesh { return new Mesh(fetchThing()) }\n}",
            ),
            (
                "/sdk/geometry.ts",
                "export class Geometry {\n  static box(): number { return 8 }\n  static sphere(): number { return 64 }\n  static cylinder(): number { return 32 }\n}",
            ),
            ("/sdk/net.ts", "export function fetchThing(): number { return 999 }"),
        ],
        &["/sdk/inject.ts"],
    );
    assert!(out.error.is_none(), "error: {:?}", out.error);
    let c = out.code;
    assert!(c.contains("static box"), "Mesh.box must survive:\n{c}");
    assert!(!c.contains("static sphere"), "unused Mesh.sphere/Geometry.sphere must be dropped:\n{c}");
    assert!(!c.contains("static load"), "unused Mesh.load must be dropped:\n{c}");
    assert!(!c.contains("static cylinder"), "unused Geometry.cylinder must be dropped:\n{c}");
    // The transitive win: fetchThing is only reachable via the (dropped) Mesh.load.
    assert!(!c.contains("fetchThing"), "fetchThing reachable only via dropped Mesh.load must vanish:\n{c}");
}

#[test]
fn m2_escaping_class_keeps_all_statics() {
    // If a class is referenced as a bare value, we can't prove a static is unused → keep them all.
    let out = run_inject(
        &[
            ("/main.ts", "const ctor = Mesh\nconsole.log(ctor.box(), ctor.sphere())"),
            ("/sdk/inject.ts", "export { Mesh } from './mesh'"),
            ("/sdk/mesh.ts", "export class Mesh {\n  static box(): number { return 1 }\n  static sphere(): number { return 2 }\n}"),
        ],
        &["/sdk/inject.ts"],
    );
    assert!(out.error.is_none(), "error: {:?}", out.error);
    assert!(out.code.contains("static box") && out.code.contains("static sphere"), "escape must keep all statics:\n{}", out.code);
}

// ---- M3: instance-method DCE (presence-gating) ------------------------------------------------

#[test]
fn m3_instance_method_dce() {
    let out = run_inject(
        &[
            ("/main.ts", "const a = new Vec(3,4).add(new Vec(1,1))\nconsole.log(a.length())"),
            ("/sdk/inject.ts", "export { Vec } from './vec'"),
            (
                "/sdk/vec.ts",
                "export class Vec {\n  constructor(public x: number, public y: number) {}\n  add(v: Vec): Vec { return new Vec(this.x+v.x, this.y+v.y) }\n  length(): number { return Math.hypot(this.x, this.y) }\n  reflect(n: Vec): Vec { return new Vec(this.x, this.y) }\n  unusedDead(): number { return 1 }\n}",
            ),
        ],
        &["/sdk/inject.ts"],
    );
    assert!(out.error.is_none(), "error: {:?}", out.error);
    let c = out.code;
    assert!(c.contains("add(v)") && c.contains("length()"), "used instance methods must survive:\n{c}");
    assert!(!c.contains("reflect"), "unused reflect must be dropped:\n{c}");
    assert!(!c.contains("unusedDead"), "unused method must be dropped:\n{c}");
}

#[test]
fn m3_numeric_index_does_not_keep_all_methods() {
    // `v[0]` is array indexing, not a dynamic method dispatch — it must NOT force keeping every method.
    let out = run_inject(
        &[
            ("/main.ts", "const r = read([5,6,7]); console.log(r)"),
            ("/sdk/inject.ts", "export { read } from './u'\nexport { Thing } from './u'"),
            (
                "/sdk/u.ts",
                "export function read(a: number[]): number { return a[0] + a[1] }\nexport class Thing {\n  used(): number { return 1 }\n  dead(): number { return 2 }\n}",
            ),
        ],
        &["/sdk/inject.ts"],
    );
    assert!(out.error.is_none(), "error: {:?}", out.error);
    // Thing isn't referenced at all → whole class gone; the point is `read`'s `a[0]` didn't blow up DCE.
    assert!(!out.code.contains("class Thing"), "unreferenced class should be gone:\n{}", out.code);
    assert!(out.code.contains("function read"), "read must survive:\n{}", out.code);
}

// ---- M4: math chain fusion --------------------------------------------------------------------

#[test]
fn m4_fusion_inlines_chain_and_dces_methods() {
    let files = [
        ("/main.ts", "const r = new Vec3(1,2,3).add([10,0,0]).scale(2).dot([1,0,0])\nconsole.log(r)"),
        ("/sdk/inject.ts", "export { Vec3 } from './vec'"),
        (
            "/sdk/vec.ts",
            "export class Vec3 {\n  constructor(public x: number, public y: number, public z: number) {}\n  add(v: any): Vec3 { return new Vec3(this.x+v[0], this.y+v[1], this.z+v[2]) }\n  scale(s: number): Vec3 { return new Vec3(this.x*s, this.y*s, this.z*s) }\n  dot(v: any): number { return this.x*v[0]+this.y*v[1]+this.z*v[2] }\n}",
        ),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect::<HashMap<_, _>>();
    let out = bundle(Input { files, entry: "/main.ts".into(), inject: vec!["/sdk/inject.ts".into()], format: Format::Esm, minify: false, fuse: true, assets: Default::default(), define: Default::default(), sourcemap: false, keep: Default::default() });
    assert!(out.error.is_none(), "error: {:?}", out.error);
    let c = out.code;
    // The whole chain is a scalar terminal → fully inlined, no method calls, no Vec3 allocation.
    assert!(!c.contains(".add("), "add call should be fused away:\n{c}");
    assert!(!c.contains(".dot("), "dot call should be fused away:\n{c}");
    assert!(!c.contains("new Vec3"), "no Vec3 should be allocated:\n{c}");
    assert!(!c.contains("class Vec3"), "Vec3 class should be DCE'd after fusion:\n{c}");
    assert!(c.contains("console.log"), "result still logged:\n{c}");
}

#[test]
fn m4_fusion_variable_rooted_chain() {
    // Chains rooted at typed local variables fuse (the per-frame pattern), not just literals.
    let files = [
        ("/main.ts", "const a = new Vec3(1,2,3)\nconst b = new Vec3(4,5,6)\nconst r = a.add(b).scale(2).dot(b)\nconsole.log(r)"),
        ("/sdk/inject.ts", "export { Vec3 } from './vec'"),
        (
            "/sdk/vec.ts",
            "export class Vec3 {\n  constructor(public x: number, public y: number, public z: number) {}\n  add(v: any): Vec3 { return new Vec3(this.x+v.x, this.y+v.y, this.z+v.z) }\n  scale(s: number): Vec3 { return new Vec3(this.x*s, this.y*s, this.z*s) }\n  dot(v: any): number { return this.x*v.x+this.y*v.y+this.z*v.z }\n}",
        ),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect::<HashMap<_, _>>();
    let out = bundle(Input { files, entry: "/main.ts".into(), inject: vec!["/sdk/inject.ts".into()], format: Format::Esm, minify: false, fuse: true, assets: Default::default(), define: Default::default(), sourcemap: false, keep: Default::default() });
    assert!(out.error.is_none(), "error: {:?}", out.error);
    let c = out.code;
    assert!(!c.contains(".add(") && !c.contains(".scale(") && !c.contains(".dot("), "chain on typed vars should fully fuse:\n{c}");
    // `a` and `b` survive (their fields are read) but their methods are DCE'd after fusion.
    assert!(c.contains("new Vec3(1, 2, 3)"), "roots kept:\n{c}");
}

#[test]
fn m4_fusion_normalize_hoists_hypot_once() {
    let files = [
        ("/main.ts", "const v = new Vec3(3,4,0).normalize()\nconsole.log(v.x, v.y)"),
        ("/sdk/inject.ts", "export { Vec3 } from './vec'"),
        (
            "/sdk/vec.ts",
            "export class Vec3 {\n  constructor(public x: number, public y: number, public z: number) {}\n  normalize(): Vec3 { const l = Math.hypot(this.x,this.y,this.z); return l===0 ? new Vec3(0,0,0) : new Vec3(this.x/l, this.y/l, this.z/l) }\n}",
        ),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect::<HashMap<_, _>>();
    let out = bundle(Input { files, entry: "/main.ts".into(), inject: vec!["/sdk/inject.ts".into()], format: Format::Esm, minify: false, fuse: true, assets: Default::default(), define: Default::default(), sourcemap: false, keep: Default::default() });
    assert!(out.error.is_none(), "error: {:?}", out.error);
    let c = out.code;
    assert!(!c.contains(".normalize("), "normalize call should be fused:\n{c}");
    assert_eq!(c.matches("Math.hypot(").count(), 1, "hypot must be computed exactly once (hoisted temp):\n{c}");
    assert!(c.contains("__chisel_"), "a temp should be hoisted:\n{c}");
}

// ---- M5: user-code tree-shaking ---------------------------------------------------------------

#[test]
fn m5_drops_unused_user_decls_and_exports() {
    // A user module (`lib`) imported by the entry: only the referenced export (and what it pulls)
    // survives; unused exports and unused locals are dropped — the case esbuild wins on today.
    let code = ok(&[
        ("/main.ts", "import { used } from './lib'\nconsole.log(used())"),
        (
            "/lib.ts",
            "export function used(): number { return helper() }\nfunction helper(): number { return 1 }\nexport const unusedConst = { a: 1, b: 2 }\nexport function unusedFn(): number { return 99 }\nconst deadLocal = 5",
        ),
    ]);
    assert!(code.contains("function used"), "used export must survive:\n{code}");
    assert!(code.contains("function helper"), "transitively-used helper must survive:\n{code}");
    assert!(!code.contains("unusedConst"), "unused export const must be dropped:\n{code}");
    assert!(!code.contains("unusedFn"), "unused export fn must be dropped:\n{code}");
    assert!(!code.contains("deadLocal"), "unused local must be dropped:\n{code}");
}

#[test]
fn m5_keeps_side_effecting_user_statements() {
    // A side-effecting top-level statement (and an impure-initialized const) must always be kept,
    // even when its binding is never referenced.
    let code = ok(&[
        ("/main.ts", "import { api } from './lib'\nconsole.log(api)"),
        (
            "/lib.ts",
            "export const api = 7\nconst eager = init()\nfunction init(): number { return 42 }\nconst lazyUnused = 5\nconsole.log('lib loaded')",
        ),
    ]);
    assert!(code.contains("api"), "used export kept:\n{code}");
    assert!(code.contains("eager") && code.contains("function init"), "impure-init const + its dep kept:\n{code}");
    assert!(code.contains("lib loaded"), "top-level side effect kept:\n{code}");
    assert!(!code.contains("lazyUnused"), "pure unused const dropped:\n{code}");
}

#[test]
fn m5_user_class_method_dce() {
    // User classes get the same instance-method DCE as the SDK.
    let code = ok(&[
        ("/main.ts", "import { Player } from './player'\nconst p = new Player()\np.move()\nconsole.log(p)"),
        ("/player.ts", "export class Player {\n  move(): void {}\n  unusedSkill(): void {}\n}"),
    ]);
    assert!(code.contains("class Player") && code.contains("move()"), "used user method kept:\n{code}");
    assert!(!code.contains("unusedSkill"), "unused user method must be DCE'd:\n{code}");
}

#[test]
fn m5_side_effect_only_import_is_kept() {
    // `import './setup'` (no bindings) must keep running setup's top-level side effects.
    let code = ok(&[
        ("/main.ts", "import './setup'\nconsole.log('main')"),
        ("/setup.ts", "register()\nfunction register(): void { console.log('registered') }"),
    ]);
    assert!(code.contains("registered"), "side-effect-only import's effects must be kept:\n{code}");
    assert!(code.contains("function register"), "the called helper must be kept:\n{code}");
}

// ---- M6: production parity — define + default export/import -----------------------------------

#[test]
fn m6_define_substitutes_free_global() {
    // `DEG2RAD` is a host global esbuild replaces via `define:`; chisel must do the same.
    let files = [("/main.ts", "const a = 90 * DEG2RAD\nconsole.log(a)")]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect::<HashMap<_, _>>();
    let define = [("DEG2RAD".to_string(), "0.017453292519943295".to_string())].into_iter().collect();
    let out = bundle(Input { files, entry: "/main.ts".into(), inject: vec![], format: Format::Esm, minify: false, fuse: false, assets: Default::default(), define, sourcemap: false, keep: Default::default() });
    assert!(out.error.is_none(), "error: {:?}", out.error);
    assert!(out.code.contains("0.017453292519943295"), "DEG2RAD must be substituted:\n{}", out.code);
    assert!(!out.code.contains("DEG2RAD"), "no free DEG2RAD should remain:\n{}", out.code);
}

#[test]
fn m6_define_leaves_local_const_alone() {
    // A *local* `DEG2RAD` (the SDK's quat.ts pattern) must not be substituted.
    let files = [("/main.ts", "const DEG2RAD = Math.PI / 180\nconsole.log(DEG2RAD)")]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect::<HashMap<_, _>>();
    let define = [("DEG2RAD".to_string(), "0.017453292519943295".to_string())].into_iter().collect();
    let out = bundle(Input { files, entry: "/main.ts".into(), inject: vec![], format: Format::Esm, minify: false, fuse: false, assets: Default::default(), define, sourcemap: false, keep: Default::default() });
    assert!(out.error.is_none(), "error: {:?}", out.error);
    assert!(out.code.contains("Math.PI / 180"), "local const must survive untouched:\n{}", out.code);
}

#[test]
fn m6_default_export_import_resolves_url() {
    // The production asset shape: an asset is a real `export default "<url>"` module, imported by
    // name. chisel must resolve the default import to that module's default binding.
    let code = ok(&[
        ("/main.ts", "import logo from './logo.png'\nconsole.log(logo)"),
        ("/logo.png", "export default \"https://cdn.example/logo.png\""),
    ]);
    assert!(code.contains("https://cdn.example/logo.png"), "default export URL must be linked in:\n{code}");
    assert!(!code.contains("import"), "default import must be linked away:\n{code}");
    assert!(!code.contains("export default"), "export default must be lowered:\n{code}");
}

#[test]
fn m6_default_export_keeps_runtime_expr() {
    // Shaders are `export default \`…${_creator.backend}…\`` — a runtime template, not a static URL.
    let code = ok(&[
        ("/main.ts", "import shader from './s.wgsl'\nconst m = { shader }\nconsole.log(m)"),
        ("/s.wgsl", "export default `/shaders/x_${_creator.backend}.wgsl`"),
    ]);
    assert!(code.contains("_creator.backend"), "runtime expr in default export must be preserved:\n{code}");
    assert!(code.contains("/shaders/x_"), "template body kept:\n{code}");
}

// ---- M7: codegen correctness — the fixer always runs --------------------------------------------

#[test]
fn m7_namespace_iife_is_parenthesized() {
    // A TS namespace lowers to an IIFE `(function(N){…})(N||(N={}))`. Without the fixer the readable
    // (non-minified) output prints it as a *nameless function declaration* → invalid JS. Regression.
    let code = ok(&[(
        "/main.ts",
        "namespace N { export const x = 5; export function f() { return x * 2 } }\nconsole.log(N.f())",
    )]);
    assert!(code.contains("(function"), "namespace IIFE must be parenthesized:\n{code}");
    assert!(!code.contains("\nfunction("), "no bare nameless function declaration at statement start:\n{code}");
}

// ---- M8: source maps ----------------------------------------------------------------------------

#[test]
fn m8_source_map_emitted_with_original_sources() {
    let files = [
        ("/main.ts", "import { greet } from './util'\nconsole.log(greet('x'))"),
        ("/util.ts", "export function greet(n: string){ return 'hi ' + n }"),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect::<HashMap<_, _>>();
    let out = bundle(Input { files, entry: "/main.ts".into(), inject: vec![], format: Format::Esm, minify: false, fuse: false, assets: Default::default(), define: Default::default(), sourcemap: true, keep: Default::default() });
    assert!(out.error.is_none(), "error: {:?}", out.error);
    let map = out.map.expect("source map should be present when requested");
    assert!(map.contains("\"version\":3") || map.contains("\"version\": 3"), "v3 source map:\n{map}");
    assert!(map.contains("/util.ts") && map.contains("/main.ts"), "original module paths in sources:\n{map}");
    assert!(!map.contains("\"mappings\":\"\"") && !map.contains("\"mappings\": \"\""), "mappings must be non-empty:\n{map}");
}

#[test]
fn m8_no_source_map_unless_requested() {
    let out = run(&[("/main.ts", "console.log(1)")], false);
    assert!(out.map.is_none(), "no map should be emitted when sourcemap is false");
}

// ---- M9: keep (host-called methods) -------------------------------------------------------------

#[test]
fn m9_keep_retains_host_called_methods() {
    // `_emitClick` has no in-bundle caller (the host invokes it) → normally dropped. `keep: ["_*"]`
    // retains every underscore method on *reached* classes; a non-underscore unused method is still
    // dropped, and an entirely unused class is not resurrected.
    let files = [
        ("/main.ts", "const b = new UIButton()\nb.render()\nconsole.log(b)"),
        ("/sdk/inject.ts", "export { UIButton } from './ui'\nexport { Unused } from './ui'"),
        (
            "/sdk/ui.ts",
            "export class UIButton {\n  render(): number { return 1 }\n  _emitClick(): number { return 2 }\n  _dead(): number { return 3 }\n  unusedPublic(): number { return 4 }\n}\nexport class Unused {\n  _emitClick(): number { return 9 }\n}",
        ),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect::<HashMap<_, _>>();
    let out = bundle(Input {
        files,
        entry: "/main.ts".into(),
        inject: vec!["/sdk/inject.ts".into()],
        format: Format::Esm,
        minify: false,
        fuse: false,
        assets: Default::default(),
        define: Default::default(),
        sourcemap: false,
        keep: vec!["_*".into()],
    });
    assert!(out.error.is_none(), "error: {:?}", out.error);
    let c = out.code;
    assert!(c.contains("render()"), "used method kept:\n{c}");
    assert!(c.contains("_emitClick"), "host-called _emitClick must be kept by _*:\n{c}");
    assert!(c.contains("_dead"), "_* keeps every underscore method:\n{c}");
    assert!(!c.contains("unusedPublic"), "non-underscore unused method still DCE'd:\n{c}");
    assert!(!c.contains("class Unused"), "keep does not resurrect an unreached class:\n{c}");
}

#[test]
fn m9_keep_exact_name() {
    let files = [
        ("/main.ts", "const b = new W()\nb.use()\nconsole.log(b)"),
        ("/sdk/inject.ts", "export { W } from './w'"),
        ("/sdk/w.ts", "export class W {\n  use(): number { return 1 }\n  _emitTouchStart(): number { return 2 }\n  alsoDead(): number { return 3 }\n}"),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect::<HashMap<_, _>>();
    let out = bundle(Input {
        files,
        entry: "/main.ts".into(),
        inject: vec!["/sdk/inject.ts".into()],
        format: Format::Esm,
        minify: false,
        fuse: false,
        assets: Default::default(),
        define: Default::default(),
        sourcemap: false,
        keep: vec!["_emitTouchStart".into()],
    });
    assert!(out.error.is_none(), "error: {:?}", out.error);
    assert!(out.code.contains("_emitTouchStart"), "exact-named keep method survives:\n{}", out.code);
    assert!(!out.code.contains("alsoDead"), "other unused method still dropped:\n{}", out.code);
}

#[test]
fn m0_missing_entry_errors() {
    let files = [("/a.ts", "export const x = 1")].iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    let out = bundle(Input { files, entry: "/main.ts".into(), inject: vec![], format: Format::Esm, minify: false, fuse: false, assets: Default::default(), define: Default::default(), sourcemap: false, keep: Default::default() });
    assert!(out.error.as_deref().unwrap_or_default().contains("Entrypoint not found"));
}
