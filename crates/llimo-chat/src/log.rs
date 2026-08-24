//! [`llimo::ChatListener`] implementation to keep a log.

use anyhow::{Result, anyhow};
use llimo::ChatListener;
use llimo::line_format::ChatMessage;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use tracing::error;

/// [`llimo::ChatListener`] implementation to keep a log in a file.
#[derive(Debug)]
pub struct SessionLog {
    /// Where to write the log output
    output: Option<File>,
}

impl SessionLog {
    /// Store the log in the given file.
    pub fn new<P: AsRef<Path>>(file: P) -> Result<Self> {
        Ok(SessionLog {
            output: Some(
                OpenOptions::new()
                    .create_new(true)
                    .append(true)
                    .open(file)?,
            ),
        })
    }

    /// Load the log from `load_from`.
    ///
    /// This log can be applied via [`llimo::Agent::push_history()`].
    pub fn load<P: AsRef<Path>>(load_from: P) -> Result<Vec<ChatMessage>> {
        BufReader::new(File::open(load_from)?)
            .lines()
            .map(|line| -> Result<ChatMessage> {
                let line = line.map_err(|err| anyhow!("Failed to read data: {err}"))?;
                serde_json::from_str(&line)
                    .map_err(|err| anyhow!("Failed to parse data: {line}: {err}"))
            })
            .collect()
    }

    /// Create a null log (i.e. does not store anything).
    ///
    /// This helps creating an `Agent<SessionLog>` that can both store a log or not.
    pub fn null() -> Self {
        SessionLog { output: None }
    }

    /// Log the given `message` to the output, raising errors.
    fn do_log(&mut self, message: &llimo::line_format::ChatMessage) -> Result<()> {
        let Some(file) = &mut self.output else {
            return Ok(());
        };

        let json = serde_json::to_string(message)
            .map_err(|err| anyhow!("Failed to serialize {message:?}: {err}"))?;

        file.write_all(json.as_bytes())?;
        file.write_all(b"\n")?;
        file.flush()?;

        Ok(())
    }
}

impl ChatListener for SessionLog {
    fn log_message(&mut self, message: &ChatMessage) {
        if let Err(err) = self.do_log(message) {
            error!("Failed to log {message:?}: {err}; log will be discontinued from here");
            self.output.take();
        }
    }
}
