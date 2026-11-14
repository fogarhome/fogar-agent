//! macOS adapters. Everything platform-specific is behind ONE cfg at module level
//! (same technique as zed's `gpui_macos`: `#![cfg(target_os = "macos")]` at crate root).
//!
//! NOTE: the `mac` module is NOT compile-checked on Linux (cfg'd out). It runs its
//! contract tests on the macOS CI runner.

#[cfg(target_os = "macos")]
pub mod mac;
