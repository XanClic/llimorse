//! Tools to manage the worklog

use anyhow::{Result, anyhow};
use chrono::format::SecondsFormat;
use chrono::{Datelike, Local, NaiveDate};
use llimo::CallableTool;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::{fmt, fs};
use tokio::sync::Mutex;

#[derive(Debug)]
pub struct WorklogDirectory {
    /// Base path of the directory containing the worklogs
    path: PathBuf,
}

/// A worklog entry
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
struct WorklogEntry {
    #[serde(flatten)]
    settable: WorklogSettableEntry,

    timestamp: String,
}

/// The LLM-settable part of a worklog entry
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
struct WorklogSettableEntry {
    summary: String,
    effort_minutes: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    components: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    root_cause: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    fix_approach: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    references: Vec<WorklogReference>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    tags: Vec<WorklogTag>,
    narrative: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case", tag = "type")]
enum WorklogReference {
    Issue(String),
    MergeRequest(String),
    Commit(String),
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum WorklogTag {
    Bug,
    Feature,
    Investigation,
    Chore,
}

impl WorklogDirectory {
    /// Create an instance storing logs in the given directory.
    pub fn new(path: PathBuf) -> Self {
        WorklogDirectory { path }
    }

    /// Add relevant tools for the worklog to `agent`.
    pub fn add_tools(self, agent: &mut llimo::Agent) {
        let this = Arc::new(Mutex::new(self));

        agent.add_tool(WorklogAdd::new(this));
    }

    fn push(&mut self, entry: WorklogSettableEntry) -> Result<()> {
        let now = Local::now();
        let entry = WorklogEntry {
            settable: entry,
            timestamp: now.to_rfc3339_opts(SecondsFormat::Secs, false),
        };

        let mut week = self.load_week(now.date_naive())?;
        week.push(entry);
        self.store_week(now.date_naive(), week)?;

        Ok(())
    }

    fn date_to_fname<D: Datelike>(date: D) -> String {
        let week = date.iso_week();
        format!("{}-w{}.json", week.year(), week.week())
    }

    fn load_week(&self, date: NaiveDate) -> Result<Vec<WorklogEntry>> {
        let path = self.path.join(Self::date_to_fname(date));
        let content = match fs::read_to_string(&path) {
            Ok(content) => content,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                let mut file = fs::File::create_new(&path).map_err(|c_err| {
                    anyhow!(
                        "{}: Opening weekly work log failed: {err}; and creating failed, too: {c_err}",
                        path.display(),
                    )
                })?;

                let content = String::from("[]");
                file.write_all(content.as_bytes())?;
                content
            }
            Err(err) => return Err(err.into()),
        };

        serde_json::from_str(&content).map_err(|err| {
            anyhow!(
                "{}: Failed to load week log from JSON: {err}",
                path.display()
            )
        })
    }

    fn store_week(&self, date: NaiveDate, week: Vec<WorklogEntry>) -> Result<()> {
        let path = self.path.join(Self::date_to_fname(date));

        let json = serde_json::to_string(&week)
            .map_err(|err| anyhow!("Failed to convert week log to JSON: {err}"))?;

        fs::write(&path, json)
            .map_err(|err| anyhow!("{}: Failed to write week log file: {err}", path.display()))?;

        Ok(())
    }
}

impl fmt::Display for WorklogEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} timestamp={}", self.settable, self.timestamp)
    }
}

impl fmt::Display for WorklogSettableEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "summary={:?} effort_mins={} ",
            self.summary, self.effort_minutes
        )?;
        if !self.components.is_empty() {
            write!(f, "components={:?} ", self.components)?;
        }
        if let Some(root_cause) = &self.root_cause {
            write!(f, "root_cause={root_cause:?} ")?;
        }
        if let Some(fix_approach) = &self.fix_approach {
            write!(f, "fix_approach={fix_approach:?} ")?;
        }
        if !self.references.is_empty() {
            write!(f, "refs={:?} ", self.references)?;
        }
        if !self.tags.is_empty() {
            write!(f, "tags={:?} ", self.tags)?;
        }
        if self.narrative.len() <= 50 {
            write!(f, "narrative={:?}", self.narrative)?;
        } else {
            write!(f, "narrative={:.49?}", self.narrative)?;
        }

        Ok(())
    }
}

llimo::tool! {
    'name: "worklog_add";

    /// Log work the user has done in the worklog.
    #[derive(Debug)]
    'params: pub struct WorklogAddParams {
        /// Worklog entry to add
        #[serde(flatten)]
        entry: WorklogSettableEntry,
    }

    /// Result of adding a worklog entry.
    #[derive(Debug)]
    'result: pub struct WorklogAddResult {
        /// Summary of the entry just added
        summary: String,
    }

    /// Log work the user has done in the worklog.
    #[derive(Debug)]
    'state: pub struct WorklogAdd {
        /// Underlying worklog “database”
        storage: Arc<Mutex<WorklogDirectory>>,
    }
}

impl std::fmt::Display for WorklogAddParams {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.entry)
    }
}

impl std::fmt::Display for WorklogAddResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "summary={:?}", self.summary)
    }
}

impl WorklogAdd {
    pub fn new(dir: Arc<Mutex<WorklogDirectory>>) -> Self {
        WorklogAdd { storage: dir }
    }
}

impl CallableTool for WorklogAdd {
    async fn execute(&self, params: WorklogAddParams) -> Result<WorklogAddResult> {
        let summary = params.entry.summary.clone();
        let mut storage = self.storage.lock().await;
        storage.push(params.entry)?;
        Ok(WorklogAddResult { summary })
    }
}
