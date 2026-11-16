//! `.sta` ("SUMMARY OF JOB INFORMATION") parser.

use super::{complete_lines, float_token};

#[derive(Debug, Clone, PartialEq)]
pub struct StaRow {
    pub step: u32,
    pub inc: u32,
    pub att: u32,
    pub itrs: u32,
    pub tot_time: f64,
    pub step_time: f64,
    pub inc_time: f64,
}

/// Parses `.sta` text. Only lines with exactly 7 tokens and an integer first token are rows;
/// headers (also repeated ones) and a trailing partial line are skipped. Unparsable floats become NaN.
pub fn parse_sta(text: &str) -> Vec<StaRow> {
    complete_lines(text).filter_map(parse_line).collect()
}

fn parse_line(line: &str) -> Option<StaRow> {
    let t: Vec<&str> = line.split_whitespace().collect();
    if t.len() != 7 {
        return None;
    }
    let f = |i: usize| float_token(t[i]).unwrap_or(f64::NAN);
    Some(StaRow {
        step: t[0].parse().ok()?,
        inc: t[1].parse().ok()?,
        att: t[2].parse().ok()?,
        itrs: t[3].parse().ok()?,
        tot_time: f(4),
        step_time: f(5),
        inc_time: f(6),
    })
}
