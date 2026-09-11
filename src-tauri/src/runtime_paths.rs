use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub(crate) struct RuntimePaths {
    pub root: PathBuf,
    pub node: PathBuf,
    pub dsh_entry: PathBuf,
    pub patch: PathBuf,
    pub find_plugin_patch: PathBuf,
    pub package_manager_bin: PathBuf,
}

fn package_entry(package: &Path) -> Result<PathBuf, String> {
    let manifest_path = package.join("package.json");
    let text = fs::read_to_string(&manifest_path)
        .map_err(|error| format!("{}: {error}", manifest_path.display()))?;
    let manifest: serde_json::Value = serde_json::from_str(&text)
        .map_err(|error| format!("{}: {error}", manifest_path.display()))?;
    let bin = &manifest["bin"];
    let entry = bin
        .as_str()
        .or_else(|| bin["dsh"].as_str())
        .ok_or_else(|| format!("{}: missing bin.dsh", manifest_path.display()))?;
    // Match the assembly validator on every OS, including Windows drive paths.
    if entry.contains(['\\', ':'])
        || entry
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(format!(
            "{}: invalid relative CLI entry {entry}",
            manifest_path.display()
        ));
    }
    Ok(package.join(entry))
}

/// Windows hands the app verbatim (`\\?\`) resource paths. Rust reads them
/// happily, but Node's CommonJS loader does not: it treats the prefix as a path
/// segment and dies with `EISDIR: illegal operation on a directory, lstat 'C:'`
/// before the server can report its URL. Every path forwarded to the child
/// process therefore uses the plain form.
fn child_path(path: &Path, windows: bool) -> PathBuf {
    if !windows {
        return path.to_path_buf();
    }
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    match text.strip_prefix(r"\\?\") {
        Some(plain) => PathBuf::from(plain),
        None => path.to_path_buf(),
    }
}

pub(crate) fn resolve_runtime(resource_dir: &Path) -> Result<RuntimePaths, String> {
    resolve_runtime_for_platform(resource_dir, cfg!(target_os = "windows"))
}

fn resolve_runtime_for_platform(
    resource_dir: &Path,
    windows: bool,
) -> Result<RuntimePaths, String> {
    let mut errors = Vec::new();
    let resource_dir = child_path(resource_dir, windows);
    // Tauri resource mappings can preserve the runtime directory or flatten it.
    for base in [resource_dir.join("runtime"), resource_dir.to_path_buf()] {
        let node = base.join(if windows { "node.exe" } else { "node" });
        let dsh = base.join("dsh");
        let package = dsh.join("node_modules/@deepseek-ai/dsh");
        let entry = package_entry(&package);
        let patch = dsh.join("openharness.patch.yml");
        let find_plugin_patch = dsh.join("openharness-find.patch.yml");
        let package_manager_bin = dsh.join("openharness-bin");
        let launcher = package_manager_bin.join(if windows { "pnpm.cmd" } else { "pnpm" });
        let mut missing = Vec::new();
        for file in [&node, &patch, &find_plugin_patch, &launcher] {
            if !file.is_file() {
                missing.push(file.display().to_string());
            }
        }
        match &entry {
            Ok(file) if !file.is_file() => missing.push(file.display().to_string()),
            Err(error) => missing.push(error.clone()),
            _ => {}
        }
        if missing.is_empty() {
            return Ok(RuntimePaths {
                root: base,
                node,
                dsh_entry: entry?,
                patch,
                find_plugin_patch,
                package_manager_bin,
            });
        }
        errors.push(format!(
            "{}: missing or invalid files: {}",
            base.display(),
            missing.join(", ")
        ));
    }
    Err(format!("bundled runtime not found; {}", errors.join("; ")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    static NEXT: AtomicUsize = AtomicUsize::new(0);

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "OpenHarness 测试 {}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }
    fn populate(root: &Path, windows: bool, entry: &str) {
        for path in [
            if windows { "node.exe" } else { "node" },
            "dsh/openharness.patch.yml",
            "dsh/openharness-find.patch.yml",
            if windows {
                "dsh/openharness-bin/pnpm.cmd"
            } else {
                "dsh/openharness-bin/pnpm"
            },
        ] {
            let file = root.join(path);
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            fs::write(file, "").unwrap();
        }
        let pkg = root.join("dsh/node_modules/@deepseek-ai/dsh");
        fs::create_dir_all(pkg.join(entry).parent().unwrap()).unwrap();
        fs::write(pkg.join(entry), "").unwrap();
        fs::write(
            pkg.join("package.json"),
            serde_json::json!({"bin":{"dsh":entry}}).to_string(),
        )
        .unwrap();
    }
    #[test]
    fn resolves_both_layouts_and_platforms_using_manifest_entry() {
        for windows in [false, true] {
            for nested in [false, true] {
                let fixture = Fixture::new();
                let root = if nested {
                    fixture.0.join("runtime")
                } else {
                    fixture.0.clone()
                };
                populate(&root, windows, "dist/cli.js");
                let paths = resolve_runtime_for_platform(&fixture.0, windows).unwrap();
                assert_eq!(paths.root, root);
                assert_eq!(
                    paths.dsh_entry,
                    root.join("dsh/node_modules/@deepseek-ai/dsh/dist/cli.js")
                );
            }
        }
    }
    #[test]
    fn windows_paths_handed_to_the_child_drop_the_verbatim_prefix() {
        assert_eq!(
            child_path(
                Path::new(r"\\?\C:\Users\Administrator\AppData\Local\OpenHarness"),
                true
            ),
            PathBuf::from(r"C:\Users\Administrator\AppData\Local\OpenHarness")
        );
        assert_eq!(
            child_path(Path::new(r"\\?\UNC\server\share\OpenHarness"), true),
            PathBuf::from(r"\\server\share\OpenHarness")
        );
        assert_eq!(
            child_path(Path::new(r"C:\Program Files\OpenHarness"), true),
            PathBuf::from(r"C:\Program Files\OpenHarness")
        );
        // POSIX paths keep any backslashes they legitimately contain.
        assert_eq!(
            child_path(Path::new(r"/opt/\\?\runtime"), false),
            PathBuf::from(r"/opt/\\?\runtime")
        );
    }

    #[test]
    fn reports_exact_missing_windows_entry_and_rejects_directories() {
        let fixture = Fixture::new();
        populate(&fixture.0.join("runtime"), true, "lib/bin.js");
        let entry = fixture
            .0
            .join("runtime/dsh/node_modules/@deepseek-ai/dsh/lib/bin.js");
        fs::remove_file(&entry).unwrap();
        let error = resolve_runtime_for_platform(&fixture.0, true).unwrap_err();
        assert!(error
            .replace('\\', "/")
            .contains(&entry.display().to_string().replace('\\', "/")));
        fs::create_dir(&entry).unwrap();
        assert!(resolve_runtime_for_platform(&fixture.0, true).is_err());
    }
    #[test]
    fn rejects_entry_outside_package() {
        let fixture = Fixture::new();
        fs::write(
            fixture.0.join("package.json"),
            r#"{"bin":{"dsh":"../bin.js"}}"#,
        )
        .unwrap();
        assert!(package_entry(&fixture.0)
            .unwrap_err()
            .contains("invalid relative CLI entry"));
    }
}
