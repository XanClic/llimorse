//! Basic set of tools to be made available to harnesses written with llimorse

pub mod bash;
pub mod file;
pub mod subagent;
pub mod web_search;

pub use bash::Bash;
pub use file::{Edit, View, Write};
use std::any::Any;
use std::fmt::{Debug, Display};
pub use subagent::{Subagent, ToolFactory};
pub use web_search::WebSearch;

/// Pseudo-trait for gateable tool parameters
pub trait GateableToolParams: Any + Debug + Display {}

impl<T: Any + Debug + Display> GateableToolParams for T {}

/// Permission gate for tool call execution
#[allow(async_fn_in_trait)]
pub trait ToolGate: Debug + Send + Sync {
    /// Ask for permission to execute the given tool call.
    ///
    /// Rejection must be indicated by returning an error.
    async fn permitted(&self, params: &dyn GateableToolParams) -> std::result::Result<(), String>;
}

/// Permission gate that permits everything.
#[derive(Debug)]
pub struct AutoApprove;

#[allow(async_fn_in_trait)]
impl ToolGate for AutoApprove {
    async fn permitted(&self, _params: &dyn GateableToolParams) -> std::result::Result<(), String> {
        Ok(())
    }
}
