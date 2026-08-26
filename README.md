# chisel

A small, purpose-built bundler in **Rust** (built on [SWC](https://swc.rs)) for TypeScript projects
that deliver an "SDK" of classes as injected globals (the esbuild `inject:` pattern). It does two
things general bundlers (esbuild, Rollup, Rolldown) structurally can't:

- **Method-granular tree-shaking** — drop *unused class methods* (static and instance), not just
  unused modules. To a general bundler a class is a single binding: use one static factory and you
  keep every method. chisel discovers classes/members structurally from the AST and drops the long
  tail of unused methods (and everything only they pulled in).
- **Math chain fusion** — lower a `Vec3`-style method chain like `a.add(b).scale(s).dot(c)` to scalar
  arithmetic at compile time, so no intermediate objects are allocated. The same scalar-replacement
  machinery also fuses single-scalar value types: a `date()` chain like `date(x).add(1,'day').format(p)`
  lowers to `formatImpl(toMs(x) + 86400000, p)` — the wrapper never allocates.

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

The same pass fuses **`date()` chains** — the single-scalar (epoch-ms) analog of a Vec3. Roots are the
`date(...)` factory (or a typed local); linear-unit `add`/`subtract` (ms…week) become scalar ms math;
scalar terminals (`diff`/`valueOf`/`unix`/`isBefore`/…) allocate nothing; and the materializing
terminals `format`/`timeAgo` lower to the SDK's module-private `formatImpl`/`timeAgoImpl(ms, …)`. An
escaping linear chain rebuilds exactly one wrapper (`new DateValue(ms)`); calendar units (`month`/
`year`) and local-tz `startOf`/`endOf` aren't closed-form, so those chains are left as real calls.

Validated **bit-exact** against the un-fused output over 800+ randomized values — variable-rooted
per-frame blocks, `normalize`/`length`/`distanceTo`, inside loops and arrow callbacks — since JS
numbers are all f64 and the lowering never reassociates. On 100k-iteration loops it cuts `Vec3`
allocations 43–57% with identical results. Fusion runs before DCE, so the inlined methods then
disappear from the bundle too.

### Curve chain fusion + validation (particle config)

A third chain shape rides the same machinery, but for *data* rather than math: the LeCodes SDK's
`curve()` / `colorCurve()` builders, whose entire state is the flat `Float32Array` the native particle
bridge expects. A literal chain is fully known at compile time, so `curve(0.3).fade(0.15)` becomes
`{ _data: new Float32Array([0,0.3,0.3,4,0,0,0,0.15,1,1,0.85,1,1,1,0,0]) }` — the same duck type the
runtime builder presents through its `_data` getter — with hex colors parsed to floats at build time
(the strings never ship, and the parser tree-shakes away).

Because a builder is **mutable** (`.to()` returns `this`), a chain is only rewritten where its value
is *immediately consumed*: a call argument, an object-literal property, the right side of a member
assignment, plus the pass-throughs that forward a value to the same consumer (`??`, `?:`, parens). A
chain bound to a variable is left as real calls, since `const c = curve(1)` … `c.to(0)` must keep
working — and a chain that can't be lowered (non-literal stop `t`, computed color, unknown method) is
left *whole*, never half-lowered into `{_data}.via(x, 2)`.

The payoff is less about bytes than about **validation**: the same pass checks what the engine would
otherwise only warn about on-device — stop `t` out of order or outside `0..1`, more than 8 stops,
`.from()` after another stop, an unparsable color literal — and reports `path:line:col: message`
through `Output.diagnostics`, leaving the chain alone so runtime behavior is unchanged. Validation
runs regardless of `fuse`; only the rewrite is opt-in. `node scripts/curve-exact.mjs` bundles a
44-case corpus twice (fusion on and off), runs both, and compares every float of every buffer the two
produce.

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
assets, sourcemap, keep, reactive_ui, flatten_ui }`. Output: `{ code, map?, diagnostics, error? }`.

- `inject` — every export of these entries becomes an ambient global (esbuild `inject:` semantics).
- `define` — compile-time global substitutions (numeric, e.g. `DEG2RAD` → `0.0174…`).
- `assets` — `path → URL` map for `asset('./x')` / `import x from './x.png'`.
- `sourcemap` — emit a v3 map in `Output.map` (`sources` are the original module paths).
- `keep` — instance-method names the **host calls by name** (no in-bundle caller, so DCE can't see
  them). Kept on any *reached* class that defines them. An entry ending in `*` is a prefix, so
  `["_*"]` keeps every underscore-prefixed method. Not needed for accessors written through a
  config object (`Object.assign(inst, { friction })`): an object-literal key that matches an
  instance setter keeps that accessor pair on its own.
- `reactive_ui` — SDK signals desugaring: memoize `.map` inside children bindings (`__uiMap`) and
  auto-wrap signal-reading text/style expressions in arrows. Keys on free references to the
  injected globals, so SDK-internal code is inert. Off by default.
- `flatten_ui` — two rewrites over UI factory calls. (1) Splice literal array arguments into
  variadic arguments (`UIColumn([a, b])` → `UIColumn(a, b)`; the SDK runtime flattens array args
  one level anyway — drops an array allocation per call site). Guards: spreads, holes, nested
  array literals, function elements, and a leading object literal (the legacy style position) are
  left alone. (2) Raw-lower calls whose arguments are all *provably* plain children — known
  factory calls (incl. `.style()`/`.onClick()` builder chains), `const`s initialized from them
  (fixpoint), falsy literals, `&&`/`?:` over those — to the SDK's dispatch-free builders:
  `UIColumn(a, b)` → `__UIColumn([a, b])`, skipping the runtime style-probe + per-arg array scan
  entirely (and letting the unused dispatch machinery tree-shake away). The reactive form lowers
  too — a lone literal closure `UIColumn(() => […])` becomes `__UIColumn(() => […])` (the Element
  constructor branches on `typeof`), composing with `reactive_ui`'s `__uiMap` memoization on the
  same call. Anything unproven falls back to the normal call. Requires an SDK with variadic
  factory dispatch + `__UI*` raw builders. Off by default.
- `scan: true` — no bundling: parse every JS/TS module in `files` (`.d.ts` skipped) and return
  `Output.scan = { path: { imports: [resolved paths], hasExports } }` — the dependency facts entry
  detection needs before an entry is known. `entry` is not required in this mode; per-file parse
  failures land in that file's `error` field instead of failing the scan.

- `diagnostics` — non-fatal build-time findings, `path:line:col: message` (today: particle curve
  chains the engine would reject; reported whether or not `fuse` is set).

Helpers: `node scripts/run.mjs <file|dir>` runs a quick bundle; `node scripts/robustness.mjs` runs a
broad JS/TS syntax corpus through the binary and checks each survives;
`node scripts/curve-exact.mjs` checks curve fusion against the real builders, buffer for buffer.

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
  not). Destructuring keys count as reads (`const { velocity } = body` keeps the getter), and an
  object-literal key that matches an instance *setter* counts as a write (`Object.assign(inst,
  { friction: 0.02 })` keeps the `friction` accessor pair — a computed key `{ [k]: v }` does not).
  The same pass shakes user modules: unused pure decls / exports are dropped and user classes are
  method-DCE'd, while side-effecting top-level statements and `import './setup'` side-effect imports are
  kept; injected SDK modules are pulled purely on demand. One exception keeps the `sideEffects: false`
  contract honest: an SDK top-level statement that *mutates a binding that module declares*
  (`X.y = …`, `X.a.b = …`, `Object.defineProperty(X, …)`) counts as part of `X`'s declaration — kept
  iff `X` is reached, dropped with it otherwise, and its references are held back until then. Without
  that, the common "callable factory + ambient statics" shape loses its statics silently, and only in
  bundled builds. A statement that merely *passes* the binding somewhere (`register(X)`) is still an
  ordinary side effect, and still dropped.
- **Minify** runs the real SWC minifier (compress + top-level mangle). The fully-linked bundle is one
  closed scope with no exports, so every top-level binding is private and renameable; free identifiers
  are exactly the host globals, which the resolver marks `unresolved` so they pass through by name.
  Bit-exact with fusion (no float reassociation).
- **No manifest needed** for DCE — classes and members are discovered structurally from the AST, so
  nothing can drift out of sync.

## Tests

`cargo test` covers strip/codegen, the linker, static & instance method DCE, fusion (math, date, and
curve payloads + diagnostics), user-code tree-shaking, `define`/default-export handling, and source
maps. `node scripts/robustness.mjs` runs a broad syntax corpus (destructuring, private fields,
generators, enums, namespaces, …) through the binary, readable and minified, and executes each;
`node scripts/curve-exact.mjs` proves fused curve payloads are bit-identical to the builders'.
