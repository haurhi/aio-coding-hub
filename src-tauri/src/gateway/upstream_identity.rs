//! Non-secret upstream identity markers used by gateway OAuth compatibility.

pub(crate) const CODEX_CLI_ORIGINATOR: &str = "codex_cli_rs";
pub(crate) const CODEX_CLI_USER_AGENT: &str = "codex_cli_rs/0.137.0";

pub(crate) const CLAUDE_CODE_USER_AGENT: &str = "claude-code/2.1.168";
pub(crate) const CLAUDE_STAINLESS_PACKAGE_VERSION: &str = "0.70.0";
pub(crate) const CLAUDE_STAINLESS_RUNTIME: &str = "node";
pub(crate) const CLAUDE_STAINLESS_LANG: &str = "js";

pub(crate) fn claude_stainless_os() -> &'static str {
    if cfg!(target_os = "windows") {
        "Windows"
    } else if cfg!(target_os = "macos") {
        "macOS"
    } else {
        "Linux"
    }
}

pub(crate) fn claude_stainless_arch() -> &'static str {
    if cfg!(target_arch = "x86_64") {
        "x64"
    } else if cfg!(target_arch = "aarch64") {
        "arm64"
    } else if cfg!(target_arch = "x86") {
        "ia32"
    } else if cfg!(target_arch = "arm") {
        "arm"
    } else {
        "unknown"
    }
}
