//! A very nice work buddy, taking care of your tickets for you. More or less.

#![warn(missing_docs)]
#![warn(clippy::missing_docs_in_private_items)]

mod llm;

use anyhow::Result;
use clap::{CommandFactory, FromArgMatches, Parser};
use futures::StreamExt;
use llm::StreamingChunk;
use std::io::{self, Write};

/// Command-line arguments for WorkBuddy
#[derive(Parser)]
struct Args {
    /// llama.cpp server base URL
    #[arg(long, default_value = "http://127.0.0.1:8080")]
    llama_url: String,

    /// Base URL of a SearXNG instance for the web_search tool
    #[arg(long, default_value = "http://127.0.0.1:8888")]
    searxng_url: String,

    /// Enable debug-level logging
    #[arg(long)]
    debug: bool,
}

/// Random a random “witty” tag line for --help
fn tagline() -> &'static str {
    const TAGLINES: [&str; 4] = [
        "because someone has to care about your tickets, and it won’t be you",
        "turning existential dread into well-formatted Jira tickets since 2026",
        "proof of work for proof of employment",
        "comprehensive documentation for the comprehensively unmotivated",
    ];

    TAGLINES[fastrand::usize(..TAGLINES.len())]
}

/// Tracks which mode we are in wrt the LLM output (to allow switching terminal colors).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum OutputMode {
    /// Default mode (CSI 0m)
    #[default]
    DefaultTerm,

    /// Proper LLM output
    Output,

    /// Reasoning content
    Thinking,
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

    let llm = llm::Client::new(&args.llama_url);
    let mut agent = llm::Agent::new(llm);

    agent.push_system("Who’s a friendly work buddy? You’re a friendly work buddy!");

    agent.push_user("Hallo!");
    loop {
        let mut result = agent.submit().await?;

        let mut output_mode = OutputMode::default();
        while let Some(chunk) = result.next().await {
            let chunk = chunk?;
            match chunk {
                StreamingChunk::Content(content) => {
                    output_mode.switch(OutputMode::Output);
                    output_mode.print(&content);
                }

                StreamingChunk::Reasoning(content) => {
                    output_mode.switch(OutputMode::Thinking);
                    output_mode.print(&content);
                }
            }
        }
        output_mode.switch(OutputMode::DefaultTerm);
        if !result.execute_pending_calls().await {
            break;
        }
    }

    Ok(())
}

impl OutputMode {
    /// Switch the currently active output mode.
    ///
    /// Internally checks whether `self == to`, so the caller does not need to do that.
    fn switch(&mut self, to: Self) {
        if *self == to {
            return;
        }

        if *self != OutputMode::DefaultTerm {
            print!("\n\x1b[0m");
        }

        match to {
            OutputMode::DefaultTerm => print!("\x1b[0m"),
            OutputMode::Output => println!("\x1b[1m"),
            OutputMode::Thinking => println!("\x1b[2;3mThinking:\n"),
        }
        let _ = io::stdout().flush();

        *self = to;
    }

    /// Writes `message` out.
    fn print(&self, message: &str) {
        print!("{message}");
        let _ = io::stdout().flush();
    }
}
