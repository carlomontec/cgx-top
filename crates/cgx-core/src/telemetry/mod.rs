//! CalculiX solver telemetry: `.sta` / `.cvg` parsing, tailing and convergence diagnosis.

mod cvg;
mod diagnose;
mod sta;

pub use cvg::{parse_cvg, CvgRow};
pub use diagnose::{diagnose, DiagCtx, Diagnosis, Finding, Severity, Thresholds, Verdict};
pub use sta::{parse_sta, StaRow};

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;

/// Returns the complete lines of `text` (a final line without `\n` is a partial write and is dropped).
pub(crate) fn complete_lines(text: &str) -> impl Iterator<Item = &str> {
    let end = text.rfind('\n').map_or(0, |i| i + 1);
    text[..end].lines()
}

/// Float token to `Option`: unparsable, non-finite, or containing `*` (Fortran overflow) gives `None`.
pub(crate) fn float_token(tok: &str) -> Option<f64> {
    if tok.contains('*') {
        return None;
    }
    tok.parse::<f64>().ok().filter(|v| v.is_finite())
}

/// Incremental reader for a growing text file (e.g. a `.sta` being appended by the solver).
#[derive(Debug, Clone, PartialEq)]
pub struct Tail {
    path: PathBuf,
    offset: u64,
}

impl Tail {
    pub fn new(path: PathBuf) -> Self {
        Tail { path, offset: 0 }
    }

    /// Returns the new text since the previous poll, up to and including the last `\n`.
    /// A missing file yields an empty string; a file shorter than the stored offset resets it to 0.
    pub fn poll(&mut self) -> std::io::Result<String> {
        let mut f = match File::open(&self.path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(String::new()),
            Err(e) => return Err(e),
        };
        let len = f.metadata()?.len();
        if len < self.offset {
            self.offset = 0;
        }
        f.seek(SeekFrom::Start(self.offset))?;
        let mut buf = Vec::new();
        f.read_to_end(&mut buf)?;
        let Some(last_nl) = buf.iter().rposition(|&b| b == b'\n') else {
            return Ok(String::new());
        };
        buf.truncate(last_nl + 1);
        self.offset += buf.len() as u64;
        Ok(String::from_utf8_lossy(&buf).into_owned())
    }
}
