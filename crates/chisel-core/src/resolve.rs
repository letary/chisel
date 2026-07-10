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
///
/// A dotted name isn't necessarily an extension — `./city.scene` resolves to `city.scene.ts`
/// (compound extensions like `.scene.ts` name special editor files) — so the `.ts`/`.js` fallback
/// applies to every specifier. Exact matches still win, and a true asset import (`./hero.png`)
/// keeps missing here (no `hero.png.ts` exists) and falls to the caller's asset handling.
pub fn resolve(files: &dyn Fn(&str) -> bool, importer: &str, specifier: &str) -> anyhow::Result<String> {
    let base = resolve_base(importer, specifier)?;
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

    #[test]
    fn dotted_specifiers_get_the_ts_fallback() {
        // `./city.scene` → `/city.scene.ts` (compound editor extensions)
        let exists = |p: &str| p == "/city.scene.ts";
        assert_eq!(resolve(&exists, "/main.ts", "./city.scene").unwrap(), "/city.scene.ts");
        // an exact match still wins over the fallback
        let both = |p: &str| p == "/city.scene" || p == "/city.scene.ts";
        assert_eq!(resolve(&both, "/main.ts", "./city.scene").unwrap(), "/city.scene");
        // a true asset import stays unresolved (falls to the caller's asset handling)
        let asset_only = |p: &str| p == "/hero.png";
        assert_eq!(resolve(&asset_only, "/main.ts", "./hero.png").unwrap(), "/hero.png");
        assert!(resolve(&asset_only, "/main.ts", "./missing.png").is_err());
    }
}
