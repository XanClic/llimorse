# llimorse

A Rust workspace for LLM agent harnesses.

(Currently only for use with llama.cpp’s llama-server.)

Crates in this workspace:
* [llimorse](crates/llimorse): The central crate — llama-server client, agent loop, and the `tool!` macro ([documentation](https://xanclic.github.io/llimorse/llimorse/index.html))
* [llimorse-chat](crates/llimorse-chat): Helpers for building chatbot-style harnesses ([documentation](https://xanclic.github.io/llimorse/llimorse_chat/index.html))
* [llimorse-tools](crates/llimorse-tools): The standard tool set (view/write/edit/bash/web search/subagent) and the `ToolGate` permission abstraction
* [term-ui](crates/term-ui): Terminal interface for such chatbots ([documentation](https://xanclic.github.io/llimorse/term_ui/index.html))
* [work-buddy](crates/work-buddy): Example application for managing day-to-day tasks and a log of work done ([documentation](https://xanclic.github.io/llimorse/work_buddy/index.html))
* [lemon](crates/lemon): An agent harness — a TUI chat app with file, bash, and web-search tools, a subagent tool, and session resuming ([documentation](https://xanclic.github.io/llimorse/lemon/index.html))
* [helpers](crates/helpers): Small shared utilities — config/CLI merging, system-prompt file deserialization, display truncation ([documentation](https://xanclic.github.io/llimorse/helpers/index.html))

Agents working in this repository should start at [AGENTS.md](AGENTS.md), the
entry point of a multi-level agent documentation setup with one detail file
per crate.

Also at the repository root: `lemonade.sh` (entry-point script that runs
`lemon` in a container, see its design document `lemonade.DESIGN.md`), and
the work-buddy system prompts (`SYSTEM_PROMPT.md`, `REPORT_PROMPT.md`).
