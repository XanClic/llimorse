//! Tools for agentic use.

use anyhow::Result;
use llimo::CallableTool;

llimo::tool! {
    'name: "hello";

    /// Make the initial greeting to the user extra special! Use this to be super friendly.
    #[derive(Debug)]
    'params: pub struct HelloToolParams {
        /// Use this for an extra special tag line between friendly colleagues!
        #[allow(unused)]
        tagline: String,
    }

    #[derive(Clone, Debug, Default)]
    'state: pub struct HelloTool {}
}

impl CallableTool for HelloTool {
    fn execute(&self, arguments: HelloToolParams) -> Result<String> {
        eprintln!(
            "\n\x1b[31;1mA super special hello from the LLM: {}\x1b[0m\n",
            arguments.tagline
        );
        Ok("Got it!".into())
    }
}
