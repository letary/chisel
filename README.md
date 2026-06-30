# chisel

A small, purpose-built bundler in **Rust** (built on [SWC](https://swc.rs)) for TypeScript projects
that deliver an "SDK" of classes as injected globals (the esbuild `inject:` pattern). It does two
things general bundlers (esbuild, Rollup, Rolldown) structurally can't:

- **Method-granular tree-shaking** — drop *unused class methods* (static and instance), not just
  unused modules. To a general bundler a class is a single binding: use one static factory and you
  keep every method. chisel discovers classes/members structurally from the AST and drops the long
  tail of unused methods (and everything only they pulled in).
- **Math chain fusion** — lower a `Vec3`-style method chain like `a.add(b).scale(s).dot(c)` to scalar
  arithmetic at compile time, so no intermediate objects are allocated.

Plus the usual: scope-hoisting linker, an import-free `inject` mechanism, the real SWC minifier
(compress + mangle), source maps, ESM/IIFE output. Status: **working prototype**, see [PLAN.md](./PLAN.md).

## Results

Against a representative TypeScript 3D/2D engine SDK (classes delivered as injected globals), minified
bytes vs esbuild for typical entry points:

| entry | esbuild | chisel | delta |
|---|--:|--:|--:|
| `Mat4.perspective(...)` | 15.4 KB | **0.7 KB** | **−96%** |
| `new Vec3().add(...)` | 5.3 KB | **0.4 KB** | **−93%** |
| `Quat.fromEuler(...)` | 9.7 KB | **1.2 KB** | **−88%** |
| `Mesh.box({size:1})` | 35.0 KB | **11.7 KB** | **−67%** |
| `Sprite` (tiny 2D) | 12.8 KB | **4.6 KB** | **−64%** |

The wins come from dropping the unused instance methods (`reflect`, `slerp`, `rotateX/Y/Z`, the unused
half of a `Node`/`Mesh`, …) that esbuild must keep. The key insight that makes this safe even with
dynamic-looking code: a dynamic `store[key]` *read* (e.g. a uniform `Proxy`) can't dispatch a method,
so it doesn't force keep-all — only a dynamic `obj[key](...)` *call* does, and a numeric index `v[0]`
is array access, not dispatch.

### Math chain fusion (opt-in: `"fuse": true`)

`a.add(b).scale(s).normalize().dot(c)` lowers to scalar arithmetic — no intermediate `Vec3` allocated;
a scalar terminal like `.dot()`/`.length()` allocates nothing at all. Works for chains rooted at
`new Vec3()`, array literals, **and typed local variables** (a fixpoint infers which locals hold a
`Vec3`, so per-frame code like `dir.scale(speed*dt)` fuses). Reused values — a scalar applied 3×,
`cross` components, the length in `normalize` — are **hoisted into a `const` temp** so nothing is
recomputed (`Math.hypot` runs once); control-flow bodies are block-ified and arrow expression-bodies
become blocks so the temp always lands in the right scope.

Validated **bit-exact** against the un-fused output over 800+ randomized values — variable-rooted
per-frame blocks, `normalize`/`length`/`distanceTo`, inside loops and arrow callbacks — since JS
numbers are all f64 and the lowering never reassociates. On 100k-iteration loops it cuts `Vec3`
allocations 43–57% with identical results. Fusion runs before DCE, so the inlined methods then
disappear from the bundle too.

## Use

Build from source, or install the prebuilt binary from npm (resolves your platform automatically via
optional `@letary/chisel-<platform>` packages — macOS/Linux/Windows, x64/arm64):

```sh
npm i @letary/chisel        # then: require("@letary/chisel").binaryPath()
# — or build from source —
cargo build --release
# JSON over stdio:
echo '{"files":{"/main.ts":"..."},"entry":"/main.ts","inject":["/inject.ts"],"minify":true}' \
  | ./target/release/chisel
```

Input: `{ files: {path→source}, entry, inject: [paths], format: "esm"|"iife", minify, fuse, define,
assets, sourcemap, keep }`. Output: `{ code, map?, diagnostics, error? }`.

- `inject` — every export of these entries becomes an ambient global (esbuild `inject:` semantics).
- `define` — compile-time global substitutions (numeric, e.g. `DEG2RAD` → `0.0174…`).
- `assets` — `path → URL` map for `asset('./x')` / `import x from './x.png'`.
- `sourcemap` — emit a v3 map in `Output.map` (`sources` are the original module paths).
- `keep` — instance-method names the **host calls by name** (no in-bundle caller, so DCE can't see
  them). Kept on any *reached* class that defines them. An entry ending in `*` is a prefix, so
  `["_*"]` keeps every underscore-prefixed method.

Helpers: `node scripts/run.mjs <file|dir>` runs a quick bundle; `node scripts/robustness.mjs` runs a
broad JS/TS syntax corpus through the binary and checks each survives.

## How it works

```
parse (SWC, owned AST) → module graph (resolve + TS-strip) → Phase A: unify identities
  → [fusion] → Phase B: member-aware reachability DCE → Phase C: concat in dep order
  → Phase D: hygiene → [minify: SWC compress + top-level mangle] → emit (+ source map)
```

- **Identity unification** rewrites each `import`-local and injected global to its origin binding's
  `(name, SyntaxContext)`, so all references to a logical binding share one identity.
- **Member-aware reachability** splits each class into a *core* unit, one unit per *static method*, and
  one per *instance method*. Statics are pulled by `Class.name`; instance methods by name presence
  (`.name(...)` anywhere reachable; a dynamic `obj[expr]` call keeps all — a numeric index `v[0]` does
  not). The same pass shakes user modules: unused pure decls / exports are dropped and user classes are
  method-DCE'd, while side-effecting top-level statements and `import './setup'` side-effect imports are
  kept; injected SDK modules are pulled purely on demand.
- **Minify** runs the real SWC minifier (compress + top-level mangle). The fully-linked bundle is one
  closed scope with no exports, so every top-level binding is private and renameable; free identifiers
  are exactly the host globals, which the resolver marks `unresolved` so they pass through by name.
  Bit-exact with fusion (no float reassociation).
- **No manifest needed** for DCE — classes and members are discovered structurally from the AST, so
  nothing can drift out of sync.

## Tests

`cargo test` covers strip/codegen, the linker, static & instance method DCE, fusion, user-code
tree-shaking, `define`/default-export handling, and source maps. `node scripts/robustness.mjs` runs a
broad syntax corpus (destructuring, private fields, generators, enums, namespaces, …) through the
binary, readable and minified, and executes each.
