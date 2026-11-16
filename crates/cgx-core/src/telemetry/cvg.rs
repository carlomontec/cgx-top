//! `.cvg` ("SUMMARY OF C0NVERGENCE INFORMATION") parser.

use super::{complete_lines, float_token};

#[derive(Debug, Clone, PartialEq)]
pub struct CvgRow {
    pub step: u32,
    pub inc: u32,
    pub att: u32,
    pub iter: u32,
    pub contact_el: u64,
    pub resid_force: Option<f64>,
    pub corr_disp: Option<f64>,
    pub resid_flux: Option<f64>,
    pub corr_temp: Option<f64>,
}

/// Parses `.cvg` text. Only lines with exactly 9 tokens and an integer first token are rows
/// (this skips every header spelling, e.g. `C0NVERGENCE`, and a trailing partial line).
/// Float tokens that are `NaN`, infinite, contain `*` or fail to parse become `None`.
pub fn parse_cvg(text: &str) -> Vec<CvgRow> {
    complete_lines(text).filter_map(parse_line).collect()
}

fn parse_line(line: &str) -> Option<CvgRow> {
    let t: Vec<&str> = line.split_whitespace().collect();
    if t.len() != 9 {
        return None;
    }
    Some(CvgRow {
        step: t[0].parse().ok()?,
        inc: t[1].parse().ok()?,
        att: t[2].parse().ok()?,
        iter: t[3].parse().ok()?,
        contact_el: t[4].parse().ok()?,
        resid_force: float_token(t[5]),
        corr_disp: float_token(t[6]),
        resid_flux: float_token(t[7]),
        corr_temp: float_token(t[8]),
    })
}
