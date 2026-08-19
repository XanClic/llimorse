//! A very nice work buddy, taking care of your tickets for you. More or less.

#![warn(missing_docs)]
#![warn(clippy::missing_docs_in_private_items)]

mod app;
mod tools;

use anyhow::{Context, Result};
use app::WorkBuddy;
use clap::{CommandFactory, FromArgMatches, Parser};
use std::fs;
use std::path::PathBuf;
use tools::TodoFile;

/// Command-line arguments for WorkBuddy
#[derive(Parser)]
struct Args {
    /// llama.cpp server base URL
    #[arg(long, default_value = "http://127.0.0.1:8080")]
    llama_url: String,

    /// Base URL of a SearXNG instance for the web_search tool
    #[arg(long, default_value = "http://127.0.0.1:8888")]
    searxng_url: String,

    /// Path to a file containing the system prompt
    #[arg(long)]
    system: Option<PathBuf>,

    /// Enable debug-level logging
    #[arg(long)]
    debug: bool,

    /// To-do file path
    #[arg(long)]
    todo: Option<PathBuf>,
}

/// Return a random “witty” tag line for --help
fn tagline() -> &'static str {
    const TAGLINES: [&str; 4] = [
        "because someone has to care about your tickets, and it won’t be you",
        "turning existential dread into well-formatted Jira tickets since 2026",
        "proof of work for proof of employment",
        "comprehensive documentation for the comprehensively unmotivated",
    ];

    TAGLINES[fastrand::usize(..TAGLINES.len())]
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::from_arg_matches(
        &Args::command()
            .about(format!("WorkBuddy! …{}", tagline()))
            .get_matches(),
    )
    .unwrap();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                if args.debug {
                    "work_buddy=debug"
                } else {
                    "work_buddy=info"
                }
                .into()
            }),
        )
        .init();

    let system_prompt = args.system.map(fs::read_to_string).transpose()?;

    let llm = llimo::Client::new(&args.llama_url);
    let mut agent = llimo::Agent::new(llm);

    agent.add_tool(llimo::tools::WebSearch::new(&args.searxng_url));
    if let Some(todo_file) = args.todo {
        let todo_file = TodoFile::open(todo_file.clone())
            .with_context(|| format!("{}", todo_file.display()))?;
        todo_file.add_tools(&mut agent);
    }

    if let Some(system_prompt) = system_prompt {
        agent.push_system(system_prompt);
    }

    WorkBuddy::new(agent).run().await
}
