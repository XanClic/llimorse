//! A very nice work buddy, taking care of your tickets for you. More or less.

#![warn(missing_docs)]
#![warn(clippy::missing_docs_in_private_items)]

mod tools;

use anyhow::{Context, Result, anyhow};
use chrono::format::SecondsFormat;
use chrono::{Datelike, Local};
use clap::{CommandFactory, FromArgMatches, Parser};
use llimo_chat::log::SessionLog;
use std::fs;
use std::path::PathBuf;
use term_ui::TermUi;

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

    let session_log_file = if let Some(session_log_dir) = args.session_logs {
        let now = Local::now().format("%Y-%m-%dT%H_%M_%S.json").to_string();
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

    let llm = llimo::Client::new(&args.llama_url);
    let mut agent = llimo::Agent::new_with_listener(llm, session_log_file);

    if let Some(history) = &resume_history {
        agent.push_history(history.clone());
    } else if let Some(system_prompt) = system_prompt {
        agent.push_system(system_prompt);
    }

    agent.add_tool(llimo::tools::WebSearch::new(&args.searxng_url));

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
        llimo_chat::App::new_with_history(agent, &resume_history, |history| {
            Ok(TermUi::new(history))
        })?
    } else {
        llimo_chat::App::new(agent, |history| Ok(TermUi::new(history)))?
    };

    app.run().await
}
