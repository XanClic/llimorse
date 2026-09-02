//! Tools for interacting with a llama-server (llama.cpp).

#![warn(missing_docs)]
#![warn(clippy::missing_docs_in_private_items)]

pub mod agent;
pub mod client;
pub mod line_format;
pub mod prefill_instructions;
pub mod streaming_result;
pub mod tools;

pub use agent::{Agent, CallableTool, ChatListener};
pub use client::Client;
pub use streaming_result::StreamingChunk;

/// Define a tool.
///
/// Specifically, defines a tool with its parameters and a state object.  Users still need to
/// implement the [`CallableTool`] trait.  To add the tool to an agent, call [`Agent::add_tool()`]
/// with the `'state` type.
#[macro_export]
macro_rules! tool {
    (
        'name: $name:literal;

        #[doc = $desc:literal]
        $(#[$attr:meta])*
        'params: $vis:vis struct $param_name:ident {
            $(
                $(#[$id_attr:meta])*
                $identifier:ident: $type:ty,
            )*
        }

        $(#[$result_attr:meta])*
        'result: $result_vis:vis struct $result_name:ident {
            $(
                $(#[$result_id_attr:meta])*
                $result_identifier:ident: $result_type:ty,
            )*
        }

        $(#[$state_attr:meta])*
        'state: $state_vis:vis struct $type_name:ident {
            $(
                $(#[$state_id_attr:meta])*
                $state_identifier:ident: $state_type:ty,
            )*
        }
    ) => {
        #[doc = $desc]
        $(#[$attr])*
        #[derive(serde::Deserialize, schemars::JsonSchema)]
        $vis struct $param_name {
            $(
                $(#[$id_attr])*
                $identifier: $type,
            )*
        }

        $(#[$result_attr])*
        #[derive(serde::Deserialize, serde::Serialize)]
        $result_vis struct $result_name {
            $(
                $(#[$result_id_attr])*
                $result_identifier: $result_type,
            )*
        }

        $(#[$state_attr])*
        $state_vis struct $type_name {
            $(
                $(#[$state_id_attr])*
                $state_identifier: $state_type,
            )*
        }

        impl $crate::agent::Tool for $type_name {
            fn name(&self) -> String {
                $name.into()
            }

            fn description(&self) -> Option<String> {
                Some($desc.into())
            }

            fn schema(&self) -> schemars::Schema {
                schemars::schema_for!($param_name)
            }

            fn execute_unparsed<'a>(
                &'a self,
                arguments: &'a str,
            ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<String>> + 'a>> {
                Box::pin(async move {
                    let params: $param_name = serde_json::from_str(arguments)?;
                    let result: $result_name = <Self as $crate::agent::CallableTool>::execute(self, params).await?;
                    Ok(serde_json::to_string(&result)?.to_string())
                })
            }

            fn fmt_call_display(
                &self,
                f: &mut std::fmt::Formatter<'_>,
                arguments: &str,
            ) -> std::fmt::Result {
                let params: $param_name = match serde_json::from_str(arguments) {
                    Ok(parsed) => parsed,
                    Err(err) => return write!(f, "[failed to parse: {err}]"),
                };

                write!(f, "{params}")
            }

            fn fmt_call_result_display(
                &self,
                f: &mut std::fmt::Formatter<'_>,
                result: &str,
            ) -> std::fmt::Result {
                let result: $result_name = match serde_json::from_str(result) {
                    Ok(parsed) => parsed,
                    Err(err) => return write!(f, "[failed to parse: {err}]"),
                };

                write!(f, "{result}")
            }
        }

        impl $crate::agent::ToolState for $type_name {
            type ParamType = $param_name;
            type ResultType = $result_name;
        }
    }
}
