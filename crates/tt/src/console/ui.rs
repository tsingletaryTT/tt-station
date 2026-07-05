//! The interactive `tt console` TUI -- Task 7. This module is a placeholder
//! so Task 6 (subcommand wiring + `--snapshot`/`--install-service`) compiles
//! and links cleanly without waiting on the TUI's implementation: everything
//! Task 7 needs ([`crate::console::env::LifecycleEnv`],
//! [`crate::console::names::ToolNames`], [`crate::console::actions::LifecycleActions`],
//! [`crate::console::env::collect_snapshot`]) already exists from Tasks 1-5,
//! so [`run_tui`]'s signature here is meant to be Task 7's real entry point,
//! not just a throwaway stub signature it'll have to change.
//!
//! `allow(dead_code)` on the params: this stub doesn't touch its inputs, but
//! the real Task 7 body will use all three, so they're named/typed to match
//! what that implementation needs rather than being erased to `_`.
#![allow(dead_code)]

use crate::console::env::LifecycleEnv;
use crate::console::names::ToolNames;

/// Launch the interactive operator TUI. Currently a stub: prints a one-line
/// note and returns `Ok(())` instead of drawing anything, so `tt console`
/// (no flags) is a harmless no-op until Task 7 lands rather than a build
/// failure or a panic.
pub fn run_tui(_env: &dyn LifecycleEnv, _names: &ToolNames, _ctrl_port: u16) -> anyhow::Result<()> {
    println!("tt console: interactive TUI arrives in Task 7 (use --snapshot or --install-service for now)");
    Ok(())
}
