//! `cgx-core`: High-performance FEA data structures, invariants, and FRD parsing for CalculiX.

pub mod model;
pub mod invariants;
pub mod parser;

pub use model::*;
pub use invariants::*;
pub use parser::{
    consolidate_inp, inspect_inp, load_companion_sets, parse_frd_file, parse_sets_from_inp,
    parse_sets_from_str, ByteCursor, FrdParserEngine, InpConsolidateStats, InpInclude, InpSummary,
};

use thiserror::Error;

#[derive(Error, Debug)]
pub enum FrdError {
    #[error("I/O error reading FRD file: {0}")]
    Io(#[from] std::io::Error),
    #[error("Invalid FRD format: {0}")]
    InvalidFormat(String),
    #[error("Dataset or block not found: {0}")]
    NotFound(String),
    #[error("Parsing number failed: {0}")]
    ParseError(String),
}

pub type Result<T> = std::result::Result<T, FrdError>;

/// Common trait for CalculiX FRD file parsers (supporting both ASCII and Binary)
pub trait FrdParser {
    /// Quickly scan file to extract metadata, node count, element count, and step index
    fn scan_metadata(&mut self) -> Result<()>;
    
    /// Parse all nodes
    fn parse_nodes(&mut self) -> Result<Vec<Node>>;
    
    /// Parse all elements
    fn parse_elements(&mut self) -> Result<Vec<Element>>;
    
    /// Parse all node and element sets
    fn parse_sets(&mut self) -> Result<Vec<Set>>;
    
    /// Hydrate dataset for a specific step index
    fn parse_step_datasets(&mut self, step_idx: usize) -> Result<Vec<Dataset>>;
}
