// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! Every source file names its license and copyright holder in its first
//! lines, so a file copied out of the tree on its own still says whose it is.

use std::path::{Path, PathBuf};

const ID: &str = "SPDX-License-Identifier: Apache-2.0";
const COPYRIGHT: &str = "Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)";
const EXTENSIONS: &[&str] = &["rs", "sh", "ps1", "js"];

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n == "target") {
                continue;
            }
            collect(&path, out);
        } else if path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| EXTENSIONS.contains(&e))
        {
            out.push(path);
        }
    }
}

#[test]
fn every_source_file_carries_the_spdx_header() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut files = vec![root.join("install.sh"), root.join("install.ps1")];
    for dir in ["rust", "scripts", "plugins"] {
        collect(&root.join(dir), &mut files);
    }

    let missing: Vec<_> = files
        .iter()
        .filter(|f| {
            let text = std::fs::read_to_string(f).unwrap();
            let head: Vec<_> = text.lines().take(3).collect();
            !(head.iter().any(|l| l.ends_with(ID)) && head.iter().any(|l| l.ends_with(COPYRIGHT)))
        })
        .collect();
    assert!(files.len() > 100, "found only {} files", files.len());
    assert!(missing.is_empty(), "missing the SPDX header: {missing:#?}");
}
