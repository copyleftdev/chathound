//! Shannon entropy instrumentation for chat streams.
//!
//! Three information-theoretic views per event, maintained incrementally:
//!
//! 1. `H_word` — lexical entropy of the word distribution (bits). High =
//!    diverse discussion; near zero = repetition (spam, one ranting user).
//! 2. `H_user_norm` — normalized speaker entropy H/log2(n): 1.0 = fully
//!    distributed crowd, ~0 = a monologue.
//! 3. `H_side` — entropy of the position-side distribution among chatters:
//!    ~1 bit = balanced YES/NO crowd, ~0 = one-sided echo chamber.
//!
//! All counters are bounded (vocabulary cap) and updated in O(1) per message.

use std::collections::HashMap;

pub const VOCAB_CAP: usize = 100_000;

/// Shannon entropy in bits from a count table. Exact over the f64 mass.
pub fn shannon<'a, I: Iterator<Item = (&'a String, &'a u32)>>(counts: I, total: u64) -> f64 {
    if total == 0 {
        return 0.0;
    }
    let t = total as f64;
    let mut h = 0.0f64;
    for (_, c) in counts {
        let p = *c as f64 / t;
        h -= p * p.log2();
    }
    h
}

#[derive(Debug)]
pub struct EntropyState {
    words: HashMap<String, u32>,
    users: HashMap<String, u32>,
    sides: HashMap<String, u32>,
    total_words: u64,
    total_msgs: u64,
    vocab_cap: usize,
}

impl Default for EntropyState {
    fn default() -> Self {
        Self {
            words: Default::default(),
            users: Default::default(),
            sides: Default::default(),
            total_words: 0,
            total_msgs: 0,
            vocab_cap: VOCAB_CAP,
        }
    }
}

impl EntropyState {
    /// Tokenize like a plain text scan: lowercase, alphanumeric runs, drop
    /// single characters (punctuation noise).
    pub fn tokenize(text: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut cur = String::new();
        for ch in text.chars().chain(std::iter::once(' ')) {
            if ch.is_alphanumeric() {
                cur.extend(ch.to_lowercase());
            } else if !cur.is_empty() {
                if cur.chars().count() > 1 {
                    out.push(std::mem::take(&mut cur));
                } else {
                    cur.clear();
                }
            }
        }
        out
    }

    /// Test/CLI constructor with a non-default vocabulary cap.
    pub fn with_cap(vocab_cap: usize) -> Self {
        Self {
            vocab_cap,
            ..Default::default()
        }
    }

    /// O(1) amortized update per message. Vocabulary is capped so a hostile
    /// chat cannot grow memory unbounded.
    pub fn update(&mut self, text: &str, user: &str, side: Option<&str>) {
        for w in Self::tokenize(text) {
            if self.words.len() >= self.vocab_cap && !self.words.contains_key(&w) {
                continue; // cap reached: count known words only
            }
            *self.words.entry(w).or_insert(0) += 1;
            self.total_words += 1;
        }
        *self.users.entry(user.to_string()).or_insert(0) += 1;
        *self
            .sides
            .entry(side.unwrap_or("none").to_string())
            .or_insert(0) += 1;
        self.total_msgs += 1;
    }

    /// (H_word bits, H_user normalized, H_side bits, msgs, words)
    pub fn report(&self) -> (f64, f64, f64, u64, u64) {
        let h_word = shannon(self.words.iter(), self.total_words);
        let n_users = self.users.len().max(1) as u64;
        let h_user = shannon(self.users.iter(), self.total_msgs);
        let denom = (n_users as f64).log2();
        let h_user_norm = if denom > 0.0 { h_user / denom } else { 0.0 };
        let h_side = shannon(self.sides.iter(), self.total_msgs);
        (
            h_word,
            h_user_norm,
            h_side,
            self.total_msgs,
            self.total_words,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uniform_distribution_is_log2_n() {
        let m: HashMap<String, u32> = [
            ("a".to_string(), 1u32),
            ("b".to_string(), 1),
            ("c".to_string(), 1),
            ("d".to_string(), 1),
        ]
        .into_iter()
        .collect();
        let h = shannon(m.iter(), 4);
        assert!(
            (h - 2.0).abs() < 1e-9,
            "uniform 4 should be 2 bits, got {h}"
        );
    }

    #[test]
    fn degenerate_distribution_is_zero() {
        let m: HashMap<String, u32> = [("x".to_string(), 10u32)].into_iter().collect();
        assert!((shannon(m.iter(), 10) - 0.0).abs() < 1e-9);
    }

    #[test]
    fn two_equal_symbols_is_one_bit() {
        let m: HashMap<String, u32> = [("heads".to_string(), 1u32), ("tails".to_string(), 1)]
            .into_iter()
            .collect();
        assert!((shannon(m.iter(), 2) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn monologue_has_zero_normalized_user_entropy() {
        let mut s = EntropyState::default();
        s.update("hello world hello", "one-user", Some("yes"));
        s.update("hello again world", "one-user", Some("yes"));
        let (_, h_user_norm, _, _, _) = s.report();
        assert!(
            h_user_norm < 1e-9,
            "one speaker must be 0 normalized, got {h_user_norm}"
        );
    }

    #[test]
    fn balanced_sides_is_one_bit() {
        let mut s = EntropyState::default();
        s.update("go winners", "u1", Some("yes"));
        s.update("no way", "u2", Some("no"));
        let (_, _, h_side, _, _) = s.report();
        assert!((h_side - 1.0).abs() < 1e-9);
    }

    #[test]
    fn one_sided_crowd_is_echo_chamber() {
        let mut s = EntropyState::default();
        for i in 0..5 {
            s.update("winning", &format!("u{i}"), Some("yes"));
        }
        let (_, _, h_side, _, _) = s.report();
        assert!(h_side < 1e-9);
    }

    #[test]
    fn tokenizer_drops_punct_and_single_chars() {
        let t = EntropyState::tokenize("GO go GO! a I ball-game 42");
        assert_eq!(t, vec!["go", "go", "go", "ball", "game", "42"]);
    }

    #[test]
    fn vocabulary_cap_is_respected() {
        // cap of 1: only the first-seen word family is counted, unknown
        // words are skipped, known words keep counting — no panic.
        let mut s = EntropyState::with_cap(1);
        s.update("seed brand-new-word", "u1", None);
        assert_eq!(s.words.len(), 1);
        assert_eq!(s.words["seed"], 1);
        s.update("seed", "u1", None);
        assert_eq!(s.words["seed"], 2);
        assert_eq!(s.words.len(), 1);
    }
}
