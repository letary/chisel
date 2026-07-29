//! Tests for the UI-factory call optimization pass (`Input.flatten_ui`): literal array arguments
//! are spliced into variadic arguments, and calls whose arguments are all provably plain children
//! are raw-lowered to the SDK's dispatch-free `__UI*` builders. Anything the runtime dispatcher
//! could read differently is left alone.

use std::collections::HashMap;

use chisel_core::{bundle, Format, Input};

/// A miniature SDK inject with variadic factories + raw builders (the real SDK's shape).
const SDK: &str = r#"
export const UIColumn = (...args: any[]) => ({ args })
export const UIRow = (...args: any[]) => ({ args })
export const UIText = (t: any) => ({ t })
export const __UIColumn = (children: any) => ({ children })
export const __UIRow = (children: any) => ({ children })
export const signal = (v: any) => ({ value: v })
export const __uiMap = (list: any[], fn: any, slot: string) => list.map(fn)
"#;

fn run_full(main: &str, reactive_ui: bool, flatten_ui: bool) -> String {
    let mut files = HashMap::new();
    files.insert("/main.ts".to_string(), main.to_string());
    files.insert("/sdk/inject.ts".to_string(), SDK.to_string());
    let out = bundle(Input {
        files,
        entry: "/main.ts".into(), scan: false,
        inject: vec!["/sdk/inject.ts".into()],
        format: Format::Esm,
        minify: false,
        fuse: false,
        assets: Default::default(),
        define: Default::default(),
        sourcemap: false,
        keep: Default::default(),
        reactive_ui, flatten_ui,
    });
    assert!(out.error.is_none(), "unexpected error: {:?}", out.error);
    out.code
}

fn run(main: &str, flatten_ui: bool) -> String {
    run_full(main, false, flatten_ui)
}

/// Whitespace-insensitive containment (codegen formatting must not matter to these tests).
fn squish(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}
fn has(code: &str, needle: &str) -> bool {
    squish(code).contains(&squish(needle))
}

// ---- splicing + raw lowering --------------------------------------------------------------------

#[test]
fn lone_array_literal_is_spliced_and_raw_lowered() {
    let code = run("const c = UIColumn([UIText('a'), UIText('b')])\nconsole.log(c)", true);
    assert!(has(&code, "__UIColumn([UIText('a'), UIText('b')])"), "not lowered:\n{code}");
}

#[test]
fn array_between_nodes_is_spliced_and_raw_lowered() {
    let code = run(
        "const c = UIColumn(UIText('h'), [UIText('m')], UIText('f'))\nconsole.log(c)",
        true,
    );
    assert!(has(&code, "__UIColumn([UIText('h'), UIText('m'), UIText('f')])"), "not lowered:\n{code}");
}

#[test]
fn variadic_provable_args_raw_lower_without_any_array() {
    let code = run("const c = UIColumn(UIText('a'), UIText('b'))\nconsole.log(c)", true);
    assert!(has(&code, "__UIColumn([UIText('a'), UIText('b')])"), "not lowered:\n{code}");
}

#[test]
fn empty_lone_array_raw_lowers_to_empty_children() {
    let code = run("const c = UIColumn([])\nconsole.log(c)", true);
    assert!(has(&code, "__UIColumn([])"), "not lowered:\n{code}");
}

#[test]
fn nested_factory_calls_lower_recursively() {
    let code = run("const c = UIColumn([UIRow([UIText('a')])])\nconsole.log(c)", true);
    assert!(has(&code, "__UIColumn([__UIRow([UIText('a')])])"), "not lowered recursively:\n{code}");
}

#[test]
fn conditional_elements_are_provable_and_lowered() {
    // `cond && X` proves via the right side; `? :` needs both branches (falsy literals count).
    let code = run(
        "const cond = Math.random() > 0.5\nconst c = UIColumn([cond && UIText('a'), cond ? UIText('b') : null, UIText('c')])\nconsole.log(c)",
        true,
    );
    assert!(
        has(&code, "__UIColumn([cond && UIText('a'), cond ? UIText('b') : null, UIText('c')])"),
        "conditionals not lowered:\n{code}"
    );
}

#[test]
fn const_node_var_is_proven_via_fixpoint() {
    let code = run(
        "const h = UIText('h')\nconst wrap = h\nconst c = UIColumn(wrap, UIText('x'))\nconsole.log(c)",
        true,
    );
    assert!(has(&code, "__UIColumn([wrap, UIText('x')])"), "const chain not proven:\n{code}");
}

#[test]
fn chained_builder_methods_stay_provable() {
    let code = run(
        "const c = UIColumn(UIText('a').style({ color: 'red' }).onClick(() => {}))\nconsole.log(c)",
        true,
    );
    assert!(has(&code, "__UIColumn([UIText('a').style"), "chain not proven:\n{code}");
}

// ---- dispatch fallbacks (still spliced where safe, but no raw lowering) -------------------------

#[test]
fn legacy_style_plus_array_keeps_dispatch_but_splices() {
    let code = run("const c = UIRow({ gap: 4 }, [UIText('a')])\nconsole.log(c)", true);
    assert!(has(&code, "UIRow({ gap: 4 }, UIText('a'))"), "not spliced:\n{code}");
    assert!(!has(&code, "__UIRow"), "style call must not raw-lower:\n{code}");
}

#[test]
fn unknown_ident_falls_back_to_dispatch() {
    let code = run(
        "function render(x: any) { return UIColumn(x, UIText('a')) }\nconsole.log(render(1))",
        true,
    );
    assert!(has(&code, "UIColumn(x, UIText('a'))"), "unknown ident was lowered:\n{code}");
    assert!(!has(&code, "__UIColumn"), "unknown ident must not raw-lower:\n{code}");
}

#[test]
fn let_variable_is_not_proven() {
    let code = run(
        "let h = UIText('h')\nconst c = UIColumn(h, UIText('x'))\nconsole.log(c)",
        true,
    );
    assert!(!has(&code, "__UIColumn"), "let binding must not prove:\n{code}");
}

// ---- reactive form ------------------------------------------------------------------------------

#[test]
fn reactive_closure_is_raw_lowered() {
    let code = run("const c = UIColumn(() => [UIText('a')])\nconsole.log(c)", true);
    assert!(has(&code, "__UIColumn(() => [UIText('a')])"), "closure not lowered:\n{code}");
}

#[test]
fn reactive_closure_composes_with_reactive_ui_memoized_maps() {
    // reactive_ui runs first (memoizes `.map` inside the closure), then flatten_ui renames the
    // callee — both rewrites must land on the same call.
    let code = run_full(
        "const items = signal([{ t: 'a' }])\nconst c = UIColumn(() => items.value.map(i => UIText(i.t)))\nconsole.log(c)",
        true,
        true,
    );
    assert!(has(&code, "__UIColumn(() => __uiMap(items.value,"), "passes did not compose:\n{code}");
}

#[test]
fn function_behind_identifier_stays_on_dispatch() {
    let code = run(
        "const render = () => [UIText('a')]\nconst c = UIColumn(render)\nconsole.log(c)",
        true,
    );
    assert!(!has(&code, "__UIColumn"), "fn ident must not raw-lower:\n{code}");
    assert!(has(&code, "UIColumn(render)"), "call shape changed:\n{code}");
}

#[test]
fn style_plus_closure_stays_on_dispatch() {
    let code = run("const c = UIRow({ gap: 4 }, () => [UIText('a')])\nconsole.log(c)", true);
    assert!(!has(&code, "__UIRow"), "style+closure must not raw-lower:\n{code}");
}

// ---- guards -------------------------------------------------------------------------------------

#[test]
fn pass_is_off_by_default() {
    let code = run("const c = UIColumn([UIText('a')])\nconsole.log(c)", false);
    assert!(has(&code, "UIColumn(["), "spliced while off:\n{code}");
    assert!(!has(&code, "__UIColumn"), "lowered while off:\n{code}");
}

#[test]
fn spread_element_is_left_alone() {
    let code = run(
        "const rest: any[] = []\nconst c = UIColumn([UIText('a'), ...rest])\nconsole.log(c)",
        true,
    );
    assert!(has(&code, "UIColumn([UIText('a'), ...rest])"), "spread literal was touched:\n{code}");
}

#[test]
fn nested_array_literal_is_left_alone() {
    let code = run("const c = UIColumn([UIText('a'), [UIText('b')]])\nconsole.log(c)", true);
    assert!(!has(&code, "__UIColumn"), "nested-array literal was lowered:\n{code}");
    assert!(has(&code, "UIColumn(["), "nested-array literal was spliced:\n{code}");
}

#[test]
fn function_element_is_left_alone() {
    let code = run("const c = UIColumn([() => UIText('a')])\nconsole.log(c)", true);
    assert!(!has(&code, "__UIColumn"), "function element was lowered:\n{code}");
    assert!(has(&code, "UIColumn(["), "function element was spliced:\n{code}");
}

#[test]
fn leading_object_literal_in_first_arg_is_left_alone() {
    let code = run("const c = UIColumn([{ gap: 4 }, UIText('a')])\nconsole.log(c)", true);
    assert!(has(&code, "UIColumn(["), "would-be-style literal was spliced:\n{code}");
}

#[test]
fn empty_first_array_with_more_args_is_left_alone() {
    let code = run("const c = UIColumn([], { gap: 4 })\nconsole.log(c)", true);
    assert!(has(&code, "UIColumn(["), "empty first array was spliced:\n{code}");
}

#[test]
fn user_defined_factory_is_inert() {
    let code = run(
        "const UIColumn = (x: any) => x\nconst c = UIColumn([1, 2])\nconsole.log(c)",
        true,
    );
    assert!(!has(&code, "__UIColumn"), "shadowed factory was lowered:\n{code}");
}

#[test]
fn sdk_internal_calls_are_inert() {
    // Inside the SDK module the factories are locals, not free globals — never touched.
    let mut files = HashMap::new();
    files.insert("/main.ts".to_string(), "console.log(wrap())".to_string());
    files.insert(
        "/sdk/inject.ts".to_string(),
        format!("{SDK}\nexport const wrap = () => UIColumn([UIText('x')])\n"),
    );
    let out = bundle(Input {
        files,
        entry: "/main.ts".into(), scan: false,
        inject: vec!["/sdk/inject.ts".into()],
        format: Format::Esm,
        minify: false,
        fuse: false,
        assets: Default::default(),
        define: Default::default(),
        sourcemap: false,
        keep: Default::default(),
        reactive_ui: false, flatten_ui: true,
    });
    assert!(out.error.is_none(), "unexpected error: {:?}", out.error);
    assert!(has(&out.code, "UIColumn(["), "SDK-internal call was touched:\n{}", out.code);
}
