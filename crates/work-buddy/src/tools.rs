//! WorkBuddy-specific tools.

use anyhow::{Result, anyhow};
use llimo::CallableTool;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::{fmt, fs};
use tokio::sync::Mutex;

/// To-do file
#[derive(Debug)]
pub struct TodoFile {
    /// Path where to load/store the content
    path: PathBuf,

    /// Content of the list
    content: HashMap<String, TodoElement>,
}

impl TodoFile {
    /// Open the to-do file udner the given `path`.
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

        Ok(TodoFile { path, content })
    }

    /// Add relevant tools for this file to `agent`.
    pub fn add_tools(self, agent: &mut llimo::Agent) {
        let this = Arc::new(Mutex::new(self));

        agent.add_tool(TodoAdd::new(Arc::clone(&this)));
        agent.add_tool(TodoRemove::new(Arc::clone(&this)));
        agent.add_tool(TodoEdit::new(Arc::clone(&this)));
        agent.add_tool(TodoList::new(this));
    }

    /// Write the contents into the file.
    fn write(&self) -> Result<()> {
        let json = serde_json::to_string(&self.content)
            .map_err(|err| anyhow!("Failed to convert to-do list to JSON: {err}"))?;

        fs::write(&self.path, json)
            .map_err(|err| anyhow!("Failed to write to-do list file: {err}"))?;

        Ok(())
    }
}

/// To-do list item (ID is in the `HashMap`)
#[derive(Clone, Debug, Deserialize, Serialize)]
struct TodoElement {
    /// Ticket URL if any
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ticket: Option<String>,

    /// What there is to do
    description: String,
}

llimo::tool! {
    'name: "todo_add";

    /// Add an item to the to-do list.
    #[derive(Debug)]
    'params: pub struct TodoAddParams {
        /// Meaningful ID to distinguish from other items
        id: String,

        /// Ticket URL, if any
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ticket: Option<String>,

        /// What there is to do
        description: String,
    }

    /// Result of adding a new to-do item.
    #[derive(Debug)]
    'result: pub struct TodoAddResult {
        /// ID of the new item
        id: String,
    }

    /// Add an item to the to-do list.
    #[derive(Debug)]
    'state: pub struct TodoAdd {
        file: Arc<Mutex<TodoFile>>,
    }
}

impl fmt::Display for TodoAddParams {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "id={} ", self.id)?;
        if let Some(ticket) = &self.ticket {
            write!(f, "ticket={ticket} ")?;
        }
        if self.description.len() <= 50 {
            write!(f, "desc={}", self.description)?;
        } else {
            write!(f, "desc={:.49}…", self.description)?;
        }
        Ok(())
    }
}

impl fmt::Display for TodoAddResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "id={}", self.id)
    }
}

impl TodoAdd {
    /// Create a todo_add tool for the given file.
    pub fn new(file: Arc<Mutex<TodoFile>>) -> Self {
        TodoAdd { file }
    }
}

impl CallableTool for TodoAdd {
    async fn execute(&self, params: TodoAddParams) -> Result<TodoAddResult> {
        let mut file = self.file.lock().await;

        if file.content.contains_key(&params.id) {
            return Err(anyhow!(
                "Item with ID {} already exists in the to-do list",
                params.id
            ));
        }

        file.content.insert(
            params.id.clone(),
            TodoElement {
                ticket: params.ticket,
                description: params.description,
            },
        );

        file.write()?;

        Ok(TodoAddResult { id: params.id })
    }
}

llimo::tool! {
    'name: "todo_remove";

    /// Remove an item from the to-do list.
    #[derive(Debug)]
    'params: pub struct TodoRemoveParams {
        /// ID of the item to remove
        id: String,
    }

    /// Result of removing a to-do item.
    #[derive(Debug)]
    'result: pub struct TodoRemoveResult {
        /// ID of the removed item
        id: String,
    }

    /// Remove an item from the to-do list.
    #[derive(Debug)]
    'state: pub struct TodoRemove {
        file: Arc<Mutex<TodoFile>>,
    }
}

impl fmt::Display for TodoRemoveParams {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "id={}", self.id)
    }
}

impl fmt::Display for TodoRemoveResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "id={}", self.id)
    }
}

impl TodoRemove {
    /// Create a todo_remove tool for the given file.
    pub fn new(file: Arc<Mutex<TodoFile>>) -> Self {
        TodoRemove { file }
    }
}

impl CallableTool for TodoRemove {
    async fn execute(&self, params: TodoRemoveParams) -> Result<TodoRemoveResult> {
        let mut file = self.file.lock().await;

        file.content.remove(&params.id).ok_or_else(|| {
            anyhow!(
                "Item with ID {} does not exist in the to-do list",
                params.id
            )
        })?;

        file.write()?;

        Ok(TodoRemoveResult { id: params.id })
    }
}

llimo::tool! {
    'name: "todo_edit";

    /// Edit an existing item on the to-do list.
    #[derive(Debug)]
    'params: pub struct TodoEditParams {
        /// ID of the existing item on the list
        id: String,

        /// Update the ticket URL; will be removed if omitted
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ticket: Option<String>,

        /// Update what there is to do
        description: String,
    }

    /// Result of editing a to-do item.
    #[derive(Debug)]
    'result: pub struct TodoEditResult {
        /// ID of the item that has been edited
        id: String,
    }

    /// Edit an existing item on the to-do list.
    #[derive(Debug)]
    'state: pub struct TodoEdit {
        file: Arc<Mutex<TodoFile>>,
    }
}

impl fmt::Display for TodoEditParams {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "id={} ", self.id)?;
        if let Some(ticket) = &self.ticket {
            write!(f, "ticket={ticket} ")?;
        }
        if self.description.len() <= 50 {
            write!(f, "desc={}", self.description)?;
        } else {
            write!(f, "desc={:.49}…", self.description)?;
        }
        Ok(())
    }
}

impl fmt::Display for TodoEditResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "id={}", self.id)
    }
}

impl TodoEdit {
    /// Create a todo_edit tool for the given file.
    pub fn new(file: Arc<Mutex<TodoFile>>) -> Self {
        TodoEdit { file }
    }
}

impl CallableTool for TodoEdit {
    async fn execute(&self, params: TodoEditParams) -> Result<TodoEditResult> {
        let mut file = self.file.lock().await;

        let element = file.content.get_mut(&params.id).ok_or_else(|| {
            anyhow!(
                "Item with ID {} does not exist in the to-do list",
                params.id
            )
        })?;

        element.ticket = params.ticket;
        element.description = params.description;

        file.write()?;

        Ok(TodoEditResult { id: params.id })
    }
}

llimo::tool! {
    'name: "todo_list";

    /// List all existing items on the to-do list.
    #[derive(Debug)]
    'params: pub struct TodoListParams {}

    /// All items on the to-do list.
    #[derive(Debug)]
    'result: pub struct TodoListResult {
        /// The full to-do list, keyed by ID
        #[serde(flatten)]
        list: HashMap<String, TodoElement>,
    }

    /// List all existing items on the to-do list.
    #[derive(Debug)]
    'state: pub struct TodoList {
        file: Arc<Mutex<TodoFile>>,
    }
}

impl fmt::Display for TodoListParams {
    fn fmt(&self, _: &mut fmt::Formatter<'_>) -> fmt::Result {
        Ok(())
    }
}

impl fmt::Display for TodoListResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let count = self.list.len();
        for (i, (id, element)) in self.list.iter().enumerate() {
            write!(f, "{id}={{")?;
            if let Some(ticket) = &element.ticket {
                write!(f, "ticket={ticket} ")?;
            }
            if element.description.len() <= 50 {
                write!(f, "desc={}", element.description)?;
            } else {
                write!(f, "desc={:.49}…", element.description)?;
            }

            if i == count - 1 {
                write!(f, "}}")?;
            } else {
                write!(f, "}}; ")?;
            }
        }

        Ok(())
    }
}

impl TodoList {
    /// Create a todo_list tool for the given file.
    pub fn new(file: Arc<Mutex<TodoFile>>) -> Self {
        TodoList { file }
    }
}

impl CallableTool for TodoList {
    async fn execute(&self, _params: TodoListParams) -> Result<TodoListResult> {
        let file = self.file.lock().await;
        Ok(TodoListResult {
            list: file.content.clone(),
        })
    }
}
