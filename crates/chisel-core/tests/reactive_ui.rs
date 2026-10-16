//! Tests for the reactive-UI desugaring pass (`Input.reactive_ui`): memoized children maps
//! (`.map` → `__uiMap` inside children bindings) and auto-wrapped signal-reading expressions.

use std::collections::HashMap;

use chisel_core::{bundle, Format, Input};

mod common;

/// A miniature SDK inject with just the globals the pass keys on.
const SDK: &str = r#"
export const UIColumn = (a: any, b?: any) => ({ a, b })
export const UIRow = (a: any, b?: any) => ({ a, b })
export const UIText = (a: any, b?: any) => ({ a, b })
export const signal = (v: any) => ({ value: v })
export const computed = (fn: any) => ({ get value() { return fn() } })
export const __uiMap = (list: any[], fn: any, slot: string) => list.map(fn)
"#;

fn run(main: &str, reactive_ui: bool) -> String {
    run_files(&[("/main.ts", main)], reactive_ui)
}

/// A project of several modules (`/main.ts` is the entry).
fn run_files(project: &[(&str, &str)], reactive_ui: bool) -> String {
    let mut files = HashMap::new();
    for (path, src) in project {
        files.insert(path.to_string(), src.to_string());
    }
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
        reactive_ui, flatten_ui: false, vocab: common::sdk_vocab(),
    });
    assert!(out.error.is_none(), "unexpected error: {:?}", out.error);
    out.code
}

/// Whitespace-insensitive containment (codegen formatting must not matter to these tests).
fn squish(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}
fn has(code: &str, needle: &str) -> bool {
    squish(code).contains(&squish(needle))
}

// ---- children maps ------------------------------------------------------------------------------

#[test]
fn map_inside_children_closure_is_memoized() {
    let code = run(
        "const items = signal([{ t: 'a' }])\nconst col = UIColumn(() => items.value.map(i => UIText(i.t)))\nconsole.log(col)",
        true,
    );
    assert!(has(&code, "__uiMap(items.value,"), "map not rewritten:\n{code}");
    assert!(code.contains("/main.ts:"), "slot id missing:\n{code}");
}

#[test]
fn map_in_style_first_children_closure_and_nested_maps() {
    let code = run(
        "const groups = signal([[1], [2]])\nconst col = UIColumn({ gap: 8 }, () => groups.value.map(g => UIRow(g.map(n => UIText('' + n)))))\nconsole.log(col)",
        true,
    );
    // both the outer and the nested map get their own slots
    let squished = squish(&code);
    assert_eq!(squished.matches("__uiMap(").count(), 2, "expected 2 rewrites:\n{code}");
}

#[test]
fn map_outside_children_closure_is_untouched() {
    let code = run(
        "const xs = [1, 2, 3].map(n => n * 2)\nconst col = UIColumn([UIText('' + xs[0])])\nconsole.log(col)",
        true,
    );
    assert!(!code.contains("__uiMap"), "top-level map must stay a plain map:\n{code}");
}

#[test]
fn set_content_closure_is_a_children_binding() {
    let code = run(
        "const rows = signal(['a'])\nconst col = UIColumn([])\ncol.a && (col as any).setContent(() => rows.value.map(r => UIText(r)))\nconsole.log(col)",
        true,
    );
    assert!(has(&code, "__uiMap(rows.value,"), "setContent closure map not rewritten:\n{code}");
}

// ---- auto-wrap ----------------------------------------------------------------------------------

#[test]
fn text_argument_reading_a_signal_is_wrapped() {
    let code = run(
        "const count = signal(0)\nconst label = UIText('Count: ' + count.value)\nconsole.log(label)",
        true,
    );
    assert!(has(&code, "UIText(()=>'Count: '+count.value)"), "text arg not wrapped:\n{code}");
}

#[test]
fn signal_alias_is_inferred_via_fixpoint() {
    let code = run(
        "const count = signal(0)\nconst alias = count\nconst label = UIText('' + alias.value)\nconsole.log(label)",
        true,
    );
    assert!(has(&code, "UIText(()=>"), "aliased signal read not wrapped:\n{code}");
}

#[test]
fn signal_chain_method_is_inferred() {
    // `signal(0).stored('count')` answers the signal (the vocabulary's `signals.chain`): the binding is a signal
    let code = run(
        "const count = signal(0).stored('count')
const label = UIText('Count: ' + count.value)
console.log(label)",
        true,
    );
    assert!(has(&code, "UIText(()=>'Count: '+count.value)"), "chained signal read not wrapped:
{code}");
}

#[test]
fn style_object_values_are_wrapped_per_property() {
    let code = run(
        "const on = signal(false)\nconst t = UIText({ opacity: on.value ? 1 : 0.5, color: 'red' }, 'x')\nt && (t as any).style({ fontSize: on.value ? 20 : 14 })\nconsole.log(t)",
        true,
    );
    assert!(has(&code, "opacity: () =>"), "factory style value not wrapped:\n{code}");
    assert!(has(&code, "fontSize: () =>"), ".style() value not wrapped:\n{code}");
    assert!(!has(&code, "color: () =>"), "static value must stay static:\n{code}");
}

#[test]
fn class_batch_values_are_wrapped_per_property() {
    // `.class({...})` (the SDK's style-class proxy batch form) takes boolean | () => boolean —
    // same wrap rule as style values.
    let code = run(
        "const on = signal(false)\nconst t = UIText({}, 'x')\nt && (t as any).class({ active: on.value, done: true })\nconsole.log(t)",
        true,
    );
    assert!(has(&code, "active: () =>"), ".class() value not wrapped:\n{code}");
    assert!(!has(&code, "done: () =>"), "static class value must stay static:\n{code}");
}

#[test]
fn already_wrapped_and_non_signal_expressions_are_untouched() {
    let code = run(
        "const count = signal(0)\nconst a = UIText(() => '' + count.value)\nconst b = UIText('static')\nconst n = 1\nconst c = UIText('' + n)\nconsole.log(a, b, c)",
        true,
    );
    let squished = squish(&code);
    assert!(!squished.contains("()=>()=>"), "double wrap:\n{code}");
    assert!(has(&code, "UIText('static')"), "static text changed:\n{code}");
    assert!(has(&code, "UIText('' + n)"), "non-signal expr changed:\n{code}");
}

#[test]
fn reads_inside_nested_handlers_do_not_wrap_the_outer_expression() {
    // the .value read lives inside an arrow already — the text arg itself is static
    let code = run(
        "const count = signal(0)\nconst fmt = () => '' + count.value\nconst label = UIText(fmt())\nconsole.log(label)",
        true,
    );
    assert!(has(&code, "UIText(fmt())"), "call without lexical .value read must not wrap:\n{code}");
}

// ---- gating -------------------------------------------------------------------------------------

#[test]
fn pass_is_off_by_default() {
    let code = run(
        "const items = signal([1])\nconst col = UIColumn(() => items.value.map(i => UIText('' + i)))\nconst label = UIText('' + items.value.length)\nconsole.log(col, label)",
        false,
    );
    assert!(!code.contains("__uiMap"), "pass ran while disabled:\n{code}");
    assert!(has(&code, "UIText('' + items.value.length)"), "wrap ran while disabled:\n{code}");
}

#[test]
fn sdk_internal_map_calls_are_inert() {
    // __uiMap itself contains `list.map(fn)` — the SDK module must never be self-rewritten
    let code = run(
        "const items = signal([1])\nconst col = UIColumn(() => items.value.map(i => UIText('' + i)))\nconsole.log(col)",
        true,
    );
    // exactly one rewrite (user code), and the SDK helper's own `.map` survives
    assert_eq!(squish(&code).matches("__uiMap(").count() >= 1, true);
    assert!(has(&code, "list.map(fn)"), "SDK-internal map was rewritten:\n{code}");
}

// ---- signals across modules ----------------------------------------------------------------------

#[test]
fn a_signal_imported_from_another_module_is_wrapped() {
    let code = run_files(
        &[
            ("/store.ts", "export const count = signal(0)\nexport const label = 'n'"),
            ("/main.ts", "import { count, label } from './store'\nconst t = UIText(`${label} ${count.value}`)\nconsole.log(t)"),
        ],
        true,
    );
    assert!(has(&code, "UIText(() => `${label} ${count.value}`)"), "imported signal not wrapped:\n{code}");
}

#[test]
fn a_signal_through_a_re_export_hub_and_an_alias_is_wrapped() {
    let code = run_files(
        &[
            ("/state/count.ts", "export const count = signal(0)"),
            ("/state/index.ts", "export { count as total } from './count'\nexport * from './count'"),
            ("/main.ts", "import { total } from './state'\nconst mine = total\nconst t = UIText(`${mine.value}`)\nconst s = UIText(`${total.value}`)\nconsole.log(t, s)"),
        ],
        true,
    );
    assert!(has(&code, "UIText(() => `${mine.value}`)"), "alias of a re-exported signal not wrapped:\n{code}");
    // the linker names the re-exported binding by its origin (`count`): both texts are wrapped
    assert_eq!(squish(&code).matches("UIText(()=>").count(), 2, "re-exported signal not wrapped:
{code}");
}

#[test]
fn an_exported_alias_of_an_imported_signal_reaches_a_third_module() {
    let code = run_files(
        &[
            ("/a.ts", "export const n = signal(1)"),
            ("/b.ts", "import { n } from './a'\nexport const m = n"),
            ("/main.ts", "import { m } from './b'\nconst t = UIText(`${m.value}`)\nconsole.log(t)"),
        ],
        true,
    );
    assert!(has(&code, "UIText(() => `${m.value}`)"), "signal through two modules not wrapped:\n{code}");
}

#[test]
fn an_imported_non_signal_is_left_alone() {
    let code = run_files(
        &[
            ("/store.ts", "export const user = { value: 'x' }"),
            ("/main.ts", "import { user } from './store'\nconst t = UIText(`${user.value}`)\nconsole.log(t)"),
        ],
        true,
    );
    assert!(!has(&code, "UIText(() =>"), "a plain object read was wrapped:\n{code}");
}
