const AMP_BRIDGE: &str = include_str!("../../amp-bridge/src/bridge.rs");
const CLAUDE_BRIDGE: &str = include_str!("../../claude-bridge/src/bridge.rs");
const DROID_BRIDGE: &str = include_str!("../../droid-bridge/src/bridge.rs");
const PI_BRIDGE: &str = include_str!("../../pi-bridge/src/bridge.rs");
const OPENCODE_BRIDGE: &str = include_str!("../../opencode-bridge/src/handlers/mod.rs");
const HERMES_BRIDGE: &str = include_str!("../../hermes-bridge/src/bridge.rs");

const STANDARD_REQUEST_METHODS: &[&str] = &[
    "account/read",
    "account/rateLimits/read",
    "account/login/start",
    "account/login/cancel",
    "account/logout",
    "feedback/upload",
    "config/read",
    "config/value/write",
    "config/batchWrite",
    "configRequirements/read",
    "mcpServerStatus/list",
    "config/mcpServer/reload",
    "mcpServer/oauth/login",
    "mock/experimentalMethod",
    "experimentalFeature/list",
    "collaborationMode/list",
    "model/list",
    "skills/list",
    "skills/remote/list",
    "skills/remote/export",
    "skills/config/write",
    "command/exec",
    "command/exec/terminate",
    "command/exec/write",
    "command/exec/resize",
    "thread/start",
    "thread/resume",
    "thread/fork",
    "thread/archive",
    "thread/unarchive",
    "thread/name/set",
    "thread/compact/start",
    "thread/rollback",
    "thread/list",
    "thread/loaded/list",
    "thread/read",
    "thread/turns/list",
    "thread/backgroundTerminals/clean",
    "turn/start",
    "turn/steer",
    "turn/interrupt",
    "review/start",
];

#[test]
fn all_jsonrpc_bridges_account_for_the_standard_method_surface() {
    for (bridge_name, source) in [
        ("claude", CLAUDE_BRIDGE),
        ("amp", AMP_BRIDGE),
        ("droid", DROID_BRIDGE),
        ("pi", PI_BRIDGE),
        ("opencode", OPENCODE_BRIDGE),
        ("hermes", HERMES_BRIDGE),
    ] {
        for method in STANDARD_REQUEST_METHODS {
            assert!(
                source.contains(&format!("\"{method}\"")),
                "{bridge_name} bridge does not account for JSON-RPC method {method}"
            );
        }
    }
}

#[test]
fn all_jsonrpc_bridges_own_initialize_separately_from_dispatch() {
    for (bridge_name, source) in [
        ("claude", CLAUDE_BRIDGE),
        ("amp", AMP_BRIDGE),
        ("droid", DROID_BRIDGE),
        ("pi", PI_BRIDGE),
        ("opencode", OPENCODE_BRIDGE),
        ("hermes", HERMES_BRIDGE),
    ] {
        assert!(
            source.contains("async fn initialize"),
            "{bridge_name} bridge is missing Bridge::initialize"
        );
    }
}
