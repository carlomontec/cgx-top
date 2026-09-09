//! FRD parser module supporting both ASCII and Binary (single/double precision) CalculiX result formats.

pub mod reader;
pub mod frd;

pub use reader::ByteCursor;
pub use frd::FrdParserEngine;

use std::path::Path;
use crate::{FrdModel, Result};

/// Convenience function to parse any CalculiX FRD model (ASCII, Binary Float, or Binary Double)
pub fn parse_frd_file<P: AsRef<Path>>(path: P) -> Result<FrdModel> {
    let path = path.as_ref();
    let file = std::fs::File::open(path)?;
    let mmap = unsafe { memmap2::Mmap::map(&file)? };

    let mut engine = FrdParserEngine::from_bytes(&mmap);
    engine.parse_all()
}
