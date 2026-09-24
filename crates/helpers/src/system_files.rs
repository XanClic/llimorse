//! Deserialization of the `system` argument from a config file.
//!
//! Accepts either a single file path — the legacy form,
//! `system = "foo.md"` — or a list of file paths,
//! `system = ["foo.md", "bar.md"]`. Both deserialize into a
//! `Vec<PathBuf>`; on the command line, `--system` is repeatable.

use serde::de::{self, Deserializer, SeqAccess, Visitor};
use std::fmt;
use std::path::PathBuf;

/// Deserialize a single file path or a list of file paths into a
/// `Vec<PathBuf>`.
///
/// The single-path form exists for backwards compatibility with config files
/// written before `--system` became repeatable.
pub fn deserialize<'de, D>(deserializer: D) -> Result<Vec<PathBuf>, D::Error>
where
    D: Deserializer<'de>,
{
    /// Visitor for a single path or a list of paths.
    struct SystemFiles;

    impl<'de> Visitor<'de> for SystemFiles {
        type Value = Vec<PathBuf>;

        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("a file path or a list of file paths")
        }

        /// The legacy form: a single path, e.g. `system = "foo.md"`.
        fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
            Ok(vec![PathBuf::from(v)])
        }

        /// The new form: a list of paths, e.g. `system = ["foo.md", "bar.md"]`.
        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            let mut files = Vec::new();
            while let Some(file) = seq.next_element()? {
                files.push(file);
            }
            Ok(files)
        }
    }

    deserializer.deserialize_any(SystemFiles)
}
