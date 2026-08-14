//! Tools for interacting with a llama-server (llama.cpp).

mod agent;
mod client;
mod line_format;
mod streaming_result;

pub use agent::Agent;
pub use client::Client;
pub use streaming_result::StreamingChunk;
