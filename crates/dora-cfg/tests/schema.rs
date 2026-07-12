//! Wires `config_schema.json` into CI: compiles it and validates every sample
//! config against it, so the schema can't silently drift from the config format.
//!
//! Runs as part of `cargo test` (the CI "Test Suite" job). Validation mirrors
//! the `dora-cfg --schema` tool: jsonschema Draft7 over the config as a JSON
//! value.

use std::path::{Path, PathBuf};

/// repo root, relative to this crate (crates/dora-cfg)
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("resolve repo root")
}

/// Recursively convert a YAML value to a JSON value. Map keys are stringified
/// (YAML allows non-string keys like the numeric option codes `23:`, but JSON —
/// and thus JSON Schema — only has string keys). Null-valued map entries are
/// dropped: a bare `key:` in YAML is how the sample configs spell "absent" for
/// an optional field, which is what serde sees as `None`.
fn yaml_to_json(y: yaml_serde::Value) -> serde_json::Value {
    use serde_json::Value as J;
    use yaml_serde::Value as Y;
    match y {
        Y::Null => J::Null,
        Y::Bool(b) => J::Bool(b),
        Y::Number(n) => {
            if let Some(i) = n.as_i64() {
                J::from(i)
            } else if let Some(u) = n.as_u64() {
                J::from(u)
            } else if let Some(f) = n.as_f64() {
                serde_json::Number::from_f64(f)
                    .map(J::Number)
                    .unwrap_or(J::Null)
            } else {
                J::Null
            }
        }
        Y::String(s) => J::String(s),
        Y::Sequence(seq) => J::Array(seq.into_iter().map(yaml_to_json).collect()),
        Y::Mapping(map) => {
            let mut obj = serde_json::Map::new();
            for (k, v) in map {
                if matches!(v, Y::Null) {
                    continue; // bare `key:` == absent optional field
                }
                let key = match k {
                    Y::String(s) => s,
                    Y::Number(n) => n.to_string(),
                    Y::Bool(b) => b.to_string(),
                    other => yaml_to_json(other).to_string(),
                };
                obj.insert(key, yaml_to_json(v));
            }
            J::Object(obj)
        }
        // yaml_serde tagged values (`!Tag value`) — unwrap to the inner value;
        // dora configs don't use YAML tags, so this is just a safety net.
        Y::Tagged(t) => yaml_to_json(t.value),
    }
}

fn load_config(path: &Path) -> serde_json::Value {
    let text =
        std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    match path.extension().and_then(|e| e.to_str()) {
        Some("json") => serde_json::from_str(&text)
            .unwrap_or_else(|e| panic!("parse json {}: {e}", path.display())),
        _ => {
            let y: yaml_serde::Value = yaml_serde::from_str(&text)
                .unwrap_or_else(|e| panic!("parse yaml {}: {e}", path.display()));
            yaml_to_json(y)
        }
    }
}

#[test]
fn all_sample_configs_match_schema() {
    let root = repo_root();
    let schema_json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(root.join("config_schema.json")).unwrap())
            .expect("config_schema.json is valid JSON");
    let schema = jsonschema::options()
        .with_draft(jsonschema::Draft::Draft7)
        .build(&schema_json)
        .expect("config_schema.json compiles as a Draft7 schema");

    // every real-world / sample config we ship should validate against the schema.
    // glob the sample dir (plus the root example) so new samples are covered
    // automatically rather than needing to be added to a hand-maintained list.
    let mut samples = vec![root.join("example.yaml")];
    let sample_dir = root.join("crates/libs/config/sample");
    for entry in std::fs::read_dir(&sample_dir).expect("read sample dir") {
        let path = entry.expect("sample dir entry").path();
        if matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("yaml") | Some("json")
        ) {
            samples.push(path);
        }
    }
    samples.sort();

    let mut failures = Vec::new();
    for sample in &samples {
        let doc = load_config(sample);
        for e in schema.iter_errors(&doc) {
            failures.push(format!(
                "{}: {} (at {})",
                sample.file_name().unwrap().to_string_lossy(),
                e,
                e.instance_path()
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "sample configs failed schema validation:\n{}",
        failures.join("\n")
    );
}
