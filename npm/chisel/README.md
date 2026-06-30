# @letary/chisel

The platform-binary resolver for [chisel](https://github.com/letary/chisel) — a method-granular
bundler with math fusion (Rust/SWC).

Installing this package pulls the binary for your OS/arch via an optional
`@letary/chisel-<platform>` dependency (the same per-platform mechanism esbuild uses), so there's no
postinstall download.

```js
const { binaryPath } = require("@letary/chisel")
const bin = binaryPath() // absolute path to the chisel binary, or null if this platform isn't supported
```

The binary speaks JSON over stdio — see the main repo for the input/output contract.
