# work-buddy

## Purpose

`work-buddy` is the workspace's example application: a terminal chat app in which an LLM agent
manages the user's task list and work log through tools. It is a binary crate and a leaf
of the workspace — no other crate depends on it. It demonstrates the `llimorse` +
`llimorse-chat` + `term-ui` stack, including custom tools built with the `llimorse::tool!`
macro.

## Files

- `Cargo.toml` — binary crate; depends on `llimorse`, `llimorse-chat` (with its `clap`
  feature), `llimorse-tools`, `term-ui`, `helpers`, plus `clap`, `serde`, `serde_json`,
  `schemars`, `chrono`, `tokio`, `toml`, `tracing-subscriber`, `fastrand`, `anyhow`.
- `src/main.rs` — entry point. `Args` (clap `Parser` + `Deserialize`, kebab-case,
  `deny_unknown_fields`; a TOML `--config` file supplies base values, CLI flags win via
  `Mergeable::merge_weak` from `helpers::derive_merge!`), `tagline()` (random `--help`
  quip), and `main()`: file-based tracing (default `$TMPDIR/work-buddy.log`, since the
  terminal belongs to the UI), assembly of the single system message from `--system` files
  plus the active-task list and a date note, session logging/resuming via `SessionManager`,
  `Agent` construction and tool registration, and running `llimorse_chat::App` with a
  `TermUi`.
- `src/tools/mod.rs` — declares the four tool modules: `knowledge`, `tasks`, `worklog`,
  `write_md`.
- `src/tools/tasks.rs` — task list management. `TaskFile` (a JSON `HashMap<String, Task>`),
  `active_tasks_message()` (rendered into the system prompt, including critical backlog
  items), and `add_tools()` registering the `TaskAdd`, `TaskRemove`, `TaskUpdate`,
  `TaskQuery` tools. Data model: `Task` (settable fields plus `created_at`/`updated_at`),
  `TaskSettable` (status, priority, components, tickets, description), `TaskStatus`
  (Backlog, NotYetTriaged, InProgress, Blocked), `TaskPriority` (Low < Normal < High <
  Critical).
- `src/tools/worklog.rs` — work log. `WorklogDirectory` stores one JSON array file per ISO
  week, named `<iso-year>-w<week>.json`; `WorklogEntry`/`WorklogSettableEntry` (summary,
  effort_minutes, components, root_cause, fix_approach, references, tags, narrative),
  `WorklogReference` (Issue, MergeRequest, Commit), `WorklogTag` (Bug, Review, Feature,
  Investigation, Chore, Meeting, Tests). Tools: `WorklogAdd` and `WorklogQuery` (date
  range, optional tag/component/string filters).
- `src/tools/knowledge.rs` — keyword knowledge base. `KnowledgeFile` is a JSON map of
  keyword to `KnowledgeOrAlias` (`Content(String)` or `Alias(String)`). Tools:
  `KnowledgeUpsert` (create/update entries, add aliases; validates before mutating and
  rejects dead alias links) and `KnowledgeQuery` (look up one keyword, or list all
  keywords).
- `src/tools/write_md.rs` — the `WriteMarkdown` tool: writes a full markdown file under a
  fixed output directory; bare filename only, `File::create_new` so it never overwrites.

## Key APIs

This is a binary crate: nothing outside it can import it, so there is no library surface for
consumers. The load-bearing items, visible within the binary, are the storage types and
their tool-registration methods:

- `tools::tasks::TaskFile::open(path)`, `TaskFile::active_tasks_message()`,
  `TaskFile::add_tools(self, &mut Agent)`
- `tools::worklog::WorklogDirectory::new(path)`, `WorklogDirectory::add_tools(self,
  &mut Agent)`
- `tools::knowledge::KnowledgeFile::open(path)`, `KnowledgeFile::add_tools(self,
  &mut Agent)`
- `tools::write_md::WriteMarkdown::new(dir)` — registered directly with `Agent::add_tool`
- `Args` in `src/main.rs` — the user-facing CLI/config surface

## Fit in the workspace

- Consumers: none — this crate is the example application (a leaf of the workspace).
- `llimorse` — core runtime: `Client` (llama-server), `Agent` (`new_with_listener`,
  `push_system`, `push_history`, `add_tool`), and the `tool!` macro with `CallableTool`,
  used by every tool in `src/tools/`.
- `llimorse-chat` (feature `clap`) — the app framework: `App::new_with_history` +
  `App::run`, `log::SessionManager` (session-log dir, `Resume` selection, exposes the
  `log` listener and `history`), `ui::NotificationChannel`, and
  `Resume::value_parser()` from the `clap` feature.
- `llimorse-tools` — only `WebSearch::new(searxng_url)` is used; the rest of that crate is
  unused here.
- `term-ui` — `TermUi::new("WorkBuddy", agent)` serves as the `UiState` handed to `App`.
- `helpers` — `Mergeable`/`derive_merge!` (CLI-over-config merging),
  `system_files::deserialize` (`--system` may be one path or a list), and
  `TruncatedDisplay` for truncating strings in `Display` impls.
- External — `clap` (CLI), `serde`/`serde_json`/`schemars` (tool parameter schemas, JSON
  files), `chrono` (timestamps, ISO-week naming), `tokio` (runtime, `Mutex`), `toml`
  (config file), `tracing-subscriber` (file logging), `fastrand` (tagline), `anyhow`
  (errors).

## Invariants

- Exactly one system message at the top of the history. `main.rs` concatenates the
  `--system` files, the active-task list, and the date note into a single message on
  purpose: not every chat template handles multiple system messages well.
- On resume (non-empty `SessionManager` history), the history is restored with
  `push_history` and no system message is pushed — the resumed log already contains it.
- The task file is opened (and thus validated) before anything is pushed into the agent,
  so a bad `--tasks` path fails up front.
- Tool state of the stateful tools (tasks, worklog, knowledge) is shared between concurrent tool
  calls via `Arc<tokio::sync::Mutex<...>>`; every mutation rewrites the whole file to disk while
  holding the lock. `WriteMarkdown` keeps a plain `PathBuf` (it only creates new files).
- File formats are single flat JSON documents: `{}` for tasks and knowledge, `[]` per
  worklog week. Missing files are created on first use (`{}` for tasks/knowledge; worklog
  week files are written with the first entry of that week).
- `task_update` semantics: `components` fully replaces the list; `status`/`priority` are
  no-ops when absent; in `tickets` and `description` maps, a `None` value removes the
  key.
- Worklog files are named by ISO week, and `worklog_query` walks from `start_date` to
  `end_date` in seven-day steps.
- `write_markdown` never overwrites existing files and accepts only bare filenames (no
  path components).
- A knowledge keyword is either `Content` or `Alias`, never both; `knowledge_upsert`
  validates all alias conflicts before mutating, and alias chains must resolve to a
  `Content` entry (dead links are errors).
- Every tool follows the same shape: an `llimorse::tool!` block, `Display` impls for the
  params and result (used in logs), state (behind `Arc<Mutex<...>>` for the stateful tools,
  a plain `PathBuf` for `WriteMarkdown`), and a `CallableTool` impl.
- Every item carries a doc comment: the crate enables `#![warn(missing_docs)]` and
  `clippy::missing_docs_in_private_items]`.

## Maintenance

- If you add a tool module under `src/tools/`: declare it in `src/tools/mod.rs`, register
  it in `src/main.rs`, and add a line to the Files section above.
- If you change `Args` (CLI/config surface): update the Files entry for `src/main.rs`.
- If you change a JSON file format (tasks, knowledge, worklog): existing user files are
  parsed at startup — note the format change here.
- If you change what is folded into the single system message in `src/main.rs`: keep the
  one-system-message invariant and update this file.
- If a dependency's API changes in a way that affects the call sites listed in Fit:
  update the Fit section.
