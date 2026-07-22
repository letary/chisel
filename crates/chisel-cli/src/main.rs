//! chisel CLI — JSON over stdio. Reads an `Input` on stdin, writes an `Output` on stdout.
//!
//! Protocol:
//!   stdin : { "files": { "/main.ts": "..." }, "entry": "/main.ts", "format": "esm", "minify": false }
//!   stdout: { "code": "...", "diagnostics": [], "error": null }
//! Scan mode (no bundling — per-module imports/exports facts):
//!   stdin : { "files": { … }, "scan": true }
//!   stdout: { "code": "", "scan": { "/main.ts": { "imports": ["/util.ts"], "hasExports": false } } }
//!
//! Exit codes: 0 ok · 1 compile error (error field set) · 2 bad invocation / unreadable input.

use std::io::Read;

use chisel_core::{bundle, Input, Output};

fn main() {
    let mut buf = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut buf) {
        eprintln!("chisel: failed to read stdin: {e}");
        std::process::exit(2);
    }

    let input: Input = match serde_json::from_str(&buf) {
        Ok(i) => i,
        Err(e) => {
            emit(&Output { error: Some(format!("invalid input json: {e}")), ..Default::default() });
            std::process::exit(2);
        }
    };

    let out = bundle(input);
    let failed = out.error.is_some();
    emit(&out);
    if failed {
        std::process::exit(1);
    }
}

fn emit(out: &Output) {
    match serde_json::to_string(out) {
        Ok(s) => println!("{s}"),
        Err(e) => eprintln!("chisel: failed to serialize output: {e}"),
    }
}
