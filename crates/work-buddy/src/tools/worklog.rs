//! Tools to manage the worklog

use anyhow::{Result, anyhow};
use chrono::format::SecondsFormat;
use chrono::{DateTime, Datelike, Days, FixedOffset, Local, NaiveDate};
use llimo::{Agent, CallableTool, ChatListener};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use std::{fmt, fs, io};
use tokio::sync::Mutex;

/// Directory in which the worklogs are stored
#[derive(Debug)]
pub struct WorklogDirectory {
    /// Base path of the directory containing the worklogs
    path: PathBuf,
}

/// A worklog entry
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
struct WorklogEntry {
    /// LLM-settable part of the entry
    #[serde(flatten)]
    settable: WorklogSettableEntry,

    /// When this entry was created
    timestamp: String,
}

/// The LLM-settable part of a worklog entry
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
struct WorklogSettableEntry {
    /// Summary of the work that was actually done in this chunk
    summary: String,

    /// How many minutes were spent, roughly, on this work item
    effort_minutes: usize,

    /// What components were touched, i.e. which projects were affected
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    components: Vec<String>,

    /// What is the underlying reason for working on this, what has caused the need to work on it;
    /// in case of bugs, what has the investigation unveiled to be the cause of the bug?
    #[serde(default, skip_serializing_if = "Option::is_none")]
    root_cause: Option<String>,

    /// What is the overarching approach being taken here, regarding the whole underlying problem?
    #[serde(default, skip_serializing_if = "Option::is_none")]
    fix_approach: Option<String>,

    /// Persistent and globally reachable references of the work: Issue tickets, merge requests,
    /// commits.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    references: Vec<WorklogReference>,

    /// Arbitrary tags you would like to give to this work item to better find it later
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    tags: Vec<WorklogTag>,

    /// A detailed explanation of what was done for this work item, capturing everything you have
    /// to offer
    narrative: String,
}

/// Persistent and globally reachable references of work
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case", tag = "type")]
#[schemars(inline)]
enum WorklogReference {
    /// Gitlab work items, github issues, Jira tickets, ...
    Issue {
        /// URL linking to the issue
        url: String,
    },

    /// Github pull requests, gitlab merge requests, ...
    MergeRequest {
        /// URL linking to the MR/PR
        url: String,
    },

    /// Commit in a repository noted in `components`
    Commit {
        /// The commit hash
        hash: String,
    },
}

/// Tags for worklog items
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(inline)]
enum WorklogTag {
    /// A bug to be fixed
    Bug,

    /// Code review
    Review,

    /// A new feature to be implemented
    Feature,

    /// Investigation for either the root cause of a bug or how to implement a new feature
    Investigation,

    /// Some general chore like refactoring
    Chore,

    /// A meeting with one or more other people
    Meeting,
}

impl WorklogDirectory {
    /// Create an instance storing logs in the given directory.
    pub fn new(path: PathBuf) -> Self {
        WorklogDirectory { path }
    }

    /// Add relevant tools for the worklog to `agent`.
    pub fn add_tools(self, agent: &mut Agent<impl ChatListener>) {
        let this = Arc::new(Mutex::new(self));

        agent.add_tool(WorklogAdd::new(Arc::clone(&this)));
        agent.add_tool(WorklogQuery::new(this));
    }

    /// Append `entry` to the worklog.
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

    /// Find the worklog filename corresponding to `date`.
    fn date_to_fname<D: Datelike>(date: D) -> String {
        let week = date.iso_week();
        format!("{}-w{}.json", week.year(), week.week())
    }

    /// Load the worklog entries for the week in which `date` is.
    fn load_week(&self, date: NaiveDate) -> Result<Vec<WorklogEntry>> {
        let path = self.path.join(Self::date_to_fname(date));
        let content = match fs::read_to_string(&path) {
            Ok(content) => content,
            Err(err) if err.kind() == io::ErrorKind::NotFound => String::from("[]"),
            Err(err) => return Err(err.into()),
        };

        serde_json::from_str(&content).map_err(|err| {
            anyhow!(
                "{}: Failed to load week log from JSON: {err}",
                path.display()
            )
        })
    }

    /// Store the worklog entries for the week in which `date` is.
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
            write!(f, "narrative=\"{}\"", self.narrative)?;
        } else {
            write!(f, "narrative=\"{:.49}…\"", self.narrative)?;
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
    /// Create a worklog_add tool for the given worklog directory.
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

llimo::tool! {
    'name: "worklog_query";

    /// Query the worklog for entries.
    #[derive(Debug)]
    'params: pub struct WorklogQueryParams {
        /// Date to start the search from (inclusive, YYYY-mm-dd)
        start_date: String,

        /// Date to end the search at (inclusive, YYYY-mm-dd)
        end_date: String,

        /// Return only entries that include any of these tags
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tags: Option<Vec<WorklogTag>>,

        /// Return only entries that touched any of these components
        #[serde(default, skip_serializing_if = "Option::is_none")]
        components: Option<Vec<String>>,

        /// Return only entries whose `summary` or `narrative` fields contain any of these strings
        #[serde(default, skip_serializing_if = "Option::is_none")]
        match_strings: Option<Vec<String>>,
    }

    /// Result of a worklog query.
    #[derive(Debug)]
    'result: pub struct WorklogQueryResult {
        /// Matching worklog entries
        matching: Vec<WorklogEntry>,
    }

    /// Query the worklog for entries.
    #[derive(Debug)]
    'state: pub struct WorklogQuery {
        /// Underlying worklog “database”
        storage: Arc<Mutex<WorklogDirectory>>,
    }
}

impl std::fmt::Display for WorklogQueryParams {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "dates=[{}..{}]", self.start_date, self.end_date)?;

        if let Some(tags) = &self.tags {
            write!(f, " tags={tags:?}")?;
        }
        if let Some(components) = &self.components {
            write!(f, " tags={components:?}")?;
        }
        if let Some(match_strings) = &self.match_strings {
            write!(f, " tags={match_strings:?}")?;
        }

        Ok(())
    }
}

impl std::fmt::Display for WorklogQueryResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[")?;
        let result_count = self.matching.len();
        for (i, entry) in self.matching.iter().enumerate() {
            if i == result_count - 1 {
                write!(f, "{entry}")?;
            } else {
                write!(f, "{entry}; ")?;
            }
        }
        write!(f, "]")
    }
}

impl WorklogQuery {
    /// Create a worklog_query tool for the given worklog directory.
    pub fn new(dir: Arc<Mutex<WorklogDirectory>>) -> Self {
        WorklogQuery { storage: dir }
    }
}

impl CallableTool for WorklogQuery {
    async fn execute(&self, params: WorklogQueryParams) -> Result<WorklogQueryResult> {
        let start = NaiveDate::parse_from_str(&params.start_date, "%Y-%m-%d")
            .map_err(|err| anyhow!("Failed to parse start_date parameter: {err}"))?;
        let end = NaiveDate::parse_from_str(&params.end_date, "%Y-%m-%d")
            .map_err(|err| anyhow!("Failed to parse end_date parameter: {err}"))?;

        let storage = self.storage.lock().await;
        let mut everything = Vec::new();
        let mut current = start;
        while current <= end {
            everything.append(&mut storage.load_week(current)?);
            current = current + Days::new(7);
        }

        let mut filtered = everything
            .into_iter()
            .filter(|item| {
                let Ok(timestamp) = DateTime::<FixedOffset>::parse_from_rfc3339(&item.timestamp)
                else {
                    // Ignore database corruption...
                    return false;
                };

                let date = timestamp.date_naive();
                if date < start || date > end {
                    return false;
                }
                if let Some(tags) = &params.tags
                    && !item.settable.tags.iter().any(|tag| tags.contains(tag))
                {
                    return false;
                }
                if let Some(components) = &params.components
                    && !item
                        .settable
                        .components
                        .iter()
                        .any(|component| components.contains(component))
                {
                    return false;
                }
                if let Some(match_strings) = &params.match_strings
                    && !match_strings.iter().any(|string| {
                        item.settable.summary.contains(string)
                            || item.settable.narrative.contains(string)
                    })
                {
                    return false;
                }

                true
            })
            .collect::<Vec<_>>();

        filtered.sort_by(|item1, item2| {
            // Yes, would be nice if we could bring the timestamp from above here, but, well,
            // whatever.  Unwrap is safe, we only have items with valid timestamps.
            let ts1 = DateTime::<FixedOffset>::parse_from_rfc3339(&item1.timestamp).unwrap();
            let ts2 = DateTime::<FixedOffset>::parse_from_rfc3339(&item2.timestamp).unwrap();

            ts1.cmp(&ts2)
        });

        Ok(WorklogQueryResult { matching: filtered })
    }
}
