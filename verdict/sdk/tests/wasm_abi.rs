//! Builds the sample kernel for `wasm32-unknown-unknown` and checks its ABI:
//! it exports `memory`, `verdict_input` and `verdict_run`, and imports nothing.

use std::path::PathBuf;
use std::process::Command;

fn leb(bytes: &[u8], at: &mut usize) -> usize {
    let mut value = 0usize;
    let mut shift = 0;
    loop {
        let byte = *bytes.get(*at).expect("truncated LEB128");
        *at += 1;
        value |= usize::from(byte & 0x7f) << shift;
        shift += 7;
        if byte & 0x80 == 0 {
            return value;
        }
    }
}

fn name(bytes: &[u8], at: &mut usize) -> String {
    let len = leb(bytes, at);
    let text = std::str::from_utf8(bytes.get(*at..*at + len).expect("truncated name"))
        .expect("names are UTF-8")
        .to_owned();
    *at += len;
    text
}

fn imports_and_exports(bytes: &[u8]) -> (usize, Vec<String>) {
    let mut at = 8;
    let mut imports = 0;
    let mut exports = Vec::new();
    while at < bytes.len() {
        let id = bytes[at];
        at += 1;
        let size = leb(bytes, &mut at);
        let end = at + size;
        let mut cursor = at;
        match id {
            2 => imports = leb(bytes, &mut cursor),
            7 => {
                let count = leb(bytes, &mut cursor);
                for _ in 0..count {
                    exports.push(name(bytes, &mut cursor));
                    cursor += 1;
                    leb(bytes, &mut cursor);
                }
            }
            _ => {}
        }
        at = end;
    }
    (imports, exports)
}

#[test]
fn sample_kernel_exports_the_abi_and_imports_nothing() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let status = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
        .current_dir(&root)
        .args([
            "build",
            "-p",
            "verdict-sample",
            "--target",
            "wasm32-unknown-unknown",
            "--release",
        ])
        .status()
        .expect("cargo runs");
    assert!(
        status.success(),
        "building the sample kernel for wasm32 failed"
    );
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("target"));
    let wasm = target.join("wasm32-unknown-unknown/release/verdict_sample.wasm");
    let bytes = std::fs::read(&wasm).unwrap_or_else(|err| panic!("reading {wasm:?}: {err}"));
    let (imports, mut exports) = imports_and_exports(&bytes);
    exports.sort();
    assert_eq!(imports, 0, "a kernel must import nothing");
    assert_eq!(exports, ["memory", "verdict_input", "verdict_run"]);
}
