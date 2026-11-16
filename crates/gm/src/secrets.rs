//! Secrets live in the macOS Keychain (service `google-management`), ADR-0013.
//!
//! Lookup order for a secret named `master-key`:
//! 1. environment variable `GM_SECRET_MASTER_KEY` (tests, CI, non-macOS)
//! 2. Keychain item: service `google-management`, account `master-key`
//!
//! Values are never logged.

use std::process::{Command, Stdio};

pub const SERVICE: &str = "google-management";

/// Every secret gm knows about, with a one-line description for `gm keychain list`.
pub const KNOWN: &[(&str, &str)] = &[
    (
        "master-key",
        "Encrypts Google refresh tokens (generate with `gm gen-key`)",
    ),
    (
        "session-secret",
        "Signs the web session cookie (generate with `gm gen-key`)",
    ),
    (
        "google-client-secret",
        "OAuth client secret from Google Cloud Console",
    ),
    ("telegram-bot-token", "Bot token from @BotFather (optional)"),
    ("metrics-token", "Bearer token for /metrics (optional)"),
];

pub fn env_name(name: &str) -> String {
    format!("GM_SECRET_{}", name.to_uppercase().replace('-', "_"))
}

pub fn get(name: &str) -> Option<String> {
    if let Ok(v) = std::env::var(env_name(name)) {
        let v = v.trim().to_string();
        if !v.is_empty() {
            return Some(v);
        }
    }
    keychain_get(name)
}

fn keychain_get(name: &str) -> Option<String> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let out = Command::new("/usr/bin/security")
        .args(["find-generic-password", "-s", SERVICE, "-a", name, "-w"])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let v = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!v.is_empty()).then_some(v)
}

/// Store a secret. `security` prompts for the value on the terminal, so it never appears in argv
/// or shell history.
pub fn keychain_set_interactive(name: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        cfg!(target_os = "macos"),
        "the Keychain is only available on macOS"
    );
    let status = Command::new("/usr/bin/security")
        .args([
            "add-generic-password",
            "-U",
            "-s",
            SERVICE,
            "-a",
            name,
            "-w",
        ])
        .status()?;
    anyhow::ensure!(status.success(), "security add-generic-password failed");
    Ok(())
}

pub fn keychain_has(name: &str) -> bool {
    keychain_get(name).is_some()
}
