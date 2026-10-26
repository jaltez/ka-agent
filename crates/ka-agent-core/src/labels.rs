//! Human labels for tool calls: one place that turns `tool` + `detail`
//! into the short phrase surfaces show while a call runs.

/// A one-line, human label for a tool call (SEED.md §5.4 wording).
pub fn tool_label(tool: &str, detail: &str) -> String {
    let detail = detail.trim();
    match tool {
        "web_search" => "searching the web".into(),
        "web_fetch" => format!(
            "reading {}",
            url_host(detail).unwrap_or_else(|| detail.into())
        ),
        "read" | "glob" | "grep" => "looking through files".into(),
        "write" | "edit" => format!("editing {}", file_name(detail)),
        "bash" => "running a command".into(),
        "pathfinder" | "delegate" => "delegating research".into(),
        "kagent__memory_propose" => "noting a memory".into(),
        "kagent__remind" => "setting a reminder".into(),
        "kagent__send_file" => "sending a file".into(),
        "kagent__calendar_events" => "checking the calendar".into(),
        other => {
            if let Some((server, rest)) = other.split_once("__") {
                if rest.starts_with("history_") {
                    "checking our history".into()
                } else {
                    format!("using {server}")
                }
            } else {
                "working".into()
            }
        }
    }
}

/// The host of `detail` when it parses as an HTTP(S) URL.
fn url_host(detail: &str) -> Option<String> {
    let rest = detail
        .strip_prefix("https://")
        .or_else(|| detail.strip_prefix("http://"))?;
    let host = rest.split(['/', '?', '#']).next()?.split(':').next()?;
    let host = host.trim();
    (!host.is_empty()).then(|| host.to_string())
}

/// The last path segment of `detail` (a file name), if any.
fn file_name(detail: &str) -> String {
    let name = detail.rsplit(['/', '\\']).next().unwrap_or(detail).trim();
    if name.is_empty() {
        "a file".into()
    } else {
        name.to_string()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::tool_label;

    #[test]
    fn each_tool_gets_its_label() {
        assert_eq!(tool_label("web_search", "rust news"), "searching the web");
        assert_eq!(
            tool_label("web_fetch", "https://example.com/a?b"),
            "reading example.com"
        );
        assert_eq!(tool_label("web_fetch", "not a url"), "reading not a url");
        assert_eq!(tool_label("read", "src/main.rs"), "looking through files");
        assert_eq!(tool_label("glob", "**/*.rs"), "looking through files");
        assert_eq!(tool_label("grep", "needle"), "looking through files");
        assert_eq!(tool_label("write", "src/lib.rs"), "editing lib.rs");
        assert_eq!(
            tool_label("edit", "docs/deep/nested.md"),
            "editing nested.md"
        );
        assert_eq!(tool_label("bash", "cargo build"), "running a command");
        assert_eq!(tool_label("pathfinder", "x"), "delegating research");
        assert_eq!(tool_label("delegate", "x"), "delegating research");
        assert_eq!(
            tool_label("kagent__history_search", "x"),
            "checking our history"
        );
        assert_eq!(
            tool_label("kagent__history_show", "x"),
            "checking our history"
        );
        assert_eq!(tool_label("kagent__memory_propose", "x"), "noting a memory");
        assert_eq!(tool_label("kagent__remind", "x"), "setting a reminder");
        assert_eq!(tool_label("kagent__send_file", "x"), "sending a file");
        assert_eq!(
            tool_label("kagent__calendar_events", "x"),
            "checking the calendar"
        );
        assert_eq!(tool_label("mcp__github", "x"), "using mcp");
        assert_eq!(tool_label("something_else", "x"), "working");
    }
}
