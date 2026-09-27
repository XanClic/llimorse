//! File tools.

use super::ToolGate;
use anyhow::{Result, anyhow, bail};
use helpers::TruncatedDisplay;
use llimorse::CallableTool;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read as _, Seek as _, Write as _};
use std::path::PathBuf;

llimorse::tool! {
    'name: "view";

    /// Read the given file, in whole or in part.
    #[derive(Debug)]
    'params: pub struct ViewParams {
        /// Filename to read
        filename: PathBuf,

        /// Range of lines to read: First line and last line (inclusive). Full file if not
        /// specified.
        range: Option<(usize, usize)>,
    }

    /// Result of reading a range from a file.
    #[derive(Debug)]
    'result: pub struct ViewResult {
        /// Filename from which data was read
        filename: PathBuf,

        /// Requested content
        content: String,
    }

    /// Allow reading files (unrestricted), in whole or in part.
    #[derive(Debug)]
    'state: pub struct View<G: ToolGate> {
        /// Gate for receiving permissions to read files
        gate: G,
    }
}

impl fmt::Display for ViewParams {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "filename={}", self.filename.display())?;

        if let Some((start, end)) = self.range {
            write!(f, " range=[{start}, {end}]")?
        }

        Ok(())
    }
}

impl fmt::Display for ViewResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "filename={} content={:?}",
            self.filename.display(),
            self.content.truncated_display(100)
        )
    }
}

impl<G: ToolGate> View<G> {
    /// Allow viewing files (anywhere)
    pub fn new(gate: G) -> Self {
        View { gate }
    }
}

impl<G: ToolGate> CallableTool for View<G> {
    async fn execute(&self, params: ViewParams) -> Result<ViewResult> {
        self.gate
            .permitted(&params)
            .await
            .map_err(|e| anyhow!("View tool call rejected: {e}"))?;

        let file = File::open(&params.filename)
            .map_err(|err| anyhow!("Failed to open {}: {err}", params.filename.display()))?;
        let reader = BufReader::new(file);

        let range = params.range.unwrap_or((1, usize::MAX));
        let skip = range
            .0
            .checked_sub(1) // ranges are 1-based
            .ok_or_else(|| anyhow!("Range {range:?}: First line is 1, not 0"))?;

        let take = range
            .1
            .checked_sub(range.0)
            .ok_or_else(|| {
                anyhow!(
                    "Range end must be greater or equal to the range start: {} >= {}",
                    range.1,
                    range.0
                )
            })?
            .saturating_add(1); // end is inclusive, so 0 diff means read 1 line

        let content = reader
            .lines()
            .skip(skip)
            .take(take)
            .try_fold(String::new(), |mut acc, line| -> Result<String> {
                if !acc.is_empty() {
                    acc.push('\n');
                }
                acc.push_str(&line?);
                Ok(acc)
            })
            .map_err(|err| anyhow!("Failed to read {}: {err}", params.filename.display()))?;

        Ok(ViewResult {
            filename: params.filename,
            content,
        })
    }
}

llimorse::tool! {
    'name: "write";

    /// Overwrite a file in full.
    #[derive(Debug)]
    'params: pub struct WriteParams {
        /// Filename to overwrite
        filename: PathBuf,

        /// Full content to write into the file
        content: String,
    }

    /// Result of overwriting a file in full.
    #[derive(Debug)]
    'result: pub struct WriteResult {
        /// Filename into which the data was written
        filename: PathBuf,

        /// How much data was written
        bytes_written: usize,
    }

    /// Overwrite files (unrestricted) in whole
    #[derive(Debug)]
    'state: pub struct Write<G: ToolGate> {
        /// Gate for receiving permissions to overwrite files
        gate: G,
    }
}

impl fmt::Display for WriteParams {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "filename={} content={:?}",
            self.filename.display(),
            self.content.truncated_display(100)
        )
    }
}

impl fmt::Display for WriteResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "filename={} bytes_written={}",
            self.filename.display(),
            self.bytes_written
        )
    }
}

impl<G: ToolGate> Write<G> {
    /// Overwrite files (anywhere) in whole
    pub fn new(gate: G) -> Self {
        Write { gate }
    }
}

impl<G: ToolGate> CallableTool for Write<G> {
    async fn execute(&self, params: WriteParams) -> Result<WriteResult> {
        self.gate
            .permitted(&params)
            .await
            .map_err(|e| anyhow!("Write tool call rejected: {e}"))?;

        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&params.filename)
            .map_err(|err| anyhow!("Failed to open {}: {err}", params.filename.display()))?;

        let len = params.content.len(); // byte length
        file.write_all(params.content.as_bytes())
            .map_err(|err| anyhow!("Failed to write to {}: {err}", params.filename.display()))?;

        Ok(WriteResult {
            filename: params.filename,
            bytes_written: len,
        })
    }
}

llimorse::tool! {
    'name: "edit";

    /// Substitute a string in a file
    #[derive(Debug)]
    'params: pub struct EditParams {
        /// File to edit
        filename: PathBuf,

        /// Old string, the one to replace
        old_content: String,

        /// New string to replace the old one
        new_content: String,
    }

    /// Result of editing a file
    #[derive(Debug)]
    'result: pub struct EditResult {
        /// Filename into which the data was written
        filename: PathBuf,

        /// Size difference from before and after
        byte_size_changed: isize,
    }

    /// Substitute strings in files (unrestricted)
    #[derive(Debug)]
    'state: pub struct Edit<G: ToolGate> {
        /// Gate for receiving permissions to edit files
        gate: G,
    }
}

impl fmt::Display for EditParams {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "filename={} {:?} -> {:?}",
            self.filename.display(),
            self.old_content.truncated_display(100),
            self.new_content.truncated_display(100)
        )
    }
}

impl fmt::Display for EditResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "filename={} byte_size_changed={}",
            self.filename.display(),
            self.byte_size_changed
        )
    }
}

impl<G: ToolGate> Edit<G> {
    /// Substitute strings in files (unrestricted)
    pub fn new(gate: G) -> Self {
        Edit { gate }
    }
}

impl<G: ToolGate> CallableTool for Edit<G> {
    async fn execute(&self, params: EditParams) -> Result<EditResult> {
        self.gate
            .permitted(&params)
            .await
            .map_err(|e| anyhow!("Edit tool call rejected: {e}"))?;

        let mut file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&params.filename)
            .map_err(|err| anyhow!("Failed to open {}: {err}", params.filename.display()))?;

        let mut content = String::new();
        file.read_to_string(&mut content)
            .map_err(|err| anyhow!("Failed to read from {}: {err}", params.filename.display()))?;

        let first_match = content.find(&params.old_content).ok_or_else(|| {
            anyhow!(
                "Did not find old_content string in {}",
                params.filename.display()
            )
        })?;

        let end_match = first_match + params.old_content.len();
        if content[end_match..].find(&params.old_content).is_some() {
            bail!(
                "Multiple old_content matches in {}",
                params.filename.display()
            )
        }

        file.set_len(first_match as u64)
            .map_err(|err| anyhow!("Failed to write to {}: {err}", params.filename.display()))?;
        file.seek(io::SeekFrom::End(0))
            .map_err(|err| anyhow!("Failed to write to {}: {err}", params.filename.display()))?;
        file.write_all(params.new_content.as_bytes())
            .map_err(|err| anyhow!("Failed to write to {}: {err}", params.filename.display()))?;
        file.write_all(&content.as_bytes()[end_match..])
            .map_err(|err| anyhow!("Failed to write to {}: {err}", params.filename.display()))?;

        let byte_size_changed =
            params.new_content.len() as isize - params.old_content.len() as isize;
        Ok(EditResult {
            filename: params.filename,
            byte_size_changed,
        })
    }
}
