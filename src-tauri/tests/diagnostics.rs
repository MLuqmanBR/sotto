//! The diagnostics bundle must be safe to share.
//!
//! Acceptance A17: "Diagnostics export contains no transcripts, keys or absolute
//! paths by default." That was previously asserted only in a source comment —
//! which is not a check, and the code contradicted it: the report included the
//! model's absolute directory, `/home/<user>/…`, which discloses the username and
//! the directory layout.
//!
//! The report is now built by `pipeline::diagnostics_report` and this test walks
//! the whole JSON tree looking for anything that should not be there. It is not
//! opt-in: it needs no device, no model and no audio, so it runs on every
//! `cargo test --workspace`.

use std::sync::Arc;

/// A root of its own per test. Two tests sharing one directory raced: whichever
/// finished first deleted it while the other was still reading, which is a test
/// bug that looks exactly like a product bug.
fn app_with_content(name: &str) -> (Arc<sotto::pipeline::App>, std::path::PathBuf) {
    let root = std::env::temp_dir().join(format!("sotto-diag-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("config")).unwrap();
    std::fs::create_dir_all(root.join("history")).unwrap();

    // Seed a dictionary entry and a take with a transcript. Neither may appear in
    // the report: a diagnostics file is about the build, not about the user.
    let entries = vec![sotto_dict::Entry {
        id: "e1".into(),
        preferred: "SottoSecretWord".into(),
        aliases: vec![],
        language: None,
        enabled: true,
        case_policy: sotto_dict::CasePolicy::Exact,
    }];
    sotto_core::save_dictionary(&root.join("config/dictionary.json"), &entries, &[]).unwrap();

    let store = sotto_core::Store::open(root.join("history")).unwrap();
    store.create_take("t1", "2026-11-23T00:00:00Z").unwrap();
    store
        .finish_take(
            "t1",
            1234,
            sotto_core::TakeStatus::Transcribed,
            None,
        )
        .unwrap();
    store
        .add_attempt(&sotto_core::store::Attempt {
            id: "a1".into(),
            take_id: "t1".into(),
            raw_text: "ThisTranscriptMustNotLeak".into(),
            corrected_text: Some("NorThisCorrectedText".into()),
            model: "parakeet-redux".into(),
            decode_ms: 10,
            corrections_json: "[]".into(),
            created_at: "2026-11-23T00:00:00Z".into(),
        })
        .unwrap();
    drop(store);

    let app = sotto::pipeline::App::new(&root, root.join("model"));
    (Arc::new(app), root)
}

/// Every string in a JSON tree, with the key path that led to it.
fn strings(v: &serde_json::Value, path: &str, out: &mut Vec<(String, String)>) {
    match v {
        serde_json::Value::String(s) => out.push((path.to_string(), s.clone())),
        serde_json::Value::Object(map) => {
            for (k, v) in map {
                strings(v, &format!("{path}.{k}"), out);
            }
        }
        serde_json::Value::Array(items) => {
            for (i, v) in items.iter().enumerate() {
                strings(v, &format!("{path}[{i}]"), out);
            }
        }
        _ => {}
    }
}

#[test]
fn the_report_contains_no_paths_transcripts_or_secrets() {
    let (app, root) = app_with_content("leak");
    let report = sotto::pipeline::diagnostics_report(&app);
    let json = serde_json::to_string_pretty(&report).unwrap();

    let mut all = Vec::new();
    strings(&report, "", &mut all);
    assert!(!all.is_empty(), "the report should contain something");

    // 1. No absolute paths, anywhere. A path carries the username and layout.
    for (key, value) in &all {
        assert!(
            !value.starts_with('/'),
            "an absolute path leaked into the diagnostics at {key}: {value}"
        );
        assert!(
            !value.contains("/home/") && !value.contains("/.local/"),
            "a home path leaked into the diagnostics at {key}: {value}"
        );
    }

    // The temp root is an absolute path, so it must not appear at all.
    let root_str = root.display().to_string();
    assert!(
        !json.contains(&root_str),
        "the data root leaked into the diagnostics"
    );

    // 2. No transcript text, raw or corrected.
    for needle in [
        "ThisTranscriptMustNotLeak",
        "NorThisCorrectedText",
    ] {
        assert!(
            !json.contains(needle),
            "transcript text leaked into the diagnostics: {needle}"
        );
    }

    // 3. No dictionary contents. A preferred spelling is personal.
    assert!(
        !json.contains("SottoSecretWord"),
        "a dictionary entry leaked into the diagnostics"
    );

    // 4. It must still be useful: the build and the counts are the point.
    assert_eq!(report["app"], "sotto");
    assert_eq!(report["storage"]["history_entries"], 1);
    assert_eq!(report["dictionary"]["entries"], 1);
    assert!(report["version"].is_string());
    assert!(report["model"]["present"].is_boolean());

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_report_names_the_model_without_saying_where_it_is() {
    let (app, root) = app_with_content("modelname");
    let report = sotto::pipeline::diagnostics_report(&app);

    // The basename is enough to know which model is loaded.
    let name = report["model"]["name"].as_str().expect("model name");
    assert_eq!(name, "model", "the model should be named by basename only");

    // And the field that used to carry the path is gone, not merely renamed.
    assert!(
        report["model"]["directory"].is_null(),
        "the absolute model directory must not be reported"
    );

    let _ = std::fs::remove_dir_all(&root);
}
