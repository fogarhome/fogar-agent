#![cfg(target_os = "linux")]

use fogar_agent_linux::ProcfsProbe;

fogar_testkit::system_probe_contract!(ProcfsProbe::new().unwrap());
