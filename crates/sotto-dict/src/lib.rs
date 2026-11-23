//! Sotto dictionary engine.
//!
//! Deterministic post-correction of speech-recognition output. This module exists
//! because the chosen runtime (`voxrt`) exposes no decoder biasing: we cannot make
//! the model hear a name correctly, so we repair the transcript afterwards.
//!
//! Four tiers are applied, in order, exactly once, with no recursion:
//!
//! 1. **Explicit replacement** — literal source → literal target, boundary-aware.
//! 2. **Preferred spelling** — exact alias → preferred form (NFC + casefold).
//! 3. **Fuzzy** — normalised Levenshtein distance, opt-in.
//! 4. **Phonetic** — Double Metaphone key equality, opt-in.
//!
//! Safety rules that are not optional:
//! - never touch a token shorter than [`MIN_FUZZY_LEN`] code points in tiers 3/4;
//! - reject ambiguous matches (two entries matching equally well → change nothing);
//! - protect URLs, emails, code spans and digit-bearing identifiers;
//! - never rewrite text produced by an earlier tier.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use thiserror::Error;
use unicode_normalization::UnicodeNormalization;

/// Shortest token (in code points) that tiers 3 and 4 are allowed to rewrite.
pub const MIN_FUZZY_LEN: usize = 4;

/// Default maximum normalised Levenshtein distance for tier 3.
pub const DEFAULT_FUZZY_THRESHOLD: f64 = 0.18;

/// How a replacement rule decides its match boundaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Boundary {
    /// Match on word boundaries only (default).
    #[default]
    Word,
    /// Match anywhere, including inside other words.
    Literal,
}

/// How case is handled when a rule matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CasePolicy {
    /// Always emit the preferred form exactly as written. This is the default,
    /// because a dictionary entry exists precisely because its spelling is the
    /// correct one.
    #[default]
    Exact,
    /// Mirror the original's casing: all-lowercase input yields a lowercase
    /// correction, ALL-CAPS input yields an uppercase one, anything else yields
    /// the preferred form as written.
    Preserve,
}

/// A dictionary term with a preferred spelling and zero or more aliases.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub id: String,
    /// The form that should appear in the transcript.
    pub preferred: String,
    /// Other spellings that should be rewritten to `preferred`.
    #[serde(default)]
    pub aliases: Vec<String>,
    /// Optional BCP-47-ish tag; informational only, never gates matching.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(default)]
    pub case_policy: CasePolicy,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

/// A literal find/replace rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Replacement {
    pub id: String,
    pub source: String,
    pub target: String,
    #[serde(default)]
    pub boundary: Boundary,
    #[serde(default)]
    pub case_sensitive: bool,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Higher priority wins when two rules match the same span.
    #[serde(default)]
    pub priority: i32,
}

/// Whole dictionary: entries plus replacements plus tier switches.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Dictionary {
    #[serde(default)]
    pub entries: Vec<Entry>,
    #[serde(default)]
    pub replacements: Vec<Replacement>,
    /// Tier 3 (fuzzy) enabled. Off by default; see plan §5.3.
    #[serde(default)]
    pub fuzzy_enabled: bool,
    /// Tier 4 (phonetic) enabled. Off by default; see plan §5.3.
    #[serde(default)]
    pub phonetic_enabled: bool,
    #[serde(default = "default_threshold")]
    pub fuzzy_threshold: f64,
}

fn default_threshold() -> f64 {
    DEFAULT_FUZZY_THRESHOLD
}

impl Default for Dictionary {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            replacements: Vec::new(),
            fuzzy_enabled: false,
            phonetic_enabled: false,
            fuzzy_threshold: DEFAULT_FUZZY_THRESHOLD,
        }
    }
}

/// Which tier produced a correction. Surfaced in the UI test panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    Replacement,
    Preferred,
    Fuzzy,
    Phonetic,
}

/// One applied correction, for explainability.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Correction {
    /// Byte offset in the *input* string.
    pub start: usize,
    pub end: usize,
    pub original: String,
    pub replacement: String,
    pub tier: Tier,
    /// The id of the entry or replacement that fired.
    pub rule_id: String,
}

/// Result of correcting a transcript.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Corrected {
    pub text: String,
    pub corrections: Vec<Correction>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum DictError {
    #[error("duplicate dictionary term `{0}` (normalised)")]
    DuplicateTerm(String),
    #[error("entry `{0}` has an empty preferred form")]
    EmptyPreferred(String),
    #[error("replacement `{0}` has an empty source")]
    EmptySource(String),
    #[error("fuzzy threshold {0} is outside 0.0..=1.0")]
    BadThreshold(String),
}

impl Dictionary {
    /// Validate structural invariants. Case-only duplicates are allowed but
    /// reported by [`Self::duplicate_terms`] so the UI can ask the user.
    pub fn validate(&self) -> Result<(), DictError> {
        if !(0.0..=1.0).contains(&self.fuzzy_threshold) || self.fuzzy_threshold.is_nan() {
            return Err(DictError::BadThreshold(self.fuzzy_threshold.to_string()));
        }
        for e in &self.entries {
            if e.preferred.trim().is_empty() {
                return Err(DictError::EmptyPreferred(e.id.clone()));
            }
        }
        for r in &self.replacements {
            if r.source.is_empty() {
                return Err(DictError::EmptySource(r.id.clone()));
            }
        }
        Ok(())
    }

    /// Normalised terms that appear more than once, for UI conflict reporting.
    pub fn duplicate_terms(&self) -> Vec<String> {
        let mut seen = HashSet::new();
        let mut dups = Vec::new();
        for e in &self.entries {
            let k = fold(&e.preferred);
            if !seen.insert(k.clone()) {
                dups.push(k);
            }
            for a in &e.aliases {
                let k = fold(a);
                if !seen.insert(k.clone()) {
                    dups.push(k);
                }
            }
        }
        dups.sort();
        dups.dedup();
        dups
    }

    /// Apply all enabled tiers once, in order, without recursion.
    pub fn correct(&self, input: &str) -> Corrected {
        let protected = protected_ranges(input);
        let mut corrections: Vec<Correction> = Vec::new();
        let mut taken: Vec<(usize, usize)> = Vec::new();

        // Tier 1: explicit replacements (longest source first, then priority).
        let mut reps: Vec<&Replacement> = self.replacements.iter().filter(|r| r.enabled).collect();
        reps.sort_by(|a, b| {
            b.source
                .chars()
                .count()
                .cmp(&a.source.chars().count())
                .then(b.priority.cmp(&a.priority))
                .then(a.id.cmp(&b.id))
        });
        for r in reps {
            for (s, e) in find_literal(input, &r.source, r.boundary, r.case_sensitive) {
                if overlaps_any(s, e, &taken) || overlaps_any(s, e, &protected) {
                    continue;
                }
                taken.push((s, e));
                corrections.push(Correction {
                    start: s,
                    end: e,
                    original: input[s..e].to_string(),
                    replacement: r.target.clone(),
                    tier: Tier::Replacement,
                    rule_id: r.id.clone(),
                });
            }
        }

        // Tiers 2-4 operate on word spans. Tier 2 additionally matches
        // multi-word aliases ("abdur rahman" → "Abdul Rahman"), longest first.
        let mut entry_rules: Vec<(String, String, Tier)> = Vec::new();
        for e in self.entries.iter().filter(|e| e.enabled) {
            for a in &e.aliases {
                entry_rules.push((a.clone(), e.preferred.clone(), Tier::Preferred));
            }
        }
        if self.fuzzy_enabled {
            for e in self.entries.iter().filter(|e| e.enabled) {
                entry_rules.push((e.preferred.clone(), e.preferred.clone(), Tier::Fuzzy));
                for a in &e.aliases {
                    entry_rules.push((a.clone(), e.preferred.clone(), Tier::Fuzzy));
                }
            }
        }

        let words = word_spans(input);
        let max_alias_words = entry_rules
            .iter()
            .filter(|(_, _, t)| *t == Tier::Preferred)
            .map(|(a, _, _)| a.split_whitespace().count().max(1))
            .max()
            .unwrap_or(1);

        let mut wi = 0usize;
        while wi < words.len() {
            // Tier 2: longest multi-word exact (folded) alias match.
            let mut best2: Option<(usize, String, String)> = None; // (end_word_idx_exclusive, preferred, alias)
            for len in (1..=max_alias_words.min(words.len() - wi)).rev() {
                let s = words[wi].0;
                let e = words[wi + len - 1].1;
                if overlaps_any(s, e, &taken) || overlaps_any(s, e, &protected) {
                    continue;
                }
                let cand = fold(&input[s..e]);
                if cand.chars().count() < 2 {
                    continue;
                }
                for (alias, preferred, tier) in &entry_rules {
                    if *tier != Tier::Preferred {
                        continue;
                    }
                    if fold(alias) == cand {
                        best2 = Some((wi + len, preferred.clone(), alias.clone()));
                        break;
                    }
                }
                if best2.is_some() {
                    break;
                }
            }
            if let Some((next_wi, preferred, alias)) = best2 {
                let s = words[wi].0;
                let e = words[next_wi - 1].1;
                let token = &input[s..e];
                let out = apply_case(
                    token,
                    &preferred,
                    self.entries.iter().find(|x| x.preferred == preferred),
                );
                taken.push((s, e));
                corrections.push(Correction {
                    start: s,
                    end: e,
                    original: token.to_string(),
                    replacement: out,
                    tier: Tier::Preferred,
                    rule_id: alias,
                });
                wi = next_wi;
                continue;
            }

            // Tiers 3 and 4: single token only.
            let (s, e) = words[wi];
            wi += 1;
            if overlaps_any(s, e, &taken) || overlaps_any(s, e, &protected) {
                continue;
            }
            let token = &input[s..e];
            let ntok = fold(token);
            if ntok.chars().count() < 2 {
                continue;
            }
            let mut hit: Option<(String, Tier, String)> = None;

            // Tier 3: fuzzy, only when nothing exact matched.
            if hit.is_none() && self.fuzzy_enabled && ntok.chars().count() >= MIN_FUZZY_LEN {
                // Collect every candidate within the threshold, then decide.
                let mut cands: Vec<(f64, String, String)> = Vec::new();
                for (alias, preferred, tier) in &entry_rules {
                    if *tier != Tier::Fuzzy {
                        continue;
                    }
                    let na = fold(alias);
                    if na.chars().count() < MIN_FUZZY_LEN {
                        continue;
                    }
                    let d = norm_levenshtein(&ntok, &na);
                    if d > self.fuzzy_threshold {
                        continue;
                    }
                    cands.push((d, preferred.clone(), alias.clone()));
                }
                cands.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
                if let Some((best_d, _, _)) = cands.first() {
                    let best_d = *best_d;
                    let distinct: HashSet<&String> = cands
                        .iter()
                        .filter(|(d, _, _)| (*d - best_d).abs() < 1e-9)
                        .map(|(_, p, _)| p)
                        .collect();
                    // Ambiguity rejection: two different preferred forms tie.
                    if distinct.len() == 1 {
                        let (_, preferred, alias) = &cands[0];
                        hit = Some((preferred.clone(), Tier::Fuzzy, alias.clone()));
                    }
                }
            }

            // Tier 4: phonetic, only when still nothing matched.
            if hit.is_none() && self.phonetic_enabled && ntok.chars().count() >= MIN_FUZZY_LEN {
                let key = double_metaphone(&ntok);
                if let Some(key) = key {
                    let mut matches: Vec<(String, String)> = Vec::new();
                    for e in self.entries.iter().filter(|e| e.enabled) {
                        let mut cands = vec![e.preferred.clone()];
                        cands.extend(e.aliases.iter().cloned());
                        for c in cands {
                            let nc = fold(&c);
                            if nc.chars().count() < MIN_FUZZY_LEN {
                                continue;
                            }
                            if double_metaphone(&nc).as_deref() == Some(key.as_str()) {
                                matches.push((e.preferred.clone(), c));
                            }
                        }
                    }
                    // Ambiguity rejection: two different preferred forms → do nothing.
                    let distinct: HashSet<&String> = matches.iter().map(|(p, _)| p).collect();
                    if distinct.len() == 1 {
                        if let Some((preferred, alias)) = matches.first() {
                            hit = Some((preferred.clone(), Tier::Phonetic, alias.clone()));
                        }
                    }
                }
            }

            if let Some((preferred, tier, rule_id)) = hit {
                let out = apply_case(token, &preferred, self.entries.iter().find(|x| x.preferred == preferred));
                taken.push((s, e));
                corrections.push(Correction {
                    start: s,
                    end: e,
                    original: token.to_string(),
                    replacement: out,
                    tier,
                    rule_id,
                });
            }
        }

        corrections.sort_by_key(|c| c.start);
        let text = apply_corrections(input, &corrections);
        Corrected { text, corrections }
    }
}

fn apply_case(original: &str, preferred: &str, entry: Option<&Entry>) -> String {
    match entry.map(|e| e.case_policy) {
        Some(CasePolicy::Preserve) => {
            // Preserve: emit the preferred form as the user wrote it in the
            // dictionary. A dictionary entry exists precisely because the
            // preferred spelling is the correct one, so we do not lowercase
            // "Abdul Rahman" just because the recogniser heard it in lower case.
            // The one thing we do preserve is shouting: if the speaker's token
            // was ALL CAPS, keep the correction in caps.
            let letters: Vec<char> = original.chars().filter(|c| c.is_alphabetic()).collect();
            let all_upper = !letters.is_empty() && letters.iter().all(|c| c.is_uppercase());
            let all_lower = !letters.is_empty() && letters.iter().all(|c| c.is_lowercase());
            if all_upper && preferred.chars().any(|c| c.is_lowercase()) {
                preferred.to_uppercase()
            } else if all_lower {
                preferred.to_lowercase()
            } else {
                preferred.to_string()
            }
        }
        _ => preferred.to_string(),
    }
}

fn apply_corrections(input: &str, corrections: &[Correction]) -> String {
    let mut out = String::with_capacity(input.len());
    let mut cursor = 0usize;
    for c in corrections {
        if c.start < cursor {
            continue;
        }
        out.push_str(&input[cursor..c.start]);
        out.push_str(&c.replacement);
        cursor = c.end;
    }
    out.push_str(&input[cursor..]);
    out
}

fn overlaps_any(s: usize, e: usize, spans: &[(usize, usize)]) -> bool {
    spans.iter().any(|(a, b)| s < *b && *a < e)
}

/// Unicode NFC + case folding used for every comparison.
pub fn fold(s: &str) -> String {
    s.nfc().collect::<String>().to_lowercase()
}

/// Byte spans of word tokens in `input`.
pub fn word_spans(input: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut start: Option<usize> = None;
    for (i, ch) in input.char_indices() {
        if ch.is_alphanumeric() || ch == '\'' || ch == '\u{2019}' {
            if start.is_none() {
                start = Some(i);
            }
        } else if let Some(s) = start.take() {
            out.push((s, i));
        }
    }
    if let Some(s) = start {
        out.push((s, input.len()));
    }
    out
}

/// Spans that must not be rewritten unless a literal rule targets them.
pub fn protected_ranges(input: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let bytes = input.as_bytes();

    // URLs and emails: any run of non-space containing "://" or '@'.
    let mut i = 0usize;
    while i < input.len() {
        let ch = input[i..].chars().next().unwrap();
        if ch.is_whitespace() {
            i += ch.len_utf8();
            continue;
        }
        let s = i;
        while i < input.len() {
            let c = input[i..].chars().next().unwrap();
            if c.is_whitespace() {
                break;
            }
            i += c.len_utf8();
        }
        let tok = &input[s..i];
        if tok.contains("://") || tok.contains('@') || tok.starts_with("www.") {
            out.push((s, i));
        }
    }

    // Backtick code spans.
    let mut idx = 0usize;
    while let Some(rel) = input[idx..].find('`') {
        let open = idx + rel;
        if let Some(rel2) = input[open + 1..].find('`') {
            let close = open + 1 + rel2 + 1;
            out.push((open, close));
            idx = close;
        } else {
            break;
        }
    }

    // Digit-bearing identifiers: any token mixing letters and digits, or a pure
    // number with 4+ digits (years, IDs, versions).
    for (s, e) in word_spans(input) {
        let t = &input[s..e];
        let has_digit = t.chars().any(|c| c.is_ascii_digit());
        let has_alpha = t.chars().any(|c| c.is_alphabetic());
        let digits = t.chars().filter(|c| c.is_ascii_digit()).count();
        if (has_digit && has_alpha) || (has_digit && digits >= 4) {
            out.push((s, e));
        }
    }

    // Silence unused warning for `bytes` on some targets.
    let _ = bytes;
    out.sort();
    out
}

fn find_literal(
    input: &str,
    needle: &str,
    boundary: Boundary,
    case_sensitive: bool,
) -> Vec<(usize, usize)> {
    if needle.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let hay: String;
    let ndl: String;
    if case_sensitive {
        hay = input.to_string();
        ndl = needle.to_string();
    } else {
        hay = input.to_lowercase();
        ndl = needle.to_lowercase();
    }
    let mut from = 0usize;
    while let Some(rel) = hay[from..].find(&ndl) {
        let s = from + rel;
        let e = s + ndl.len();
        let ok = match boundary {
            Boundary::Literal => true,
            Boundary::Word => {
                let before_ok = s == 0
                    || !hay[..s]
                        .chars()
                        .next_back()
                        .map(|c| c.is_alphanumeric())
                        .unwrap_or(false);
                let after_ok = e >= hay.len()
                    || !hay[e..]
                        .chars()
                        .next()
                        .map(|c| c.is_alphanumeric())
                        .unwrap_or(false);
                before_ok && after_ok
            }
        };
        if ok {
            out.push((s, e));
        }
        from = s + 1;
        if from >= hay.len() {
            break;
        }
    }
    out
}

/// Normalised Levenshtein distance: distance / max(code-point length).
pub fn norm_levenshtein(a: &str, b: &str) -> f64 {
    let ac: Vec<char> = a.chars().collect();
    let bc: Vec<char> = b.chars().collect();
    let max = ac.len().max(bc.len());
    if max == 0 {
        return 0.0;
    }
    levenshtein(&ac, &bc) as f64 / max as f64
}

fn levenshtein(a: &[char], b: &[char]) -> usize {
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = if ca == cb { 0 } else { 1 };
            cur[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// Simplified Double Metaphone key. Returns `None` for tokens with no
/// pronounceable letters. Only equality of keys is ever used.
pub fn double_metaphone(word: &str) -> Option<String> {
    let w: Vec<char> = word
        .chars()
        .filter(|c| c.is_alphabetic())
        .flat_map(|c| c.to_lowercase())
        .collect();
    if w.is_empty() {
        return None;
    }
    let mut key = String::new();

    // Keep the first letter, then encode.
    let first = w[0];
    key.push(first);
    let mut i = 1usize;

    while i < w.len() {
        let c = w[i];
        let prev = w[i - 1];
        let next = w.get(i + 1).copied();
        match c {
            'b' => {
                if prev != 'm' {
                    key.push('p');
                }
            }
            'p' | 'f' | 'v' => key.push('f'),
            'c' => {
                if next == Some('h') {
                    key.push('x');
                    i += 1;
                } else if matches!(next, Some('e') | Some('i') | Some('y')) {
                    key.push('s');
                } else {
                    key.push('k');
                }
            }
            'k' | 'q' => key.push('k'),
            'g' => {
                if next == Some('h') {
                    i += 1;
                } else if matches!(next, Some('e') | Some('i') | Some('y')) {
                    key.push('j');
                } else {
                    key.push('k');
                }
            }
            'j' => key.push('j'),
            's' => {
                if next == Some('h') {
                    key.push('x');
                    i += 1;
                } else {
                    key.push('s');
                }
            }
            'z' => key.push('s'),
            'x' => key.push('s'),
            'd' => {
                if next == Some('g') {
                    key.push('j');
                    i += 1;
                } else {
                    key.push('t');
                }
            }
            't' => {
                if next == Some('h') {
                    key.push('0');
                    i += 1;
                } else {
                    key.push('t');
                }
            }
            'h' => { /* silent */ }
            'm' | 'n' => key.push(c),
            'r' => key.push('r'),
            'l' => key.push('l'),
            'w' | 'y' => key.push(c),
            'a' | 'e' | 'i' | 'o' | 'u' => {
                if i == 0 {
                    key.push(c);
                }
            }
            _ => {}
        }
        i += 1;
    }

    // Collapse adjacent duplicates (Mohammed -> MHMT style).
    let mut collapsed = String::new();
    let mut last: Option<char> = None;
    for ch in key.chars() {
        if Some(ch) != last {
            collapsed.push(ch);
            last = Some(ch);
        }
    }
    if collapsed.is_empty() {
        None
    } else {
        Some(collapsed)
    }
}
