//! Runs on the macOS CI runner; compiles to nothing elsewhere.
#![cfg(target_os = "macos")]

use fogar_agent_macos::mac::MacSystemProbe;

fogar_testkit::system_probe_contract!(MacSystemProbe::new().unwrap());
