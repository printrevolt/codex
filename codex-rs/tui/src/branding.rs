pub(crate) const DEFAULT_TUI_TITLE: &str = "OpenAI Codex";

pub(crate) fn tui_title() -> String {
    match std::env::var("CODEX_TUI_TITLE") {
        Ok(value) if !value.trim().is_empty() => value,
        _ => DEFAULT_TUI_TITLE.to_string(),
    }
}
