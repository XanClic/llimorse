//! Example tools to be made available to harnesses written with llimorse

pub mod file;
pub mod web_search;

pub use file::{Edit, View, Write};
pub use web_search::WebSearch;
