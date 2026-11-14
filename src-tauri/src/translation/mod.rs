//! Translation scheduler, cache keys, validation, placeholders and splitting (spec §6.3, §7).
pub mod cache_key;
pub mod placeholder;
pub mod prepare;
pub mod prompt;
pub mod service;
pub mod splitter;
pub mod validation;

#[cfg(test)]
mod service_tests;
