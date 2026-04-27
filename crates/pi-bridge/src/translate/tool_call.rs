//! Classify pi tool names into the codex `ThreadItem` kind we should emit.
//!
//! Pi exposes a small set of built-in tools (`bash`, `read`, `write`, `edit`,
//! `grep`, `ls`, `find` — see `pi-mono/packages/coding-agent/src/core/tools/`)
//! plus MCP-exposed tools whose names follow the `<server>__<tool>` convention
//! and arbitrary user/extension-registered tools.
//!
//! Codex models tool calls with several `ThreadItem` variants
//! (`app-server-protocol/src/protocol/v2.rs:5327`):
//!
//! | pi tool name             | codex `ThreadItem` kind |
//! |--------------------------|-------------------------|
//! | `bash`                   | `CommandExecution`      |
//! | `write`, `edit`          | `FileChange`            |
//! | `<server>__<tool>` (MCP) | `McpToolCall`           |
//! | anything else            | `DynamicToolCall`       |
//!
//! `read`, `grep`, `ls`, `find` are read-only inspection tools and have no
//! file-mutation surface in codex; they fall through to `DynamicToolCall`
//! (codex's catch-all for unknown agent tools).
//!
//! The MCP convention `<server>__<tool>` matches pi's MCP bridge naming
//! (double-underscore separator). When matching, we also accept the
//! single-underscore `<server>_<tool>` form some MCP clients emit, but only
//! when the bridge has registered the server name — for now we conservatively
//! match the double-underscore form only, since pi's own MCP integration uses
//! it.

/// Coarse codex item kind a given pi `toolName` should be promoted to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodexToolKind {
    /// Pi `bash` — codex `ThreadItem::CommandExecution`.
    CommandExecution,

    /// Pi `write` / `edit` — codex `ThreadItem::FileChange`.
    FileChange,

    /// Pi MCP bridge tool call. The original pi name was `<server>__<tool>`
    /// and is split on the first `__` separator.
    Mcp { server: String, tool: String },

    /// Anything else — codex `ThreadItem::DynamicToolCall`. The optional
    /// `namespace` is the substring before the first `__` if present (so the
    /// codex client can group dynamic tool calls by an outer scope).
    Dynamic {
        namespace: Option<String>,
        tool: String,
    },
}

/// Tool names recognized as file-mutation operations.
///
/// Pi's built-in mutators are `write` (full-file replacement) and `edit`
/// (search-and-replace patch). `apply_patch` is included to forward-compat
/// with future pi unified-diff support — pi's tool registry does not ship
/// it today, but if a custom tool with that name appears we treat it as a
/// file change.
const FILE_CHANGE_TOOLS: &[&str] = &["write", "edit", "apply_patch"];

/// Classify the pi `toolName` string into a codex item kind.
///
/// `bash` is matched case-sensitively; everything else is too. Pi tool names
/// are always lowercase ASCII identifiers in practice, so we don't lower-case
/// here (case mismatches indicate a wire-format bug we'd rather surface).
pub fn classify(tool_name: &str) -> CodexToolKind {
    if tool_name == "bash" {
        return CodexToolKind::CommandExecution;
    }
    if FILE_CHANGE_TOOLS.contains(&tool_name) {
        return CodexToolKind::FileChange;
    }
    if let Some((server, tool)) = split_mcp(tool_name) {
        return CodexToolKind::Mcp {
            server: server.to_string(),
            tool: tool.to_string(),
        };
    }
    CodexToolKind::Dynamic {
        namespace: None,
        tool: tool_name.to_string(),
    }
}

/// Split a `<server>__<tool>` MCP-style tool name into its parts.
///
/// Returns `None` for names without `__` or where either side is empty
/// (e.g. `__foo`, `foo__`, `__`).
fn split_mcp(tool_name: &str) -> Option<(&str, &str)> {
    let (server, tool) = tool_name.split_once("__")?;
    if server.is_empty() || tool.is_empty() {
        return None;
    }
    Some((server, tool))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bash_is_command_execution() {
        assert_eq!(classify("bash"), CodexToolKind::CommandExecution);
    }

    #[test]
    fn write_and_edit_are_file_change() {
        assert_eq!(classify("write"), CodexToolKind::FileChange);
        assert_eq!(classify("edit"), CodexToolKind::FileChange);
        assert_eq!(classify("apply_patch"), CodexToolKind::FileChange);
    }

    #[test]
    fn read_grep_ls_find_are_dynamic() {
        for name in ["read", "grep", "ls", "find"] {
            assert_eq!(
                classify(name),
                CodexToolKind::Dynamic {
                    namespace: None,
                    tool: name.to_string()
                },
                "{name} should be dynamic"
            );
        }
    }

    #[test]
    fn double_underscore_separates_mcp() {
        assert_eq!(
            classify("github__create_issue"),
            CodexToolKind::Mcp {
                server: "github".into(),
                tool: "create_issue".into(),
            }
        );
    }

    #[test]
    fn mcp_tool_name_with_inner_underscore_is_preserved() {
        // Only the *first* `__` splits — the tool half can keep underscores.
        assert_eq!(
            classify("github__list__pull_requests"),
            CodexToolKind::Mcp {
                server: "github".into(),
                tool: "list__pull_requests".into(),
            }
        );
    }

    #[test]
    fn malformed_mcp_falls_through_to_dynamic() {
        for name in ["__foo", "foo__", "__"] {
            match classify(name) {
                CodexToolKind::Dynamic { tool, .. } => assert_eq!(tool, name),
                other => panic!("expected dynamic for {name:?}, got {other:?}"),
            }
        }
    }

    #[test]
    fn empty_name_is_dynamic() {
        assert_eq!(
            classify(""),
            CodexToolKind::Dynamic {
                namespace: None,
                tool: String::new(),
            }
        );
    }

    #[test]
    fn case_sensitive_match() {
        // BASH is not bash. Surfacing it as dynamic preserves the original
        // wire name and avoids silently rewriting a pi protocol violation.
        assert_eq!(
            classify("BASH"),
            CodexToolKind::Dynamic {
                namespace: None,
                tool: "BASH".to_string(),
            }
        );
    }
}
