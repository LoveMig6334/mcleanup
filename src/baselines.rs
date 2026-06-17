//! Persisted per-section duration baselines for the progress display.
//!
//! Stored as a flat JSON object at `~/.cache/mcleanup/baselines.json` mapping
//! `"<phase>:<section>"` (e.g. `"exec:Homebrew cleanup"`) to expected seconds.
//! The map is numeric-only, so parse/emit are hand-rolled — no serde dependency.
//! Every operation is best-effort: a missing or corrupt file yields an empty
//! map, and a write failure is ignored (the data is purely cosmetic).

use std::collections::HashMap;
use std::path::PathBuf;

/// EMA weight on the previously-stored value when merging a new measurement.
const EMA_OLD_WEIGHT: f64 = 0.7;

pub struct Baselines {
    /// Expected seconds loaded from disk, keyed `"<phase>:<section>"`.
    stored: HashMap<String, f64>,
    /// Durations measured during this run, merged into `stored` on `save`.
    measured: HashMap<String, f64>,
}

fn key(phase: &str, section: &str) -> String {
    format!("{phase}:{section}")
}

impl Baselines {
    /// Expected seconds for a section, or `None` when no baseline exists yet.
    pub fn get(&self, phase: &str, section: &str) -> Option<f64> {
        self.stored.get(&key(phase, section)).copied()
    }

    /// Record a measured duration for this run (merged into storage on `save`).
    pub fn record(&mut self, phase: &str, section: &str, seconds: f64) {
        self.measured.insert(key(phase, section), seconds);
    }

    /// Merge this run's measurements into the stored map and write it back.
    /// Best-effort: directory/file errors are ignored.
    pub fn save(&self) {
        let merged = merge(&self.stored, &self.measured);
        let p = path();
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(&p, emit(&merged));
    }
}

/// Load baselines from disk; an absent or unparseable file yields an empty map.
pub fn load() -> Baselines {
    let stored = std::fs::read_to_string(path())
        .map(|s| parse(&s))
        .unwrap_or_default();
    Baselines {
        stored,
        measured: HashMap::new(),
    }
}

fn path() -> PathBuf {
    crate::fsutil::home().join(".cache/mcleanup/baselines.json")
}

/// Apply the EMA: `new = 0.7*old + 0.3*measured`; first-time keys store the
/// measurement directly. Unmeasured stored keys are preserved unchanged.
fn merge(stored: &HashMap<String, f64>, measured: &HashMap<String, f64>) -> HashMap<String, f64> {
    let mut out = stored.clone();
    for (k, &m) in measured {
        let v = match stored.get(k) {
            Some(&old) => EMA_OLD_WEIGHT * old + (1.0 - EMA_OLD_WEIGHT) * m,
            None => m,
        };
        out.insert(k.clone(), v);
    }
    out
}

/// Tolerant flat-map parser: scans for `"string" : number` pairs and ignores
/// everything else. Never panics; malformed input yields whatever pairs parsed.
fn parse(s: &str) -> HashMap<String, f64> {
    let mut map = HashMap::new();
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'"' {
            i += 1;
            continue;
        }
        i += 1; // past opening quote
        let kstart = i;
        while i < b.len() && b[i] != b'"' {
            i += 1;
        }
        if i >= b.len() {
            break;
        }
        let k = &s[kstart..i];
        i += 1; // past closing quote
        while i < b.len() && b[i] != b':' {
            i += 1;
        }
        if i >= b.len() {
            break;
        }
        i += 1; // past colon
        while i < b.len() && (b[i] as char).is_whitespace() {
            i += 1;
        }
        let nstart = i;
        while i < b.len() && matches!(b[i], b'0'..=b'9' | b'.' | b'-' | b'+' | b'e' | b'E') {
            i += 1;
        }
        if let Ok(v) = s[nstart..i].parse::<f64>() {
            map.insert(k.to_string(), v);
        }
    }
    map
}

/// Emit a stable (key-sorted) flat JSON object with 3-decimal values.
fn emit(map: &HashMap<String, f64>) -> String {
    let mut keys: Vec<&String> = map.keys().collect();
    keys.sort();
    let mut out = String::from("{\n");
    for (idx, k) in keys.iter().enumerate() {
        let comma = if idx + 1 < keys.len() { "," } else { "" };
        out.push_str(&format!("  {:?}: {:.3}{}\n", k, map[*k], comma));
    }
    out.push_str("}\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_joins_phase_and_section() {
        assert_eq!(key("exec", "Homebrew cleanup"), "exec:Homebrew cleanup");
    }

    #[test]
    fn record_does_not_affect_get_until_save() {
        let mut b = Baselines {
            stored: HashMap::new(),
            measured: HashMap::new(),
        };
        b.record("scan", "uv", 1.5);
        // `get` reads stored, not measured — so still None this run.
        assert_eq!(b.get("scan", "uv"), None);
    }

    #[test]
    fn get_reads_stored() {
        let mut stored = HashMap::new();
        stored.insert("exec:npm cache".to_string(), 0.4);
        let b = Baselines {
            stored,
            measured: HashMap::new(),
        };
        assert_eq!(b.get("exec", "npm cache"), Some(0.4));
        assert_eq!(b.get("exec", "missing"), None);
    }

    #[test]
    fn parse_emit_round_trip() {
        let mut m = HashMap::new();
        m.insert("exec:Homebrew cleanup".to_string(), 3.42);
        m.insert("scan:.DS_Store".to_string(), 2.987);
        let round = parse(&emit(&m));
        assert_eq!(round.len(), 2);
        assert!((round["exec:Homebrew cleanup"] - 3.42).abs() < 1e-6);
        assert!((round["scan:.DS_Store"] - 2.987).abs() < 1e-6);
    }

    #[test]
    fn parse_tolerates_garbage() {
        assert!(parse("").is_empty());
        assert!(parse("not json at all").is_empty());
        // A partial/truncated object still yields the pairs it could read.
        let m = parse("{ \"a:b\": 1.5, \"c:d\":");
        assert_eq!(m.get("a:b"), Some(&1.5));
    }

    #[test]
    fn merge_first_time_stores_measurement() {
        let stored = HashMap::new();
        let mut measured = HashMap::new();
        measured.insert("scan:uv".to_string(), 2.0);
        let out = merge(&stored, &measured);
        assert_eq!(out["scan:uv"], 2.0);
    }

    #[test]
    fn merge_applies_ema_to_existing() {
        let mut stored = HashMap::new();
        stored.insert("scan:uv".to_string(), 10.0);
        let mut measured = HashMap::new();
        measured.insert("scan:uv".to_string(), 20.0);
        let out = merge(&stored, &measured);
        // 0.7*10 + 0.3*20 = 13.0
        assert!((out["scan:uv"] - 13.0).abs() < 1e-9);
    }

    #[test]
    fn merge_preserves_unmeasured_keys() {
        let mut stored = HashMap::new();
        stored.insert("scan:uv".to_string(), 10.0);
        stored.insert("exec:npm cache".to_string(), 0.4);
        let mut measured = HashMap::new();
        measured.insert("scan:uv".to_string(), 20.0);
        let out = merge(&stored, &measured);
        assert_eq!(out["exec:npm cache"], 0.4);
    }
}
