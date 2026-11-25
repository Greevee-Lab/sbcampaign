// SPDX-License-Identifier: AGPL-3.0-only
//! Enforces the core boundary rule (ADR-0001): new code outside the campaign
//! core never accesses the database.

use std::path::{Path, PathBuf};

/// Upstream files: unchanged campaign logic and protocol, may use the database.
const UPSTREAM: &[&str] = &[
    "src/main.rs",
    "src/client.rs",
    "src/command.rs",
    "src/command/rx.rs",
    "src/command/tx.rs",
    "src/service.rs",
];

/// The fork's campaign core: the only new code that may use the database.
const CORE_DIR: &str = "src/campaign_core/";

/// Database access markers (assembled so this file does not match itself).
fn markers() -> Vec<String> {
    let m = |a: &str, b: &str| format!("{a}{b}");
    vec![
        m("sq", "lx"),
        m("get_", "pool"),
        m("Pg", "Pool"),
        m("Pg", "Connection"),
        m("SEL", "ECT "),
        m("INS", "ERT INTO"),
        m("UPD", "ATE "),
        m("DEL", "ETE FROM"),
        m("CREATE ", "TABLE"),
    ]
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn new_code_has_no_sql() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    rust_files(&root.join("src"), &mut files);
    let markers = markers();
    let mut checked = 0;
    let mut violations = Vec::new();

    for file in files {
        let rel = file.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
        if UPSTREAM.contains(&rel.as_str()) || rel.starts_with(CORE_DIR) {
            continue;
        }
        checked += 1;
        let text = std::fs::read_to_string(&file).unwrap();
        for (n, line) in text.lines().enumerate() {
            let code = line.split("//").next().unwrap_or("");
            for marker in &markers {
                if code.contains(marker.as_str()) {
                    violations.push(format!("{rel}:{}: {marker:?}", n + 1));
                }
            }
        }
    }

    assert!(checked >= 4, "boundary files not found ({checked})");
    assert!(
        violations.is_empty(),
        "database access outside {CORE_DIR} (ADR-0001, see src/core_api/mod.rs):\n{}",
        violations.join("\n")
    );
}

#[test]
fn upstream_file_list_is_current() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    for f in UPSTREAM {
        assert!(root.join(f).is_file(), "{f} listed as upstream but missing");
    }
}
