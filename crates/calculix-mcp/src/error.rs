//! Tool error model: every failure is a structured, actionable tool result (`isError: true`).

use rmcp::handler::server::tool::IntoCallToolResult;
use rmcp::model::{CallToolResponse, CallToolResult};
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct ToolError {
    pub code: &'static str,
    pub message: String,
    /// The next call to make, written imperatively
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    /// Valid alternatives for the offending argument (at most 10)
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub valid: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub valid_total: Option<usize>,
    pub retryable: bool,
}

impl ToolError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self { code, message: message.into(), hint: None, valid: Vec::new(), valid_total: None, retryable: false }
    }

    pub fn hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    /// Attach valid alternatives, keeping the first 10 and recording the total.
    pub fn valid(mut self, mut all: Vec<String>) -> Self {
        self.valid_total = Some(all.len());
        all.truncate(10);
        self.valid = all;
        self
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new("INVALID_ARGUMENT", message)
    }

    pub fn internal(message: impl std::fmt::Display) -> Self {
        Self::new("INTERNAL", message.to_string())
    }
}

impl IntoCallToolResult for ToolError {
    fn into_call_tool_result(self) -> Result<CallToolResponse, rmcp::ErrorData> {
        let value = serde_json::to_value(&self).unwrap_or_default();
        Ok(CallToolResult::structured_error(value).into())
    }
}

/// Rank `candidates` by closeness to `wanted`: exact (case-insensitive), prefix, substring,
/// edit distance <= 3, then alphabetical.
pub fn nearest(wanted: &str, candidates: impl Iterator<Item = String>) -> Vec<String> {
    let w = wanted.to_lowercase();
    let mut ranked: Vec<(u8, usize, String)> = candidates
        .map(|c| {
            let l = c.to_lowercase();
            let rank = if l == w {
                (0, 0)
            } else if l.starts_with(&w) || w.starts_with(&l) {
                (1, l.len())
            } else if l.contains(&w) || w.contains(&l) {
                (2, l.len())
            } else {
                match edit_distance(&l, &w) {
                    d if d <= 3 => (3, d),
                    _ => (4, 0),
                }
            };
            (rank.0, rank.1, c)
        })
        .collect();
    ranked.sort();
    ranked.into_iter().map(|r| r.2).collect()
}

fn edit_distance(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for i in 1..=a.len() {
        let mut prev = row[0];
        row[0] = i;
        for j in 1..=b.len() {
            let cur = row[j];
            row[j] = (prev + (a[i - 1] != b[j - 1]) as usize).min(row[j] + 1).min(row[j - 1] + 1);
            prev = cur;
        }
    }
    row[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_ranks_exact_then_prefix_then_substring_then_typo() {
        let names = ["BLADE_ROOT", "Nblade", "HUB", "BLADE", "BLAED", "ZZZ"].map(String::from);
        let got = nearest("blade", names.into_iter());
        assert_eq!(got, ["BLADE", "BLADE_ROOT", "Nblade", "BLAED", "HUB", "ZZZ"]);
    }

    #[test]
    fn valid_is_capped_at_ten_with_total() {
        let e = ToolError::new("X", "m").valid((0..25).map(|i| i.to_string()).collect());
        assert_eq!((e.valid.len(), e.valid_total), (10, Some(25)));
    }
}
