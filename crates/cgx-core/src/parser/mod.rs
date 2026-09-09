//! FRD parser module supporting both ASCII and Binary (single/double precision) CalculiX result formats.

pub mod reader;
pub mod frd;
pub mod inp;

pub use reader::ByteCursor;
pub use frd::FrdParserEngine;
pub use inp::{
    consolidate_inp, inspect_inp, parse_sets_from_inp, parse_sets_from_str, load_companion_sets,
    InpConsolidateStats, InpInclude, InpSummary,
};

use std::path::Path;
use crate::{FrdModel, Result};

/// Convenience function to parse any CalculiX FRD model (ASCII, Binary Float, or Binary Double)
pub fn parse_frd_file<P: AsRef<Path>>(path: P) -> Result<FrdModel> {
    let path = path.as_ref();
    let file = std::fs::File::open(path)?;
    let mmap = unsafe { memmap2::Mmap::map(&file)? };

    let mut engine = FrdParserEngine::from_bytes(&mmap);
    let mut model = engine.parse_all()?;
    model.load_companion_sets(path);
    Ok(model)
}
