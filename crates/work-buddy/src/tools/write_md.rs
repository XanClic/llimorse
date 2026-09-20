//! A tool to just write out markdown files

use anyhow::{Result, anyhow};
use helpers::TruncatedDisplay;
use llimorse::CallableTool;
use std::io::Write;
use std::path::PathBuf;
use std::{fmt, fs};

llimorse::tool! {
    'name: "write_markdown";

    /// Write a full markdown file for example with design specifications developed in a
    /// conversation with the user, to save for later viewing.
    #[derive(Debug)]
    'params: pub struct WriteMarkdownParams {
        /// Filename for the file (no path components, just plain filename)
        filename: PathBuf,

        /// What to write into the file (full content)
        content: String,
    }

    /// Result of writing the markdown file
    #[derive(Debug)]
    'result: pub struct WriteMarkdownResult {
        /// Filename of the file written
        filename: PathBuf,

        /// How many bytes were written
        bytes_written: usize,
    }

    /// Write a full markdown file for example with design specifications developed in a
    /// conversation with the user, to save for later viewing.
    #[derive(Debug)]
    'state: pub struct WriteMarkdown {
        /// Directory where to store markdown output files
        dir: PathBuf,
    }
}

impl fmt::Display for WriteMarkdownParams {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "filename={} content={:?}",
            self.filename.display(),
            self.content.truncated_display(100)
        )?;
        Ok(())
    }
}

impl fmt::Display for WriteMarkdownResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "filename={} bytes_written={}",
            self.filename.display(),
            self.bytes_written
        )
    }
}

impl WriteMarkdown {
    /// Create a new `write_markdown` tool, allowing to put markdown files into `dir`.
    pub fn new(dir: PathBuf) -> Self {
        WriteMarkdown { dir }
    }
}

impl CallableTool for WriteMarkdown {
    async fn execute(&self, params: WriteMarkdownParams) -> Result<WriteMarkdownResult> {
        let path = self.dir.join(&params.filename);
        let mut file = fs::File::create_new(&path)
            .map_err(|err| anyhow!("Failed to create {}: {err}", path.display()))?;
        let bytes = params.content.as_bytes();
        file.write_all(bytes)
            .map_err(|err| anyhow!("Failed to write to {}: {err}", path.display()))?;

        Ok(WriteMarkdownResult {
            filename: params.filename,
            bytes_written: bytes.len(),
        })
    }
}
