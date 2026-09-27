//! Damnit, I just want a llama-server harness that works.

#![warn(missing_docs)]
#![warn(clippy::missing_docs_in_private_items)]

use anyhow::{Result, anyhow};
use chrono::format::SecondsFormat;
use chrono::{Datelike, Local};
use clap::Parser;
use helpers::macros::{Mergeable, derive_merge};
use llimorse_chat::log::{Resume, SessionManager};
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

        /// Raw session log to resume from. With no value, resumes the newest session log in
        /// `--session-logs`
        #[arg(long, num_args(0..=1), default_missing_value = "", value_parser = Resume::value_parser())]
        #[serde(skip)] // no sense allowing this in a config file
        resume: Option<Resume>,

        /// Auto-approve all tool calls
        #[arg(long)]
        zesty: bool,
    }
}

/// Strip a trailing file extension (e.g. `.gguf`) from a model name, so that the name shown to the
/// LLM does not look like a file path. The full name must be kept for API requests, which need the
/// exact model id.
fn display_model_name(name: &str) -> &str {
    match name.rfind('.') {
        Some(idx) if name[idx + 1..].chars().all(|c| c.is_ascii_alphanumeric()) => &name[..idx],
        _ => name,
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

    // Use the CLI's `--resume` if provided, otherwise default to Fresh.
    let resume = args.resume.unwrap_or(Resume::Fresh);
    let manager = SessionManager::new(args.session_logs.as_deref(), &resume)?;

    let llm = llimorse::Client::new(
        args.llama_url.as_deref().unwrap_or(LLAMA_URL_DEFAULT),
        args.model.as_deref(),
    )
    .await?;
    let mut agent = llimorse::Agent::new_with_listener(llm, manager.log);

    if !manager.history.is_empty() {
        agent.push_history(manager.history.clone());
    } else if let Some(system_prompt) = system_prompt {
        agent.push_system(system_prompt);
    }

    let ui_notifications = llimorse_chat::ui::NotificationChannel::new();

    agent.add_tool(llimorse_tools::View::new());
    agent.add_tool(llimorse_tools::Write::new());
    agent.add_tool(llimorse_tools::Edit::new());
    if args.zesty {
        agent.add_tool(llimorse_tools::Bash::new(llimorse_tools::AutoApprove));
    } else {
        agent.add_tool(llimorse_tools::Bash::new(llimorse_chat::UserToolGate::new(
            &ui_notifications,
        )));
    }

    if let Some(searxng_url) = &args.searxng_url {
        agent.add_tool(llimorse_tools::WebSearch::new(searxng_url));
    }

    // Push the current time and date
    if manager.history.is_empty() {
        let now = Local::now();
        let model_name = display_model_name(&agent.client_state().model_name);
        agent.push_system(format!(
            "The current date and time is {}, {}. Your model name is {2}, and you are running in the lemon harness. \
             Sign git commits with \"Co-Authored-by: {2} on lemon <lemon@localhost>\".",
            now.weekday(),
            now.to_rfc3339_opts(SecondsFormat::Secs, false),
            model_name,
        ));
    }

    let mut app = llimorse_chat::App::new_with_history(
        agent,
        &manager.history,
        ui_notifications,
        |agent, history| Ok(TermUi::new(agent, history)),
    )?;

    app.run().await
}

#[cfg(test)]
mod tests {
    use super::display_model_name;

    #[test]
    fn strips_trailing_extension() {
        assert_eq!(
            display_model_name("Qwen3.8-27B-UD-Q4_K_M.gguf"),
            "Qwen3.8-27B-UD-Q4_K_M"
        );
    }

    #[test]
    fn keeps_dots_inside_the_name() {
        assert_eq!(display_model_name("Qwen3.8-27B"), "Qwen3.8-27B");
    }

    #[test]
    fn no_dot_is_unchanged() {
        assert_eq!(display_model_name("default"), "default");
    }

    #[test]
    fn non_alphanumeric_suffix_is_kept() {
        assert_eq!(display_model_name("model.v2-beta"), "model.v2-beta");
    }
}
