//! In-memory fakes for this role's ports. Append `pub mod` lines, never remove others'.

pub mod git;
pub mod parser;
pub mod scip;
pub mod search;

pub use scip::{FakeScipReader, ScipFixture, contract_scip_reader, sample_scip_fixture};
pub use search::{MemTextSearch, contract_text_search};
pub mod world;
