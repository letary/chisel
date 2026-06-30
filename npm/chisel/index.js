// Resolves the chisel binary for the current platform. The binary ships in one of the optional
// `@<scope>/chisel-<platform>` packages (esbuild-style: each has `os`/`cpu` filters, so npm installs
// only the matching one). Returns the absolute path, or null if no package matches this platform.
const { join, dirname } = require("node:path")
const { existsSync } = require("node:fs")

const SCOPE = "@letary" // keep in sync with scripts/npm-publish.mjs

/** Absolute path to the chisel binary for this platform, or null if unsupported / not installed. */
function binaryPath() {
  const key = `${process.platform}-${process.arch}` // e.g. "darwin-arm64", "linux-x64", "win32-x64"
  const bin = process.platform === "win32" ? "chisel.exe" : "chisel"
  try {
    const pkgDir = dirname(require.resolve(`${SCOPE}/chisel-${key}/package.json`))
    const p = join(pkgDir, bin)
    return existsSync(p) ? p : null
  } catch {
    return null
  }
}

module.exports = { binaryPath }
