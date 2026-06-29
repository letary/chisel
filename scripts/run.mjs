#!/usr/bin/env node
// Dev helper: feed a single .ts file (or a fixtures dir's main.ts) to the chisel binary and print
// the bundle. Wraps the file map into chisel's JSON-stdio protocol.
//
//   node scripts/run.mjs path/to/main.ts [--iife] [--minify]
//
// With a directory, every .ts/.js file under it is included and `<dir>/main.ts` is the entry.

import { readFileSync, readdirSync, statSync, existsSync } from "node:fs"
import { join, resolve, relative, isAbsolute } from "node:path"
import { spawnSync } from "node:child_process"

const args = process.argv.slice(2)
const target = args.find((a) => !a.startsWith("--"))
if (!target) {
  console.error("usage: node scripts/run.mjs <file.ts|dir> [--iife] [--minify]")
  process.exit(2)
}
const format = args.includes("--iife") ? "iife" : "esm"
const minify = args.includes("--minify")

const root = resolve(target)
const files = {}
let entry

const collect = (dir, base) => {
  for (const name of readdirSync(dir)) {
    const p = join(dir, name)
    if (statSync(p).isDirectory()) collect(p, base)
    else if (/\.[tj]sx?$/.test(name)) files["/" + relative(base, p)] = readFileSync(p, "utf8")
  }
}

if (statSync(root).isDirectory()) {
  collect(root, root)
  entry = "/main.ts"
} else {
  files["/main.ts"] = readFileSync(root, "utf8")
  entry = "/main.ts"
}

const bin = process.env.CHISEL_BIN || resolve(import.meta.dirname, "../target/debug/chisel")
if (!existsSync(bin)) {
  console.error(`chisel binary not found at ${bin} — run: cargo build`)
  process.exit(2)
}

const res = spawnSync(bin, [], { input: JSON.stringify({ files, entry, format, minify }), encoding: "utf8" })
if (res.error) { console.error(res.error); process.exit(2) }
const out = JSON.parse(res.stdout || "{}")
if (out.error) { console.error("ERROR:", out.error); process.exit(1) }
for (const d of out.diagnostics || []) console.error("warn:", d)
process.stdout.write(out.code)
process.stderr.write(`\n--- ${out.code.length} bytes ---\n`)
