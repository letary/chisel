#!/usr/bin/env node
// Assemble + publish chisel's npm packages (esbuild-style per-platform distribution).
//
//   node scripts/npm-publish.mjs platform <rust-target> <version> <binary-path>
//   node scripts/npm-publish.mjs main <version>
//
// `platform` builds a one-binary `@letary/chisel-<platform>` package (with os/cpu filters) and
// publishes it. `main` publishes `@letary/chisel` (the resolver) with optionalDependencies on every
// platform package at the same version. `npm publish` reads NODE_AUTH_TOKEN from the environment.

import { mkdtempSync, copyFileSync, writeFileSync, readFileSync, chmodSync } from "node:fs"
import { join, dirname } from "node:path"
import { tmpdir } from "node:os"
import { fileURLToPath } from "node:url"
import { execFileSync } from "node:child_process"

const SCOPE = "@letary" // keep in sync with npm/chisel/index.js
const REPO = "https://github.com/letary/chisel"
const __dirname = dirname(fileURLToPath(import.meta.url))

// rust target -> npm platform package metadata. `node` matches `${process.platform}-${process.arch}`.
const TARGETS = {
  "aarch64-apple-darwin":       { os: "darwin", cpu: "arm64", node: "darwin-arm64", bin: "chisel" },
  "x86_64-apple-darwin":        { os: "darwin", cpu: "x64",   node: "darwin-x64",   bin: "chisel" },
  "x86_64-unknown-linux-musl":  { os: "linux",  cpu: "x64",   node: "linux-x64",    bin: "chisel" },
  "aarch64-unknown-linux-musl": { os: "linux",  cpu: "arm64", node: "linux-arm64",  bin: "chisel" },
  "x86_64-pc-windows-msvc":     { os: "win32",  cpu: "x64",   node: "win32-x64",    bin: "chisel.exe" },
}

const publish = (cwd) => execFileSync("npm", [ "publish", "--access", "public" ], { cwd, stdio: "inherit" })

const [ mode, ...rest ] = process.argv.slice(2)

if (mode === "platform") {
  const [ target, version, binaryPath ] = rest
  const t = TARGETS[target]
  if (!t) throw new Error(`unknown rust target: ${target}`)
  const dir = mkdtempSync(join(tmpdir(), "chisel-npm-"))
  copyFileSync(binaryPath, join(dir, t.bin))
  if (t.os !== "win32") chmodSync(join(dir, t.bin), 0o755) // keep the executable bit through the tarball
  writeFileSync(join(dir, "package.json"), JSON.stringify({
    name: `${SCOPE}/chisel-${t.node}`,
    version,
    description: `chisel bundler binary for ${t.node}`,
    repository: { type: "git", url: `${REPO}.git` },
    license: "MIT",
    os: [ t.os ],
    cpu: [ t.cpu ],
    files: [ t.bin ],
  }, null, 2) + "\n")
  publish(dir)
} else if (mode === "main") {
  const [ version ] = rest
  const src = join(__dirname, "..", "npm", "chisel")
  const dir = mkdtempSync(join(tmpdir(), "chisel-npm-"))
  for (const f of [ "index.js", "README.md" ]) copyFileSync(join(src, f), join(dir, f))
  const pkg = JSON.parse(readFileSync(join(src, "package.json"), "utf8"))
  pkg.version = version
  pkg.optionalDependencies = Object.fromEntries(
    Object.values(TARGETS).map((t) => [ `${SCOPE}/chisel-${t.node}`, version ]),
  )
  writeFileSync(join(dir, "package.json"), JSON.stringify(pkg, null, 2) + "\n")
  publish(dir)
} else {
  console.error("usage: npm-publish.mjs platform <rust-target> <version> <binary> | main <version>")
  process.exit(2)
}
