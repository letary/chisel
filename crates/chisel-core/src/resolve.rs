//! Path resolution for the in-memory file map: only relative/absolute specifiers, `.ts`/`.js`
//! extension fallback, and **bare imports are a hard error** (the whole SDK is globals).

/// Collapse `.`/`..` segments; always returns an absolute `/`-rooted path.
pub fn normalize(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => continue,
            ".." => {
                parts.pop();
            }
            p => parts.push(p),
        }
    }
    format!("/{}", parts.join("/"))
}

/// The directory of a path (`/a/b/c.ts` → `/a/b`).
pub fn dir_of(path: &str) -> String {
    match path.rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(i) => path[..i].to_string(),
    }
}

fn has_extension(path: &str) -> bool {
    match path.rfind('/') {
        Some(i) => path[i..].contains('.'),
        None => path.contains('.'),
    }
}

fn resolve_base(importer: &str, specifier: &str) -> anyhow::Result<String> {
    if let Some(rest) = specifier.strip_prefix('/') {
        Ok(normalize(&format!("/{rest}")))
    } else if specifier.starts_with("./") || specifier.starts_with("../") {
        Ok(normalize(&format!("{}/{}", dir_of(importer), specifier)))
    } else {
        Err(anyhow::anyhow!(
            "Bare imports are not allowed — the whole SDK is available as globals (no imports): {specifier}"
        ))
    }
}

/// Resolve `specifier` (as written in `importer`) to a concrete path that exists in `files`.
pub fn resolve(files: &dyn Fn(&str) -> bool, importer: &str, specifier: &str) -> anyhow::Result<String> {
    let base = resolve_base(importer, specifier)?;
    if has_extension(&base) {
        if files(&base) {
            return Ok(base);
        }
        return Err(anyhow::anyhow!("Module not found: {base}"));
    }
    for cand in [base.clone(), format!("{base}.ts"), format!("{base}.js"), format!("{base}/index.ts")] {
        if files(&cand) {
            return Ok(cand);
        }
    }
    Err(anyhow::anyhow!("Module not found: {specifier} (from {importer})"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes() {
        assert_eq!(normalize("/a/b/../c.ts"), "/a/c.ts");
        assert_eq!(normalize("a/./b.ts"), "/a/b.ts");
    }

    #[test]
    fn dirs() {
        assert_eq!(dir_of("/a/b/c.ts"), "/a/b");
        assert_eq!(dir_of("/main.ts"), "/");
    }

    #[test]
    fn bare_is_error() {
        let exists = |_: &str| true;
        assert!(resolve(&exists, "/main.ts", "react").is_err());
    }
}
