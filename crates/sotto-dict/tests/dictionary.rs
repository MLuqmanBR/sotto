//! Acceptance tests for the dictionary engine (plan §5, §9 A08–A10).

use sotto_dict::{
    double_metaphone, fold, norm_levenshtein, Boundary, CasePolicy, Dictionary, Entry, Replacement,
    Tier, MIN_FUZZY_LEN,
};

fn entry(preferred: &str, aliases: &[&str]) -> Entry {
    Entry {
        id: preferred.to_lowercase(),
        preferred: preferred.to_string(),
        aliases: aliases.iter().map(|s| s.to_string()).collect(),
        language: None,
        case_policy: CasePolicy::Exact,
        enabled: true,
    }
}

fn dict_with(entries: Vec<Entry>) -> Dictionary {
    Dictionary {
        entries,
        ..Default::default()
    }
}

// ── Tier 1: explicit replacement ────────────────────────────────────────────

#[test]
fn replacement_word_boundary_does_not_hit_inside_words() {
    let d = Dictionary {
        replacements: vec![Replacement {
            id: "r1".into(),
            source: "code".into(),
            target: "Code".into(),
            boundary: Boundary::Word,
            case_sensitive: false,
            enabled: true,
            priority: 0,
        }],
        ..Default::default()
    };
    let out = d.correct("the code and barcode and decode");
    assert_eq!(out.text, "the Code and barcode and decode");
    assert_eq!(out.corrections.len(), 1);
}

#[test]
fn replacement_multiword_phrase() {
    let d = Dictionary {
        replacements: vec![Replacement {
            id: "r2".into(),
            source: "open code".into(),
            target: "OpenCode".into(),
            boundary: Boundary::Word,
            case_sensitive: false,
            enabled: true,
            priority: 0,
        }],
        ..Default::default()
    };
    let out = d.correct("I use open code daily");
    assert_eq!(out.text, "I use OpenCode daily");
}

#[test]
fn replacement_longest_source_wins() {
    let d = Dictionary {
        replacements: vec![
            Replacement {
                id: "short".into(),
                source: "open".into(),
                target: "Open".into(),
                boundary: Boundary::Word,
                case_sensitive: false,
                enabled: true,
                priority: 0,
            },
            Replacement {
                id: "long".into(),
                source: "open code".into(),
                target: "OpenCode".into(),
                boundary: Boundary::Word,
                case_sensitive: false,
                enabled: true,
                priority: 0,
            },
        ],
        ..Default::default()
    };
    let out = d.correct("open code");
    assert_eq!(out.text, "OpenCode");
    assert_eq!(out.corrections.len(), 1);
    assert_eq!(out.corrections[0].rule_id, "long");
}

#[test]
fn replacement_priority_breaks_ties() {
    let d = Dictionary {
        replacements: vec![
            Replacement {
                id: "low".into(),
                source: "cat".into(),
                target: "LOW".into(),
                boundary: Boundary::Word,
                case_sensitive: false,
                enabled: true,
                priority: 0,
            },
            Replacement {
                id: "high".into(),
                source: "cat".into(),
                target: "HIGH".into(),
                boundary: Boundary::Word,
                case_sensitive: false,
                enabled: true,
                priority: 5,
            },
        ],
        ..Default::default()
    };
    let out = d.correct("the cat");
    assert_eq!(out.text, "the HIGH");
}

#[test]
fn disabled_replacement_is_ignored() {
    let d = Dictionary {
        replacements: vec![Replacement {
            id: "r".into(),
            source: "cat".into(),
            target: "dog".into(),
            boundary: Boundary::Word,
            case_sensitive: false,
            enabled: false,
            priority: 0,
        }],
        ..Default::default()
    };
    assert_eq!(d.correct("the cat").text, "the cat");
}

#[test]
fn case_sensitive_replacement() {
    let d = Dictionary {
        replacements: vec![Replacement {
            id: "r".into(),
            source: "IT".into(),
            target: "Information Technology".into(),
            boundary: Boundary::Word,
            case_sensitive: true,
            enabled: true,
            priority: 0,
        }],
        ..Default::default()
    };
    assert_eq!(d.correct("IT works").text, "Information Technology works");
    assert_eq!(d.correct("it works").text, "it works");
}

// ── Tier 2: preferred spelling ──────────────────────────────────────────────

#[test]
fn preferred_spelling_exact_alias() {
    let d = dict_with(vec![entry("Luqman", &["Lukman", "Lukmaan"])]);
    let out = d.correct("ask Lukman about it");
    assert_eq!(out.text, "ask Luqman about it");
    assert_eq!(out.corrections[0].tier, Tier::Preferred);
}

#[test]
fn preferred_spelling_is_case_insensitive() {
    let d = dict_with(vec![entry("Luqman", &["lukman"])]);
    // Default policy is Exact: the dictionary's spelling wins regardless of input case.
    assert_eq!(d.correct("ask LUKMAN").text, "ask Luqman");
    assert_eq!(d.correct("ask lukman").text, "ask Luqman");
}

#[test]
fn preferred_spelling_multiword_alias() {
    let d = dict_with(vec![entry("Abdul Rahman", &["abdur rahman"])]);
    assert_eq!(
        d.correct("call abdur rahman now").text,
        "call Abdul Rahman now"
    );
}

#[test]
fn disabled_entry_is_ignored() {
    let mut e = entry("Luqman", &["Lukman"]);
    e.enabled = false;
    let d = dict_with(vec![e]);
    assert_eq!(d.correct("ask Lukman").text, "ask Lukman");
}

#[test]
fn no_recursion_preferred_tier() {
    // "cat" -> "dog" and "dog" -> "cow": a single pass must yield "dog", not "cow".
    let d = dict_with(vec![entry("dog", &["cat"]), entry("cow", &["dog"])]);
    let out = d.correct("cat");
    assert_eq!(out.text, "dog");
}

#[test]
fn single_characters_are_never_rewritten() {
    // Deliberate safety rail: one-character tokens carry too little signal.
    let d = dict_with(vec![entry("I", &["a"])]);
    assert_eq!(d.correct("a").text, "a");
}

// ── Tier 3: fuzzy ───────────────────────────────────────────────────────────

#[test]
fn fuzzy_off_by_default() {
    let d = dict_with(vec![entry("Luqman", &["Luqman"])]);
    assert!(!d.fuzzy_enabled);
    assert_eq!(d.correct("Lukman").text, "Lukman");
}

#[test]
fn fuzzy_catches_single_char_slip() {
    let d = Dictionary {
        entries: vec![entry("Luqman", &["Luqman"])],
        fuzzy_enabled: true,
        ..Default::default()
    };
    let out = d.correct("ask Lukman");
    assert_eq!(out.text, "ask Luqman");
    assert_eq!(out.corrections[0].tier, Tier::Fuzzy);
}

#[test]
fn fuzzy_respects_threshold() {
    // "Mohammed" -> "Muhammad" is 0.25, above the 0.18 default.
    let d = Dictionary {
        entries: vec![entry("Muhammad", &["Muhammad"])],
        fuzzy_enabled: true,
        ..Default::default()
    };
    assert_eq!(d.correct("Mohammed").text, "Mohammed");
    assert!(norm_levenshtein("mohammed", "muhammad") > 0.18);
}

#[test]
fn fuzzy_never_touches_short_tokens() {
    let d = Dictionary {
        entries: vec![entry("Ali", &["Ali"])],
        fuzzy_enabled: true,
        ..Default::default()
    };
    // "Ana" is 3 code points, below MIN_FUZZY_LEN.
    assert!("ana".chars().count() < MIN_FUZZY_LEN);
    assert_eq!(d.correct("Ana").text, "Ana");
}

#[test]
fn fuzzy_rejects_ambiguous_matches() {
    // "kareex" is exactly 1 edit from both "kareem" and "kareef", which are two
    // different preferred forms. Ambiguity must block the rewrite.
    let d = Dictionary {
        entries: vec![entry("Kareem", &["Kareem"]), entry("Kareef", &["Kareef"])],
        fuzzy_enabled: true,
        fuzzy_threshold: 0.4,
        ..Default::default()
    };
    let out = d.correct("kareex");
    assert_eq!(out.text, "kareex");
    assert!(out.corrections.is_empty());
}

#[test]
fn fuzzy_picks_the_strictly_closer_match() {
    // When one candidate is strictly closer, it wins and is not ambiguous.
    let d = Dictionary {
        entries: vec![entry("Kareem", &["Kareem"]), entry("Karim", &["Karim"])],
        fuzzy_enabled: true,
        ..Default::default()
    };
    assert_eq!(d.correct("kareen").text, "Kareem");
}

// ── Tier 4: phonetic ────────────────────────────────────────────────────────

#[test]
fn phonetic_off_by_default() {
    let d = dict_with(vec![entry("Muhammad", &["Muhammad"])]);
    assert!(!d.phonetic_enabled);
    assert_eq!(d.correct("Mohammed").text, "Mohammed");
}

#[test]
fn phonetic_catches_transliteration_variants() {
    let d = Dictionary {
        entries: vec![entry("Muhammad", &["Muhammad"])],
        phonetic_enabled: true,
        ..Default::default()
    };
    let out = d.correct("ask Mohammed");
    assert_eq!(out.text, "ask Muhammad");
    assert_eq!(out.corrections[0].tier, Tier::Phonetic);
}

#[test]
fn phonetic_abdul_abdal() {
    let d = Dictionary {
        entries: vec![entry("Abdul", &["Abdul"])],
        phonetic_enabled: true,
        ..Default::default()
    };
    assert_eq!(d.correct("Abdal").text, "Abdul");
}

#[test]
fn phonetic_rejects_ambiguous_entries() {
    // Two different preferred forms with the same phonetic key.
    let d = Dictionary {
        entries: vec![entry("Muhammad", &["Muhammad"]), entry("Mohammed", &["Mohammed"])],
        phonetic_enabled: true,
        ..Default::default()
    };
    let out = d.correct("Mohamad");
    assert_eq!(out.text, "Mohamad");
    assert!(out.corrections.is_empty());
}

#[test]
fn phonetic_key_matches_known_pairs() {
    let a = double_metaphone("mohammed").unwrap();
    let b = double_metaphone("muhammad").unwrap();
    assert_eq!(a, b, "Mohammed/Muhammad must share a key");

    let c = double_metaphone("abdul").unwrap();
    let d2 = double_metaphone("abdal").unwrap();
    assert_eq!(c, d2, "Abdul/Abdal must share a key");

    // Unrelated words must not collide.
    assert_ne!(
        double_metaphone("table").unwrap(),
        double_metaphone("chair").unwrap()
    );
}

// ── Protected spans (A09) ───────────────────────────────────────────────────

#[test]
fn protects_urls() {
    let d = dict_with(vec![entry("Luqman", &["Lukman"])]);
    let out = d.correct("see https://example.com/Lukman for details");
    assert_eq!(out.text, "see https://example.com/Lukman for details");
    assert!(out.corrections.is_empty());
}

#[test]
fn protects_emails() {
    let d = dict_with(vec![entry("Luqman", &["Lukman"])]);
    let out = d.correct("mail Lukman@example.com now");
    assert_eq!(out.text, "mail Lukman@example.com now");
}

#[test]
fn protects_code_spans() {
    let d = Dictionary {
        replacements: vec![Replacement {
            id: "r".into(),
            source: "code".into(),
            target: "Code".into(),
            boundary: Boundary::Word,
            case_sensitive: false,
            enabled: true,
            priority: 0,
        }],
        ..Default::default()
    };
    let out = d.correct("run `the code` please");
    assert_eq!(out.text, "run `the code` please");
}

#[test]
fn protects_digit_bearing_identifiers() {
    let d = Dictionary {
        replacements: vec![Replacement {
            id: "r".into(),
            source: "abc".into(),
            target: "XYZ".into(),
            boundary: Boundary::Word,
            case_sensitive: false,
            enabled: true,
            priority: 0,
        }],
        ..Default::default()
    };
    // "abc123" contains digits and letters → protected.
    assert_eq!(d.correct("token abc123 here").text, "token abc123 here");
    // Plain "abc" is not protected.
    assert_eq!(d.correct("token abc here").text, "token XYZ here");
}

// ── Determinism and ordering (A10) ──────────────────────────────────────────

#[test]
fn ordering_is_deterministic_across_runs() {
    let d = Dictionary {
        entries: vec![entry("Luqman", &["Lukman"]), entry("Bilal", &["Bilaal"])],
        replacements: vec![Replacement {
            id: "r".into(),
            source: "code".into(),
            target: "Code".into(),
            boundary: Boundary::Word,
            case_sensitive: false,
            enabled: true,
            priority: 0,
        }],
        fuzzy_enabled: true,
        phonetic_enabled: true,
        ..Default::default()
    };
    let input = "Lukman and Bilaal met about the code";
    let first = d.correct(input).text;
    for _ in 0..50 {
        assert_eq!(d.correct(input).text, first);
    }
    assert_eq!(first, "Luqman and Bilal met about the Code");
}

#[test]
fn corrections_are_sorted_and_non_overlapping() {
    let d = Dictionary {
        entries: vec![entry("Luqman", &["Lukman"])],
        replacements: vec![Replacement {
            id: "r".into(),
            source: "hello".into(),
            target: "Hi".into(),
            boundary: Boundary::Word,
            case_sensitive: false,
            enabled: true,
            priority: 0,
        }],
        ..Default::default()
    };
    let out = d.correct("hello Lukman bye");
    let mut last = 0usize;
    for c in &out.corrections {
        assert!(c.start >= last, "corrections must be ordered");
        assert!(c.end > c.start);
        last = c.end;
    }
}

// ── Unicode ─────────────────────────────────────────────────────────────────

#[test]
fn unicode_nfc_normalisation() {
    // "é" as e + combining acute should fold identically to precomposed é.
    let decomposed = "cafe\u{0301}";
    let precomposed = "caf\u{00e9}";
    assert_eq!(fold(decomposed), fold(precomposed));
}

#[test]
fn unicode_preferred_form_preserved() {
    let d = dict_with(vec![entry("Zoë", &["Zoe"])]);
    assert_eq!(d.correct("ask Zoe").text, "ask Zoë");
}

// ── Validation ──────────────────────────────────────────────────────────────

#[test]
fn validate_rejects_empty_preferred() {
    let d = dict_with(vec![Entry {
        id: "x".into(),
        preferred: "  ".into(),
        aliases: vec![],
        language: None,
        case_policy: CasePolicy::Preserve,
        enabled: true,
    }]);
    assert!(d.validate().is_err());
}

#[test]
fn validate_rejects_bad_threshold() {
    let d = Dictionary {
        fuzzy_threshold: 1.5,
        ..Default::default()
    };
    assert!(d.validate().is_err());
}

#[test]
fn duplicate_terms_reported() {
    let d = dict_with(vec![entry("Luqman", &["Lukman"]), entry("Lukman", &["luqman"])]);
    let dups = d.duplicate_terms();
    assert!(!dups.is_empty(), "expected case-insensitive duplicate");
}

// ── Real observed error (from S0) ───────────────────────────────────────────

#[test]
fn repairs_real_s0_error_mohammed() {
    // S0 observed the model output "Mohamed" for "Mohammed".
    let d = Dictionary {
        entries: vec![entry("Mohammed", &["Mohammed"])],
        fuzzy_enabled: true,
        phonetic_enabled: true,
        ..Default::default()
    };
    let out = d.correct("schedule a meeting with Sarah and Mohamed");
    assert_eq!(out.text, "schedule a meeting with Sarah and Mohammed");
    assert!(out.corrections.iter().any(|c| c.original == "Mohamed"));
}

#[test]
fn does_not_damage_ordinary_english() {
    let d = Dictionary {
        entries: vec![
            entry("Luqman", &["Luqman"]),
            entry("Muhammad", &["Muhammad"]),
            entry("Bilal", &["Bilal"]),
        ],
        fuzzy_enabled: true,
        phonetic_enabled: true,
        ..Default::default()
    };
    let ordinary = "The quick brown fox jumps over the lazy dog. \
                    Please review the quarterly budget and discuss the new hiring plan \
                    for the engineering team before Tuesday afternoon.";
    let out = d.correct(ordinary);
    assert_eq!(out.text, ordinary, "ordinary English must be untouched");
    assert!(out.corrections.is_empty());
}
