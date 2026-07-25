#!/usr/bin/env node
// Bit-exact check for particle curve fusion: bundle each case TWICE — with `fuse` on (the chain is
// lowered to `{ _data: new Float32Array([…]) }` at compile time) and off (the real `curve()` builder
// runs) — then run both and compare every float of every produced buffer with `Object.is`.
//
// The SDK under test is `fixtures/curve-sdk/` (verbatim builders from packages/sdk). A case reports
// its curves by calling the host global `sink(builder)`; the harness reads `_data` either way.
//
//   node scripts/curve-exact.mjs [--show <name>]

import { spawnSync } from "node:child_process"
import { existsSync, readFileSync } from "node:fs"
import { join } from "node:path"
import vm from "node:vm"

const root = join(import.meta.dirname, "..")
const bin = ["target/release/chisel", "target/debug/chisel"]
  .flatMap((p) => [p, p + ".exe"])
  .map((p) => join(root, p)).find(existsSync)
if (!bin) { console.error("build chisel first (cargo build --release)"); process.exit(2) }
const showOnly = process.argv.includes("--show") ? process.argv[process.argv.indexOf("--show") + 1] : null

const SDK = {
  "/__sdk/inject.ts": readFileSync(join(root, "fixtures/curve-sdk/inject.ts"), "utf8"),
  "/__sdk/curve.ts": readFileSync(join(root, "fixtures/curve-sdk/curve.ts"), "utf8"),
}

// Every case: source that pushes one or more curves through `sink`. `fusedAway` asserts the builder
// classes left the bundle entirely (the whole point — only literal chains manage that).
const cases = [
  // --- the documented shapes ---
  ["fade-default", `sink(curve(0.3).fade())`, { fusedAway: true }],
  ["fade-symmetric", `sink(curve(0.3).fade(0.15))`, { fusedAway: true }],
  ["fade-asymmetric", `sink(curve(1).fade(0.1, 0.4))`, { fusedAway: true }],
  ["fade-no-middle", `sink(curve(1).fade(0.5, 0.5))`, { fusedAway: true }],
  ["fade-clamped-down", `sink(curve(1).fade(0.9, 0.9))`, { fusedAway: true }],
  ["fade-no-up", `sink(curve(1).fade(0, 0.3))`, { fusedAway: true }],
  ["fade-out-of-range", `sink(curve(1).fade(-1, 5))`, { fusedAway: true }],
  ["from-to-range", `sink(curve({ min: 1, max: 1.6 }).from(0.3).to(1))`, { fusedAway: true }],
  ["add-mode-range-stop", `sink(curve(0, 'add').to({ min: -2, max: 2 }))`, { fusedAway: true }],
  ["implicit-base-prepend", `sink(curve().to(0))`, { fusedAway: true }],
  ["no-stops", `sink(curve(2))`, { fusedAway: true }],
  ["many-vias", `sink(curve(0.5).via(0.25, 2).via(0.5, 3).to(1))`, { fusedAway: true }],
  ["eight-stops", `sink(curve(1).from(0).via(0.1,1).via(0.2,2).via(0.3,3).via(0.4,4).via(0.5,5).via(0.6,6).to(7))`, { fusedAway: true }],
  ["f32-rounding", `sink(curve(0.1).via(0.3333333333333333, 0.7071067811865476).to(1e-8))`, { fusedAway: true }],
  ["negative-literals", `sink(curve(-1.5, 'add').from(-0).to(-2.25))`, { fusedAway: true }],
  // --- colors ---
  ["color-hex-journey", `sink(colorCurve('#ffaa33').via(0.7, '#ffffff').to('#000000'))`, { fusedAway: true }],
  ["color-shorthand", `sink(colorCurve('#abc').to('#abcd'))`, { fusedAway: true }],
  ["color-packed-and-array", `sink(colorCurve(0xff8800).to([0.1, 0.2, 0.3]))`, { fusedAway: true }],
  ["color-rgba8", `sink(colorCurve('#ffffff80').to([0.5, 0.5, 0.5, 0.25]))`, { fusedAway: true }],
  ["color-range", `sink(colorCurve({ min: '#f00', max: '#00f' }).to('#000'))`, { fusedAway: true }],
  ["color-default-base", `sink(colorCurve().to('#112233'))`, { fusedAway: true }],
  ["color-uppercase-hex", `sink(colorCurve('#FFAA33').to('#0F0'))`, { fusedAway: true }],
  // --- non-literal values: the `lohi` typeof test is inlined, so these still fuse ---
  ["var-number-base", `const s = 2.5\nsink(curve(s).to(0))`, { fusedAway: true }],
  ["var-range-base", `const r = { min: 1, max: 2 }\nsink(curve(r).to(0))`, { fusedAway: true }],
  ["var-in-range-literal", `const n = 8\nsink(curve({ min: 0, max: n - 1 }, 'add').from(0).to(n))`, { fusedAway: true }],
  ["member-value", `const cfg = { size: 3 }\nsink(curve(cfg.size).via(0.5, cfg.size).to(0))`, { fusedAway: true }],
  // --- positions: object literal / member assignment / pass-throughs ---
  ["object-prop", `const o = { size: curve(1).to(0), opacity: curve(0.3).fade() }\nsink(o.size)\nsink(o.opacity)`, { fusedAway: true }],
  ["member-assign", `const p = { size: null }\np.size = curve(1).to(0)\nsink(p.size)`, { fusedAway: true }],
  ["nullish-passthrough", `const given = undefined\nsink(given ?? curve(1).to(0))`, { fusedAway: true }],
  ["ternary-passthrough", `sink(1 > 0 ? curve(1).to(0) : curve(2).to(1))`, { fusedAway: true }],
  // --- bail-outs: identical results, builder still shipped (chain left as real calls) ---
  ["bail-var-binding", `const c = curve(1).to(0)\nsink(c)`, { fusedAway: false }],
  ["bail-mutated-binding", `const c = curve(1)\nc.via(0.5, 2)\nc.to(0)\nsink(c)`, { fusedAway: false }],
  ["bail-computed-base", `const f = () => 2\nsink(curve(f()).to(0))`, { fusedAway: false }],
  ["bail-dynamic-t", `const t = 0.4\nsink(curve(1).via(t, 2).to(0))`, { fusedAway: false }],
  ["bail-dynamic-fade", `const inv = 0.2\nsink(curve(1).fade(inv))`, { fusedAway: false }],
  ["bail-dynamic-color", `const hex = '#ff0000'\nsink(colorCurve(hex).to('#000'))`, { fusedAway: false }],
  ["bail-dynamic-mode", `const m = 'add'\nsink(curve(1, m).to(0))`, { fusedAway: false }],
  ["bail-array-element-expr", `const g = 0.5\nsink(colorCurve([0.1, g, 0.3]).to('#000'))`, { fusedAway: false }],
  ["bail-return-value", `const make = () => curve(1).to(0)\nsink(make())`, { fusedAway: false }],
  // --- invalid chains: reported, and left alone so the runtime behaves exactly as before ---
  ["invalid-descending", `sink(curve(1).via(0.8, 1).via(0.2, 0))`, { fusedAway: false, diag: "ascend" }],
  ["invalid-t-range", `sink(curve(1).via(1.5, 1).to(0))`, { fusedAway: false, diag: "outside 0..1" }],
  ["invalid-nine-stops", `sink(curve(1).via(0.1,1).via(0.2,2).via(0.3,3).via(0.4,4).via(0.5,5).via(0.6,6).via(0.7,7).to(8))`, { fusedAway: false, diag: "at most 8" }],
  ["invalid-color", `sink(colorCurve('#nope!!').to('#000'))`, { fusedAway: false, diag: "not a color" }],
  ["invalid-mode", `sink(curve(1, 'plus').to(0))`, { fusedAway: false, diag: "unknown mode" }],
]

// `keep: ["_*"]` is what lecodes-cli passes: the `_data` getter has no in-bundle caller here (the
// host reads it), and without it method-DCE would drop the very thing under test.
const run = (files, fuse) => {
  const input = JSON.stringify({ files, entry: "/main.ts", inject: ["/__sdk/inject.ts"], format: "esm", minify: false, fuse, keep: ["_*"] })
  const r = spawnSync(bin, [], { input, encoding: "utf8", maxBuffer: 64 << 20 })
  if (r.status === 2) return { fatal: (r.stderr || "crash").split("\n")[0] }
  try { return JSON.parse(r.stdout || "{}") } catch { return { fatal: "bad json: " + (r.stdout || r.stderr || "").slice(0, 120) } }
}

const execute = (code) => {
  const out = []
  const ctx = vm.createContext({ Math, Object, Array, Float32Array, JSON, Number, String, Boolean, parseInt, parseFloat, Error, console: { log() {}, warn() {} }, sink: (v) => out.push(Array.from(v._data)) })
  vm.runInContext(code, ctx, { timeout: 4000 })
  return out
}

let pass = 0, fail = 0
for (const [name, body, opts = {}] of cases) {
  const files = { ...SDK, "/main.ts": body }
  let verdict = "ok"
  try {
    const on = run(files, true)
    const off = run(files, false)
    if (on.fatal || off.fatal) throw new Error(`chisel: ${on.fatal || off.fatal}`)
    if (on.error || off.error) throw new Error(`chisel: ${on.error || off.error}`)
    if (showOnly === name) console.log(`\n--- ${name} (fused) ---\n${on.code}\n--- diagnostics: ${JSON.stringify(on.diagnostics)}`)
    // The builders must be gone exactly when the case says every chain fused.
    const shipped = /class .*Builder|_stops/.test(on.code)
    if (opts.fusedAway && shipped) throw new Error("chain did not fuse (builder still in the bundle)")
    if (!opts.fusedAway && !shipped) throw new Error("chain fused but was expected to bail")
    if (opts.diag) {
      const hit = (on.diagnostics ?? []).some((d) => d.includes(opts.diag))
      if (!hit) throw new Error(`missing diagnostic ${JSON.stringify(opts.diag)}: ${JSON.stringify(on.diagnostics)}`)
      // Validation must not depend on the rewrite being enabled.
      if (!(off.diagnostics ?? []).some((d) => d.includes(opts.diag))) throw new Error("diagnostic missing without fuse")
    } else if ((on.diagnostics ?? []).length) {
      throw new Error(`unexpected diagnostics: ${JSON.stringify(on.diagnostics)}`)
    }
    const a = execute(on.code), b = execute(off.code)
    if (a.length !== b.length || a.length === 0) throw new Error(`sink count ${a.length} vs ${b.length}`)
    for (let i = 0; i < a.length; i++) {
      if (a[i].length !== b[i].length) throw new Error(`buffer ${i} length ${a[i].length} vs ${b[i].length}`)
      for (let k = 0; k < a[i].length; k++) {
        if (!Object.is(a[i][k], b[i][k])) throw new Error(`buffer ${i}[${k}]: fused ${a[i][k]} !== builder ${b[i][k]}\n  fused:   [${a[i]}]\n  builder: [${b[i]}]`)
      }
    }
  } catch (e) {
    verdict = e.message
  }
  const ok = verdict === "ok"
  ok ? pass++ : fail++
  console.log(`  ${ok ? "✓" : "✗"} ${name.padEnd(26)} ${ok ? "" : verdict}`)
}
console.log(`\n${pass}/${pass + fail} bit-exact${fail ? `, ${fail} FAILED` : ""}`)
process.exit(fail ? 1 : 0)
