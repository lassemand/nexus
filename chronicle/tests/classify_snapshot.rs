//! Pins `classify_item` for every title in the fixtures plus the titles used
//! inline by the unit and integration tests (`fixtures/classify_titles.txt`,
//! each tried with several tag sets). NEX-211 moves the word lists into
//! per-language modules; this output must not change.
//!
//! Regenerate deliberately with `UPDATE_SNAPSHOTS=1`.

use chronicle_lib::nordic::classify::classify_item;
use serde_json::Value;
use std::fmt::Write as _;
use std::path::PathBuf;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests")
}

fn tag_vec(t: &[&str]) -> Vec<String> {
    t.iter().map(|s| s.to_string()).collect()
}

fn line(tags: &[String], title: &str) -> String {
    let c = classify_item(tags, title);
    format!(
        "{:?} | kind={:?} period={:?} correction={} calendar={} | {:?}\n",
        tags, c.report_kind, c.fiscal_period, c.is_correction, c.calendar_update, title
    )
}

fn items(v: &Value) -> Vec<&Value> {
    match v {
        Value::Array(a) => a.iter().collect(),
        Value::Object(o) => o.get("items").map(|i| items(i)).unwrap_or_default(),
        _ => vec![],
    }
}

fn json_files(dir: &std::path::Path, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            json_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "json") {
            out.push(p);
        }
    }
}

#[test]
fn classify_output_is_pinned() {
    let mut out = String::new();

    // Every item of every JSON fixture, with its real tags.
    let mut files = Vec::new();
    json_files(&root().join("fixtures"), &mut files);
    files.sort();
    for f in files {
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&f).unwrap()).unwrap();
        writeln!(out, "# {}", f.strip_prefix(root()).unwrap().display()).unwrap();
        for it in items(&v) {
            let Some(title) = it["content"]["title"].as_str().or(it["title"].as_str()) else {
                continue;
            };
            let tags: Vec<String> = it["properties"]["tags"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|t| t.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            out.push_str(&line(&tags, title));
        }
    }

    // Titles used inline by the tests, under several tag sets.
    writeln!(out, "# classify_titles.txt").unwrap();
    let titles = std::fs::read_to_string(root().join("fixtures/classify_titles.txt")).unwrap();
    let tag_sets = [
        tag_vec(&[]),
        tag_vec(&[":regulatory"]),
        tag_vec(&[":regulatory", "sub:report:interim"]),
        tag_vec(&[":regulatory", "sub:report", "sub:report:annual"]),
        tag_vec(&[":regulatory", "sub:report:interim", "sub:report:interim:q4"]),
    ];
    for title in titles.strip_suffix('\n').unwrap_or(&titles).split('\n') {
        for tags in &tag_sets {
            out.push_str(&line(tags, title));
        }
    }

    let file = root().join("snapshots/classify.txt");
    if std::env::var_os("UPDATE_SNAPSHOTS").is_some() {
        std::fs::write(&file, &out).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&file).unwrap();
    assert_eq!(expected, out, "classify snapshot changed");
}
