//! chisel-core — a method-granular bundler for TypeScript SDK-as-globals projects.
//!
//! Pipeline (built up across milestones): parse → graph → inject → resolve-types →
//! method-DCE → chain-fusion → link → emit. M0 wires parse → strip → codegen for a single
//! entry file so the toolchain is proven end to end.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use swc_core::common::{Globals, GLOBALS};

pub mod curve;
pub mod emit;
pub mod flatten_ui;
pub mod fusion;
pub mod graph;
pub mod link;
pub mod parse;
pub mod reactive_ui;
pub mod resolve;
pub mod scan;
pub mod tla;

/// Output module format.
#[derive(Debug, Clone, Copy, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    /// ES module (what the platform ships).
    #[default]
    Esm,
    /// Self-running script that reads `_creator*` host primitives as free globals (local renderer).
    Iife,
}

/// Bundler input: an in-memory file map + an entrypoint.
#[derive(Debug, Deserialize)]
pub struct Input {
    /// path → source. Paths are absolute-ish (`/main.ts`); relative imports resolve against them.
    pub files: HashMap<String, String>,
    /// The entry path (must exist in `files`). Required unless `scan` is set.
    #[serde(default)]
    pub entry: String,
    /// Scan mode: no bundling — parse every JS/TS module in `files` and return `Output.scan`
    /// (resolved local imports + hasExports per file). All other options are ignored.
    #[serde(default)]
    pub scan: bool,
    /// Inject entries (esbuild `inject:` semantics): every export becomes an ambient global that
    /// user code may reference without importing. Usually the SDK's `inject.ts`.
    #[serde(default)]
    pub inject: Vec<String>,
    #[serde(default)]
    pub format: Format,
    #[serde(default)]
    pub minify: bool,
    /// Enable math chain fusion (scalar replacement of `Vec3` chains). Off by default.
    #[serde(default)]
    pub fuse: bool,
    /// Asset path → URL map. `import x from './hero.png'` and `asset('./hero.png')` are resolved to
    /// the mapped URL (a compile-time string), mirroring the production loader. Unmapped paths fall
    /// back to the path itself (fine for local testing; the backend supplies real URLs).
    #[serde(default)]
    pub assets: HashMap<String, String>,
    /// Compile-time global substitutions (esbuild `define:` semantics). A free identifier matching a
    /// key is replaced by the value parsed as a numeric literal — e.g. `DEG2RAD` → `0.0174…`. Only
    /// numeric values are supported (all the SDK needs); non-numeric entries are ignored.
    #[serde(default)]
    pub define: HashMap<String, String>,
    /// Emit a source map (`Output.map`, standard JSON). `sources` are the original module paths; no
    /// `sourcesContent` is inlined (the editor already has the files). Fusion-synthesized nodes have
    /// no original span and map to their nearest real ancestor.
    #[serde(default)]
    pub sourcemap: bool,
    /// Instance-method names the host calls by name (so they have no in-bundle caller and would
    /// otherwise be tree-shaken). They're kept on any *reached* class that defines them. An entry
    /// ending in `*` is a prefix — e.g. `"_*"` keeps every underscore-prefixed method.
    #[serde(default)]
    pub keep: Vec<String>,
    /// Reactive-UI desugaring: memoize `.map` inside children bindings (`__uiMap`) and auto-wrap
    /// signal-reading text/style expressions in arrows. Keys on free references to the SDK's
    /// injected globals, so it only ever fires in user code. Off by default.
    #[serde(default)]
    pub reactive_ui: bool,
    /// Flatten literal array arguments of UI factory calls into variadic arguments
    /// (`UIColumn([a, b])` → `UIColumn(a, b)`) — the runtime flattens array arguments one level
    /// anyway, so this saves an array allocation per call site. Requires an SDK with variadic
    /// factory dispatch (`buildUI`). Off by default.
    #[serde(default)]
    pub flatten_ui: bool,
}

/// Bundler output. `error` is `Some` on a hard failure (and `code` is empty).
#[derive(Debug, Serialize, Default)]
pub struct Output {
    pub code: String,
    /// Source map JSON (present only when `Input.sourcemap` was set).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub map: Option<String>,
    pub diagnostics: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Scan-mode result (`Input.scan`): path → imports/hasExports per JS/TS module.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scan: Option<HashMap<String, scan::ScanModule>>,
}

/// Bundle a project. Never panics on user error — failures surface in `Output::error`.
pub fn bundle(input: Input) -> Output {
    if input.scan {
        return Output { scan: Some(scan::scan(&input.files)), ..Default::default() };
    }
    match bundle_inner(&input) {
        Ok((code, map, diagnostics)) => Output { code, map, diagnostics, ..Default::default() },
        Err(e) => Output { error: Some(format!("{e:#}")), ..Default::default() },
    }
}

/// `(code, source map, diagnostics)`. Diagnostics are non-fatal build-time findings (today: particle
/// curve chains that native would reject at set time) — `path:line:col: message`.
fn bundle_inner(input: &Input) -> anyhow::Result<(String, Option<String>, Vec<String>)> {
    if !input.files.contains_key(&input.entry) {
        anyhow::bail!("Entrypoint not found: {}", input.entry);
    }
    for ij in &input.inject {
        if !input.files.contains_key(ij) {
            anyhow::bail!("Inject entry not found: {ij}");
        }
    }

    let cm = parse::source_map();

    // The whole graph build + link runs inside one GLOBALS scope so every module's marks/contexts
    // are mutually consistent and `hygiene()` can finalize names across all of them.
    GLOBALS.set(&Globals::new(), || {
        let mut entries = vec![input.entry.clone()];
        entries.extend(input.inject.iter().cloned());

        let mut g = graph::build(&cm, &input.files, &entries, &input.assets, &input.define, input.reactive_ui, input.flatten_ui)?;
        let entry_id = g.path_to_id[&input.entry];
        let inject_ids: Vec<usize> = input.inject.iter().map(|p| g.path_to_id[p]).collect();

        let mut diagnostics = Vec::new();
        let merged = link::link(&mut g, entry_id, &inject_ids, input.fuse, &input.keep, &cm, &mut diagnostics)?;
        let (code, map) = emit::codegen(&cm, &merged, input.minify, input.sourcemap)?;
        // The IIFE wrapper must not add a leading line, or every source-map line would shift by one.
        let code = match input.format {
            Format::Esm => code,
            Format::Iife => format!("(()=>{{{code}}})();\n"),
        };
        Ok((code, map, diagnostics))
    })
}
