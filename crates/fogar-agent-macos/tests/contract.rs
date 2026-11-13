//! UNTESTED until the macOS CI job runs: the macOS adapters against the shared contract suites.
#![cfg(target_os = "macos")]

use fogar_agent_macos::*;

fogar_testkit::system_probe_contract!(MacSystemProbe);
fogar_testkit::audio_source_contract!(MacAudioSource);
fogar_testkit::audio_sink_contract!(MacAudioSink);
fogar_testkit::power_policy_contract!(MacPowerPolicy);
