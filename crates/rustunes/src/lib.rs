//! iPhone Music synchronization application and engine.
#![recursion_limit = "256"]
pub mod cli;
pub mod engine;

#[cfg(test)]
#[path = "../../../tests/support/transferred.rs"]
mod transferred_fixture;

#[cfg(test)]
mod test_fixtures;
