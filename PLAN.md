# PLAN — `chisel` status & roadmap

`chisel` is a Rust bundler for TypeScript projects that deliver an SDK of classes as injected globals.
It does two things general bundlers can't: **method-granular tree-shaking** (delete unused class
members) and **compile-time math chain fusion** (scalar-replace `Vec3` chains so they allocate
nothing). See [README.md](./README.md) for how it works.

---

## Built & validated

Pipeline (SWC, owned AST): `parse → graph (resolve + TS-strip + asset desugar + define + default-export
lowering) → unify identities → [fusion] → member-aware reachability DCE → concat (topo) → hygiene →
minify → emit (+ source map)`. ~23 cargo tests + a syntax robustness sweep (`scripts/robustness.mjs`).

- **Linker** — module graph, the import-free `inject` mechanism, scope-hoisting, hygiene, ESM/IIFE.
- **Static method-granular DCE** — drops unused static factories *and* their transitive deps; no
  manifest needed (members discovered structurally from the AST).
- **Instance-method DCE** — presence-gating; a numeric index `v[0]` and dynamic *reads* don't force
  keep-all (only a dynamic `obj[key](...)` *call* does).
- **User-code tree-shaking** — user modules get the same demand-driven DCE: unused pure decls / exports
  dropped, user classes method-DCE'd, while side-effecting top-level statements and `import './setup'`
  side-effect imports are kept. A conservative `expr_is_pure` check decides what's droppable.
- **Math chain fusion** — variable-rooted, `normalize`/`length`/`distanceTo` via hoisted temps,
  **bit-exact** over 800+ fuzzed values, cuts `Vec3` allocations 43–57%.
- **Minifier** — the real SWC minifier (compress + **top-level mangle**). The whole bundle is one closed
  scope (no exports), so every top-level name is private and manglable; host globals stay untouched
  (resolver marks them `unresolved`). Bit-exact with fusion (no float reassociation).
- **`define`** — esbuild `define:` semantics: a free identifier matching a key → a numeric literal. A
  *local* binding of the same name is left alone (only `unresolved` refs are substituted).
- **Default export/import** — `export default <expr>` lowers to a named const (the value can be a
  runtime expression); `import x from './m'` binds to it. Asset specifiers with no backing module fall
  back to `Input.assets` (path → URL).
- **Source maps** — opt-in v3 map (`Output.map`); `sources` are the original module paths, no
  `sourcesContent`. Fusion-synthesized nodes have no original span and map to their nearest ancestor.

---

## Roadmap / ideas

- **Type-resolution layer** — a conservative local type inference (seed from `new T()`, known static
  return types, known property types like `node.position: Vec3`) would sharpen instance-DCE on
  factory-rooted cases *and* enable fusion on getter-rooted chains (`self.position.add(...)`) without
  re-calling the getter.
- **Fusion beyond `Vec3`** — extend scalar replacement to `Vec2` / `Quat` / `Mat4`.
- **Source-map precision** — thread original spans through the fusion and minify rewrites so
  synthesized nodes map better (currently they degrade to the nearest real ancestor).
- **In-process embedding** — a napi-rs (or WASM) binding to skip the subprocess + per-call file
  re-read when embedding chisel in a Node/Bun toolchain.
- **Robustness** — keep widening `scripts/robustness.mjs`; each "chisel choked on X" is a contained fix.

## Non-goals
node_modules / npm resolution · code-splitting / dynamic `import()` · CSS loaders · watch mode ·
competing with esbuild on cold-start. chisel stays a purpose-built bundler for the injected-globals SDK
shape; its correctness budget is spent on method-DCE, fusion, and matching esbuild's size everywhere else.
