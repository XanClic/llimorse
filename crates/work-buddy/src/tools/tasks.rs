//! Task list management

use anyhow::{Result, anyhow};
use chrono::Local;
use chrono::format::SecondsFormat;
use llimo::{Agent, CallableTool, ChatListener};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt::{self, Write as _};
use std::fs;
use std::io::{self, Write as _};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Task file
#[derive(Debug)]
pub struct TaskFile {
    /// Path where to load/store the content
    path: PathBuf,

    /// Content of the list
    content: HashMap<String, Task>,
}

impl TaskFile {
    /// Open the task file under the given `path`.
    pub fn open(path: PathBuf) -> Result<Self> {
        let content = match fs::read_to_string(&path) {
            Ok(content) => content,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                let mut file = fs::File::create_new(&path).map_err(|c_err| {
                    anyhow!("Opening file failed: {err}; and creating failed, too: {c_err}")
                })?;

                let content = String::from("{}");
                file.write_all(content.as_bytes())?;
                content
            }
            Err(err) => return Err(err.into()),
        };

        let content = serde_json::from_str(&content)?;

        Ok(TaskFile { path, content })
    }

    /// Inject the list of active tasks as system messages
    ///
    /// This also includes critical backlog items.
    pub fn inject_active_tasks(&self, agent: &mut Agent<impl ChatListener>) {
        let active_tasks = self.content.iter().filter(|(_, task)| {
            task.settable.status != TaskStatus::Backlog
                || task.settable.priority == TaskPriority::Critical
        });

        let mut message = None::<String>;
        for (id, task) in active_tasks {
            let message = message.get_or_insert_with(|| "List of active tasks:".into());

            write!(
                message,
                "\n- '{id}' ({:?}, {:?} priority):\n",
                task.settable.status, task.settable.priority,
            )
            .expect("Failed to append to task list string");

            for (list, header, inline_name) in [
                (&task.settable.description, "Description", "description"),
                (&task.settable.tickets, "Tickets", "public tickets"),
            ] {
                if list.is_empty() {
                    writeln!(message, "  - (does not have any {inline_name} yet)")
                        .expect("Failed to append to task list string");
                } else {
                    writeln!(message, "  - {header}:")
                        .expect("Failed to append to task list string");
                    for (key, value) in list {
                        writeln!(message, "    - {key}: {value}")
                            .expect("Failed to append to task list string");
                    }
                }
            }

            write!(
                message,
                "  - Created: {}\n  - Last updated: {}",
                task.created_at, task.updated_at,
            )
            .expect("Failed to append to task list string");
        }

        if let Some(message) = message {
            agent.push_system(&message);
        } else {
            agent.push_system("(There are no active tasks.)");
        }
    }

    /// Add relevant tools for this file to `agent`.
    pub fn add_tools(self, agent: &mut Agent<impl ChatListener>) {
        let this = Arc::new(Mutex::new(self));

        agent.add_tool(TaskAdd::new(Arc::clone(&this)));
        agent.add_tool(TaskRemove::new(Arc::clone(&this)));
        agent.add_tool(TaskUpdate::new(Arc::clone(&this)));
        agent.add_tool(TaskQuery::new(this));
    }

    /// Write the contents into the file.
    fn write(&self) -> Result<()> {
        let json = serde_json::to_string(&self.content)
            .map_err(|err| anyhow!("Failed to convert task list to JSON: {err}"))?;

        fs::write(&self.path, json)
            .map_err(|err| anyhow!("Failed to write task list file: {err}"))?;

        Ok(())
    }
}

/// A task (ID is in the `HashMap`)
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
struct Task {
    /// LLM-settable fields
    #[serde(flatten)]
    settable: TaskSettable,

    /// When the task was first created
    created_at: String,

    /// When the task was last updated
    updated_at: String,
}

/// The part of a task that is LLM-settable
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
struct TaskSettable {
    /// Task status
    status: TaskStatus,

    /// Task priority
    #[serde(default)]
    priority: TaskPriority,

    /// Ticket URLs
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    tickets: HashMap<String, String>,

    /// What there is to do, keyed by keywords (to allow information to be added over time)
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    description: HashMap<String, String>,
}

/// The task’s status
#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize, JsonSchema)]
#[schemars(inline)]
enum TaskStatus {
    /// Not currently active, but planned for later, at some point
    Backlog,

    /// Completely new on the list, needs triaging first
    NotYetTriaged,

    /// Currently being worked on
    InProgress,

    /// Used to be worked on, but currently blocked by something (reason goes into the description)
    Blocked,
}

/// The task’s priority
#[derive(
    Clone, Copy, Debug, Default, Eq, PartialEq, PartialOrd, Deserialize, Serialize, JsonSchema,
)]
#[schemars(inline)]
enum TaskPriority {
    /// Somewhere in the background, if there is time
    Low,

    /// Normal task priority
    #[default]
    Normal,

    /// Elevated priority, is needed soon
    High,

    /// Absolutely critical priority, trounces everything else
    Critical,
}

impl fmt::Display for Task {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} created_at={} updated_at={}",
            self.settable, self.created_at, self.updated_at
        )
    }
}

impl fmt::Display for TaskSettable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "status={:?} prio={:?}", self.status, self.priority)?;

        for (map, title) in [(&self.tickets, "tickets"), (&self.description, "desc")] {
            write!(f, " {title}=[")?;
            let len = map.len();
            for (i, (key, value)) in map.iter().enumerate() {
                let separator = if i == len - 1 { "" } else { ", " };

                if value.len() <= 50 {
                    write!(f, "{key}={value:?}{separator}")?;
                } else {
                    write!(f, "{key}=\"{value:.49}…\"{separator}")?;
                }
            }
            write!(f, "]")?;
        }

        Ok(())
    }
}

llimo::tool! {
    'name: "task_add";

    /// Add a task to the task list.
    #[derive(Debug)]
    'params: pub struct TaskAddParams {
        /// Meaningful ID to distinguish from other tasks
        id: String,

        /// Task to add
        #[serde(flatten)]
        task: TaskSettable,
    }

    /// Result of adding a new task.
    #[derive(Debug)]
    'result: pub struct TaskAddResult {
        /// ID of the new task
        id: String,
    }

    /// Add a task to the task list.
    #[derive(Debug)]
    'state: pub struct TaskAdd {
        file: Arc<Mutex<TaskFile>>,
    }
}

impl fmt::Display for TaskAddParams {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "id={} {}", self.id, self.task)
    }
}

impl fmt::Display for TaskAddResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "id={}", self.id)
    }
}

impl TaskAdd {
    /// Create a task_add tool for the given file.
    pub fn new(file: Arc<Mutex<TaskFile>>) -> Self {
        TaskAdd { file }
    }
}

impl CallableTool for TaskAdd {
    async fn execute(&self, params: TaskAddParams) -> Result<TaskAddResult> {
        let mut file = self.file.lock().await;

        if file.content.contains_key(&params.id) {
            return Err(anyhow!(
                "Task with ID {} already exists in the task list",
                params.id
            ));
        }

        let now = Local::now().to_rfc3339_opts(SecondsFormat::Secs, false);
        let task = Task {
            settable: params.task.clone(),
            created_at: now.clone(),
            updated_at: now,
        };
        file.content.insert(params.id.clone(), task);

        file.write()?;

        Ok(TaskAddResult { id: params.id })
    }
}

llimo::tool! {
    'name: "task_remove";

    /// Remove a task from the task list.
    #[derive(Debug)]
    'params: pub struct TaskRemoveParams {
        /// ID of the task to remove
        id: String,
    }

    /// Result of removing a task.
    #[derive(Debug)]
    'result: pub struct TaskRemoveResult {
        /// ID of the removed task
        id: String,
    }

    /// Remove a task from the task list.
    #[derive(Debug)]
    'state: pub struct TaskRemove {
        file: Arc<Mutex<TaskFile>>,
    }
}

impl fmt::Display for TaskRemoveParams {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "id={}", self.id)
    }
}

impl fmt::Display for TaskRemoveResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "id={}", self.id)
    }
}

impl TaskRemove {
    /// Create a task_remove tool for the given file.
    pub fn new(file: Arc<Mutex<TaskFile>>) -> Self {
        TaskRemove { file }
    }
}

impl CallableTool for TaskRemove {
    async fn execute(&self, params: TaskRemoveParams) -> Result<TaskRemoveResult> {
        let mut file = self.file.lock().await;

        file.content
            .remove(&params.id)
            .ok_or_else(|| anyhow!("Task with ID {} does not exist in the task list", params.id))?;

        file.write()?;

        Ok(TaskRemoveResult { id: params.id })
    }
}

llimo::tool! {
    'name: "task_update";

    /// Update/edit an existing task on the task list.
    #[derive(Debug)]
    'params: pub struct TaskUpdateParams {
        /// ID of the existing task on the list
        id: String,

        /// New task status; default is no change
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<TaskStatus>,

        /// New task priority; default is no change
        #[serde(default, skip_serializing_if = "Option::is_none")]
        priority: Option<TaskPriority>,

        /// Tickets to add to the task (or specify nil to remove a ticket)
        #[serde(default, skip_serializing_if = "HashMap::is_empty")]
        tickets: HashMap<String, Option<String>>,

        /// Information to add to the task (or specify nil to remove a keyword)
        #[serde(default, skip_serializing_if = "HashMap::is_empty")]
        description: HashMap<String, Option<String>>,
    }

    /// Result of updating a task.
    #[derive(Debug)]
    'result: pub struct TaskUpdateResult {
        /// ID of the task that has been updated
        id: String,
    }

    /// Update/edit an existing task on the task list.
    #[derive(Debug)]
    'state: pub struct TaskUpdate {
        file: Arc<Mutex<TaskFile>>,
    }
}

impl fmt::Display for TaskUpdateParams {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "id={}", self.id)?;

        if let Some(status) = self.status {
            write!(f, " status={status:?}")?;
        }
        if let Some(priority) = self.priority {
            write!(f, " prio={priority:?}")?;
        }
        for (map, title) in [(&self.tickets, "tickets"), (&self.description, "desc")] {
            let len = map.len();
            if len > 0 {
                write!(f, " {title}=[")?;
                for (i, (key, value)) in map.iter().enumerate() {
                    let separator = if i == len - 1 { "" } else { ", " };
                    if let Some(value) = value {
                        if value.len() <= 50 {
                            write!(f, "{key}={value:?}{separator}")?;
                        } else {
                            write!(f, "{key}=\"{value:.49}…\"{separator}")?;
                        }
                    } else {
                        write!(f, "{key}=nil")?;
                    }
                }
                write!(f, "]")?;
            }
        }

        Ok(())
    }
}

impl fmt::Display for TaskUpdateResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "id={}", self.id)
    }
}

impl TaskUpdate {
    /// Create a task_update tool for the given file.
    pub fn new(file: Arc<Mutex<TaskFile>>) -> Self {
        TaskUpdate { file }
    }
}

impl CallableTool for TaskUpdate {
    async fn execute(&self, params: TaskUpdateParams) -> Result<TaskUpdateResult> {
        let mut file = self.file.lock().await;

        let task = file
            .content
            .get_mut(&params.id)
            .ok_or_else(|| anyhow!("Task with ID {} does not exist in the task list", params.id))?;

        if let Some(status) = params.status {
            task.settable.status = status;
        }
        if let Some(priority) = params.priority {
            task.settable.priority = priority;
        }
        for (state, amendment) in [
            (&mut task.settable.tickets, params.tickets),
            (&mut task.settable.description, params.description),
        ] {
            for (key, value) in amendment {
                if let Some(value) = value {
                    state.insert(key, value);
                } else {
                    state.remove(&key);
                }
            }
        }

        task.updated_at = Local::now().to_rfc3339_opts(SecondsFormat::Secs, false);

        file.write()?;

        Ok(TaskUpdateResult { id: params.id })
    }
}

llimo::tool! {
    'name: "task_query";

    /// Query tasks from the list of *all* tasks.
    #[derive(Debug)]
    'params: pub struct TaskQueryParams {
        /// Query specific task IDs
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ids: Option<Vec<String>>,

        /// List only tasks with one of these statuses
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<Vec<TaskStatus>>,

        /// List only tasks with at least this priority
        #[serde(default, skip_serializing_if = "Option::is_none")]
        min_priority: Option<TaskPriority>,
    }

    /// Tasks on the task list, as requested.
    #[derive(Debug)]
    'result: pub struct TaskQueryResult {
        /// Matching tasks from the task list, keyed by ID
        #[serde(flatten)]
        list: HashMap<String, Task>,
    }

    /// Query tasks from the list of *all* tasks.
    #[derive(Debug)]
    'state: pub struct TaskQuery {
        file: Arc<Mutex<TaskFile>>,
    }
}

impl fmt::Display for TaskQueryParams {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut display = Vec::with_capacity(3);

        if let Some(ids) = &self.ids {
            display.push(format!("id in {ids:?}"));
        }

        if let Some(status) = &self.status {
            display.push(format!("status in {status:?}"));
        }

        if let Some(min_priority) = &self.min_priority {
            display.push(format!("priority >= {min_priority:?}"));
        }

        write!(f, "{}", display.join("; "))
    }
}

impl fmt::Display for TaskQueryResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let count = self.list.len();
        for (i, (id, task)) in self.list.iter().enumerate() {
            if i == count - 1 {
                write!(f, "{id}={{{task}}}")?;
            } else {
                write!(f, "{id}={{{task}}}; ")?;
            }
        }

        Ok(())
    }
}

impl TaskQuery {
    /// Create a task_query tool for the given file.
    pub fn new(file: Arc<Mutex<TaskFile>>) -> Self {
        TaskQuery { file }
    }
}

impl CallableTool for TaskQuery {
    async fn execute(&self, params: TaskQueryParams) -> Result<TaskQueryResult> {
        let file = self.file.lock().await;

        let list = file
            .content
            .iter()
            .filter(|(id, task)| {
                if let Some(ids) = &params.ids
                    && !ids.contains(id)
                {
                    return false;
                }

                if let Some(status) = &params.status
                    && !status.contains(&task.settable.status)
                {
                    return false;
                }

                if let Some(min_priority) = &params.min_priority
                    && task.settable.priority < *min_priority
                {
                    return false;
                }

                true
            })
            .map(|(id, task)| (id.clone(), task.clone()))
            .collect();

        Ok(TaskQueryResult { list })
    }
}
