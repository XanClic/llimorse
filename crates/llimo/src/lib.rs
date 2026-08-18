//! Tools for interacting with a llama-server (llama.cpp).

#![warn(missing_docs)]
#![warn(clippy::missing_docs_in_private_items)]

pub mod agent;
pub mod client;
pub mod line_format;
pub mod streaming_result;

pub use agent::{Agent, CallableTool};
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

            fn execute_unparsed(
                &self,
                arguments: String,
            ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<String>> + '_>> {
                Box::pin(async move {
                    let params: $param_name = serde_json::from_str(&arguments)?;
                    let result = <Self as $crate::agent::CallableTool>::execute(self, params).await?;
                    Ok(result.to_string())
                })
            }
        }

        impl $crate::agent::ToolState for $type_name {
            type ParamType = $param_name;
        }
    }
}
