//! A very nice work buddy, taking care of your tickets for you. More or less.

#![warn(missing_docs)]
#![warn(clippy::missing_docs_in_private_items)]

mod macros;
mod tools;

use crate::macros::{Mergeable, derive_merge};
use anyhow::{Context, Result, anyhow};
use chrono::format::SecondsFormat;
use chrono::{Datelike, Local};
use clap::{CommandFactory, FromArgMatches, Parser};
use llimorse_chat::log::SessionLog;
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::Arc;
use std::{env, fs};
use term_ui::TermUi;

/// Default llama-server URL
const LLAMA_URL_DEFAULT: &str = "http://127.0.0.1:8080";

/// Default SearXNG URL
const SEARXNG_URL_DEFAULT: &str = "http://127.0.0.1:8888";

derive_merge! {
    /// Command-line arguments for WorkBuddy
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

        /// Base URL of a SearXNG instance for the web_search tool [default: http://127.0.0.1:8888]
        #[arg(long)]
        searxng_url: Option<String>,

        /// Path to a file containing the system prompt
        #[arg(long)]
        system: Option<PathBuf>,

        /// Enable debug-level logging
        #[arg(long)]
        #[serde(default)]
        debug: bool,

        /// File to append log output to, because the terminal is taken by the UI
        /// [default: $TMPDIR/work-buddy.log]
        #[arg(long)]
        log_file: Option<PathBuf>,

        /// Task file path
        #[arg(long)]
        tasks: Option<PathBuf>,

        /// Directory to store work logs in
        #[arg(long)]
        worklogs: Option<PathBuf>,

        /// Allow the LLM to create markdown files in this directory
        #[arg(long)]
        markdown_output: Option<PathBuf>,

        /// JSON knowledge file to explain keywords (e.g. projects) and such
        #[arg(long)]
        knowledge: Option<PathBuf>,

        /// Where to store the raw session log for later resuming
        #[arg(long)]
        session_logs: Option<PathBuf>,

        /// Raw session log to resume from
        #[arg(long)]
        resume: Option<PathBuf>,
    }
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
    let mut args = Args::from_arg_matches(
        &Args::command()
            .about(format!("WorkBuddy! …{}", tagline()))
            .get_matches(),
    )
    .unwrap();

    if let Some(file) = &args.config {
        let config =
            fs::read_to_string(file).map_err(|err| anyhow!("{}: {err}", file.display()))?;
        let cfg_args =
            toml::from_str(&config).map_err(|err| anyhow!("{}: {err}", file.display()))?;

        args.merge_weak(cfg_args);
    }

    let log_path = args
        .log_file
        .clone()
        .unwrap_or_else(|| env::temp_dir().join("work-buddy.log"));
    let log_file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|err| anyhow!("{}: {err}", log_path.display()))?;

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                if args.debug {
                    "info,work_buddy=debug"
                } else {
                    "info"
                }
                .into()
            }),
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

    let llm = llimorse::Client::new(args.llama_url.as_deref().unwrap_or(LLAMA_URL_DEFAULT));
    let mut agent = llimorse::Agent::new_with_listener(llm, session_log_file);

    if let Some(history) = &resume_history {
        agent.push_history(history.clone());
    } else if let Some(system_prompt) = system_prompt {
        agent.push_system(system_prompt);
    }

    agent.add_tool(llimorse::tools::WebSearch::new(
        args.searxng_url.as_deref().unwrap_or(SEARXNG_URL_DEFAULT),
    ));

    if let Some(task_file) = args.tasks {
        let task_file = tools::tasks::TaskFile::open(task_file.clone())
            .with_context(|| format!("{}", task_file.display()))?;

        if resume_history.is_none() {
            task_file.inject_active_tasks(&mut agent);
        }
        task_file.add_tools(&mut agent);
    }

    if let Some(worklog_dir) = args.worklogs {
        let worklog_dir = tools::worklog::WorklogDirectory::new(worklog_dir);
        worklog_dir.add_tools(&mut agent);
    }

    if let Some(markdown_dir) = args.markdown_output {
        agent.add_tool(tools::write_md::WriteMarkdown::new(markdown_dir));
    }

    if let Some(knowledge_file) = args.knowledge {
        let knowledge_file = tools::knowledge::KnowledgeFile::open(knowledge_file.clone())
            .with_context(|| format!("{}", knowledge_file.display()))?;

        knowledge_file.add_tools(&mut agent);
    }

    // Push the current time and date so the LLM knows what the timestamps mean
    let now = Local::now();
    agent.push_system(format!(
        "The current date and time is {}, {}",
        now.weekday(),
        now.to_rfc3339_opts(SecondsFormat::Secs, false)
    ));

    let mut app = if let Some(resume_history) = resume_history {
        llimorse_chat::App::new_with_history(agent, &resume_history, |history| {
            Ok(TermUi::new(history))
        })?
    } else {
        llimorse_chat::App::new(agent, |history| Ok(TermUi::new(history)))?
    };

    app.run().await
}
