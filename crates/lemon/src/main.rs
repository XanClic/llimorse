//! Damnit, I just want a llama-server harness that works.

#![warn(missing_docs)]
#![warn(clippy::missing_docs_in_private_items)]

use anyhow::{Result, anyhow};
use chrono::format::SecondsFormat;
use chrono::{Datelike, Local};
use clap::Parser;
use helpers::macros::{Mergeable, derive_merge};
use llimorse_chat::log::SessionLog;
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::Arc;
use std::{env, fs};
use term_ui::TermUi;

/// Default llama-server URL
const LLAMA_URL_DEFAULT: &str = "http://127.0.0.1:8080";

/// Default log file name (in `$TMPDIR`)
const LOG_FILE_DEFAULT: &str = "lemon.log";

derive_merge! {
    /// Command-line arguments for Lemon
    #[derive(Parser, Deserialize)]
    #[serde(deny_unknown_fields, rename_all = "kebab-case")]
    struct Args {
        /// Config file path (containing base values for all of these arguments)
        #[arg(long)]
        #[serde(skip)]
        config: Option<PathBuf>,

        /// llama.cpp server base URL [default: http://127.0.0.1:8080]
        #[arg(long)]
        llama_url: Option<String>,

        /// Base URL of a SearXNG instance for the web_search tool
        #[arg(long)]
        searxng_url: Option<String>,

        /// Which model to use [default: "default"; or, if there is only one, that one]
        #[arg(long)]
        model: Option<String>,

        /// Path to a file containing the system prompt
        #[arg(long)]
        system: Option<PathBuf>,

        /// File to append log output to, because the terminal is taken by the UI
        /// [default: $TMPDIR/lemon.log]
        #[arg(long)]
        log_file: Option<PathBuf>,

        /// Where to store the raw session log for later resuming
        #[arg(long)]
        session_logs: Option<PathBuf>,

        /// Raw session log to resume from
        #[arg(long)]
        resume: Option<PathBuf>,
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = Args::parse();
    if let Some(file) = &args.config {
        let cfg = fs::read_to_string(file).map_err(|err| anyhow!("{}: {err}", file.display()))?;
        let cfg = toml::from_str(&cfg).map_err(|err| anyhow!("{}: {err}", file.display()))?;
        args.merge_weak(cfg);
    }

    let log_path = args
        .log_file
        .clone()
        .unwrap_or_else(|| env::temp_dir().join(LOG_FILE_DEFAULT));
    let log_file = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&log_path)
        .map_err(|err| anyhow!("{}: {err}", log_path.display()))?;

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(Arc::new(log_file))
        .with_ansi(false)
        .init();

    let system_prompt = args.system.map(fs::read_to_string).transpose()?;

    let session_log_file = if let Some(session_log_dir) = args.session_logs {
        let now = Local::now().format("%Y-%m-%dT%H_%M_%S.jsonl").to_string();
        let path = session_log_dir.join(now);
        SessionLog::new(&path).map_err(|err| anyhow!("{}: {err}", path.display()))?
    } else {
        SessionLog::null()
    };

    let resume_history = args
        .resume
        .map(|resume_from| {
            SessionLog::load(&resume_from)
                .map_err(|err| anyhow!("{}: {err}", resume_from.display()))
        })
        .transpose()?;

    let llm = llimorse::Client::new(
        args.llama_url.as_deref().unwrap_or(LLAMA_URL_DEFAULT),
        args.model.as_deref(),
    )
    .await?;
    let mut agent = llimorse::Agent::new_with_listener(llm, session_log_file);

    if let Some(history) = &resume_history {
        agent.push_history(history.clone());
    } else if let Some(system_prompt) = system_prompt {
        agent.push_system(system_prompt);
    }

    agent.add_tool(llimorse::tools::View::new());
    agent.add_tool(llimorse::tools::Write::new());
    agent.add_tool(llimorse::tools::Edit::new());

    if let Some(searxng_url) = &args.searxng_url {
        agent.add_tool(llimorse::tools::WebSearch::new(searxng_url));
    }

    // Push the current time and date
    if resume_history.is_none() {
        let now = Local::now();
        agent.push_system(format!(
            "The current date and time is {}, {}",
            now.weekday(),
            now.to_rfc3339_opts(SecondsFormat::Secs, false)
        ));
    }

    let mut app = if let Some(resume_history) = resume_history {
        llimorse_chat::App::new_with_history(agent, &resume_history, |agent, history| {
            Ok(TermUi::new(agent, history))
        })?
    } else {
        llimorse_chat::App::new(agent, |agent, history| Ok(TermUi::new(agent, history)))?
    };

    app.run().await
}
