//! Tools to manage contextual knowledge

use anyhow::{Result, anyhow};
use llimo::CallableTool;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::{fmt, fs};
use tokio::sync::Mutex;

/// Knowledge file
#[derive(Debug)]
pub struct KnowledgeFile {
    /// Path where to load/store the content
    path: PathBuf,

    /// Knowledge content
    content: HashMap<String, KnowledgeOrAlias>,
}

/// Knowledge entry, or an alias
#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum KnowledgeOrAlias {
    /// Knowledge content, describing the keyword
    Content(String),

    /// Link to a different keyword, both of which are described by the same content
    Alias(String),
}

impl KnowledgeFile {
    /// Open the knowledge file under the given `path`.
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

        Ok(KnowledgeFile { path, content })
    }

    /// Add relevant tools for this file to `agent`.
    pub fn add_tools(self, agent: &mut llimo::Agent) {
        let this = Arc::new(Mutex::new(self));

        agent.add_tool(KnowledgeUpsert::new(Arc::clone(&this)));
        agent.add_tool(KnowledgeQuery::new(this));
    }

    /// Write the contents into the file.
    fn write(&self) -> Result<()> {
        let json = serde_json::to_string(&self.content)
            .map_err(|err| anyhow!("Failed to convert knowledge database to JSON: {err}"))?;

        fs::write(&self.path, json)
            .map_err(|err| anyhow!("Failed to write knowledge database file: {err}"))?;

        Ok(())
    }
}

llimo::tool! {
    'name: "knowledge_upsert";

    /// Add new content to the knowledge database (to explain keywords, e.g. projects, components,
    /// etc.), or modify an existing entry.
    #[derive(Debug)]
    'params: pub struct KnowledgeUpsertParams {
        /// The keyword which this information describes
        keyword: String,

        /// The content to store under this keyword; by default, leave the existing content
        /// unchanged.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content: Option<String>,

        /// Alias keywords to add to this one, i.e., keywords that should resolve to the exact same
        /// content when queried.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        add_aliases: Option<Vec<String>>,
    }

    /// Result of modifying the knowledge database
    #[derive(Debug)]
    'result: pub struct KnowledgeUpsertResult {
        /// The keyword whose content was updated
        keyword: String,

        /// List of aliases that resolve to the same content
        aliases: Vec<String>,
    }

    /// Add new content to the knowledge database (to explain keywords, e.g. projects, components,
    /// etc.), or modify an existing entry.
    #[derive(Debug)]
    'state: pub struct KnowledgeUpsert {
        /// Knowledge database
        db: Arc<Mutex<KnowledgeFile>>,
    }
}

impl fmt::Display for KnowledgeUpsertParams {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "key={}", self.keyword)?;
        if let Some(content) = &self.content {
            if content.len() <= 50 {
                write!(f, " content={content:?}")?;
            } else {
                write!(f, " content=\"{content:.49}…\"")?;
            }
        }
        if let Some(aliases) = &self.add_aliases {
            write!(f, " add_aliases={aliases:?}")?;
        }
        Ok(())
    }
}

impl fmt::Display for KnowledgeUpsertResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "key={} aliases={:?}", self.keyword, self.aliases)
    }
}

impl KnowledgeUpsert {
    /// Create a knowledge_upsert tool for the given knowledge storage.
    pub fn new(storage: Arc<Mutex<KnowledgeFile>>) -> Self {
        KnowledgeUpsert { db: storage }
    }
}

impl CallableTool for KnowledgeUpsert {
    async fn execute(&self, params: KnowledgeUpsertParams) -> Result<KnowledgeUpsertResult> {
        if params.content.is_none() && params.add_aliases.is_none() {
            return Err(anyhow!("Must update content or add aliases"));
        }

        let mut db = self.db.lock().await;

        // Check first, when we can still generate errors without having modified anything
        if let Some(aliases) = &params.add_aliases {
            for alias in aliases {
                if let Some(entry) = db.content.get(alias) {
                    let err = match entry {
                        KnowledgeOrAlias::Content(_) => {
                            anyhow!("Cannot make '{alias}' an alias: Already contains content")
                        }
                        KnowledgeOrAlias::Alias(link) => {
                            anyhow!("Cannot make '{alias}' an alias: Already an alias of '{link}'")
                        }
                    };
                    return Err(err);
                }
            }
        }

        let resolved_keyword = if let Some(mut entry) = db.content.get_mut(&params.keyword) {
            let mut resolved = params.keyword.clone();
            while let KnowledgeOrAlias::Alias(link) = entry {
                let link = link.clone(); // Cannot `get_mut()` while having this borrowed
                entry = db.content.get_mut(&link).ok_or_else(|| {
                    anyhow!("Dead link: '{resolved}' points to '{link}', which does not exist")
                })?;
                resolved = link.clone();
            }
            if let Some(content) = params.content {
                *entry = KnowledgeOrAlias::Content(content);
            }
            resolved
        } else if let Some(content) = params.content {
            db.content
                .insert(params.keyword.clone(), KnowledgeOrAlias::Content(content));
            params.keyword.clone()
        } else {
            return Err(anyhow!(
                "{} does not have an entry yet, and you cannot add aliases to an empty entry",
                params.keyword
            ));
        };

        if let Some(aliases) = params.add_aliases {
            for alias in aliases {
                let old = db
                    .content
                    .insert(alias, KnowledgeOrAlias::Alias(resolved_keyword.clone()));
                // Loop above must have verified this
                assert!(old.is_none());
            }
        }

        db.write()?;

        let all_aliases = db
            .content
            .iter()
            .filter_map(|(kw, entry)| match entry {
                KnowledgeOrAlias::Content(_) => None,
                KnowledgeOrAlias::Alias(link) if link == &resolved_keyword => Some(kw.clone()),
                KnowledgeOrAlias::Alias(_) => None,
            })
            .collect();

        Ok(KnowledgeUpsertResult {
            keyword: params.keyword,
            aliases: all_aliases,
        })
    }
}

llimo::tool! {
    'name: "knowledge_query";

    /// Query content from the knowledge database, by keyword.
    #[derive(Debug)]
    'params: pub struct KnowledgeQueryParams {
        /// The keyword whose content to look up
        keyword: String,
    }

    /// Result of querying the knowledge database
    #[derive(Debug)]
    'result: pub struct KnowledgeQueryResult {
        /// The keyword that was looked up
        keyword: String,

        /// Content that describes the keyword
        content: String,
    }

    /// Query content from the knowledge database, by keyword.
    #[derive(Debug)]
    'state: pub struct KnowledgeQuery {
        /// Knowledge database
        db: Arc<Mutex<KnowledgeFile>>,
    }
}

impl fmt::Display for KnowledgeQueryParams {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "key={}", self.keyword)
    }
}

impl fmt::Display for KnowledgeQueryResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "key={} ", self.keyword)?;
        if self.content.len() <= 50 {
            write!(f, "content={:?}", self.content)
        } else {
            write!(f, "content=\"{:.49}…\"", self.content)
        }
    }
}

impl KnowledgeQuery {
    /// Create a knowledge_query tool for the given knowledge storage.
    pub fn new(storage: Arc<Mutex<KnowledgeFile>>) -> Self {
        KnowledgeQuery { db: storage }
    }
}

impl CallableTool for KnowledgeQuery {
    async fn execute(&self, params: KnowledgeQueryParams) -> Result<KnowledgeQueryResult> {
        let db = self.db.lock().await;

        let mut entry = db
            .content
            .get(&params.keyword)
            .ok_or_else(|| anyhow!("Keyword {} not present in the database", params.keyword))?;
        let mut looked_up = &params.keyword;
        while let KnowledgeOrAlias::Alias(link) = entry {
            entry = db.content.get(link).ok_or_else(|| {
                anyhow!("Dead link: '{looked_up}' points to '{link}', which does not exist")
            })?;
            looked_up = link;
        }

        let KnowledgeOrAlias::Content(content) = entry else {
            panic!(
                "Exhaustively went through all alias links, for some reason this is still no content entry: {entry:?}"
            );
        };

        Ok(KnowledgeQueryResult {
            keyword: params.keyword,
            content: content.clone(),
        })
    }
}
