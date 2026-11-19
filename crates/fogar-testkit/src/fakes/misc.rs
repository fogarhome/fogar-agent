use fogar_ports::{
    Support,
    power::{Applied, PowerCaps, PowerError, PowerPolicy, WorkClass},
    random::Random,
    secrets::{ExposeSecret, SecretCaps, SecretError, SecretKey, SecretStore, SecretString},
    tailnet::{TailnetError, TailnetPeers},
};
use std::net::IpAddr;
use std::{
    collections::HashMap,
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

/// Deterministic xorshift64* generator (NOT cryptographic; jitter only).
#[derive(Debug)]
pub struct SeededRandom(AtomicU64);

impl SeededRandom {
    pub fn new(seed: u64) -> Self {
        Self(AtomicU64::new(seed.max(1)))
    }
}

impl Random for SeededRandom {
    fn next_u64(&self) -> u64 {
        let mut x = self.0.load(Ordering::Relaxed);
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0.store(x, Ordering::Relaxed);
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
}

/// In-memory secret store. `fail_next` injects `Unavailable` (locked keychain).
#[derive(Debug, Default)]
pub struct MemorySecretStore {
    map: Mutex<HashMap<SecretKey, SecretString>>,
    fail: Mutex<u32>,
}

impl MemorySecretStore {
    pub fn with_client_key(key: &str) -> Self {
        let s = Self::default();
        s.put(
            SecretKey::GatewayClientKey,
            SecretString::from(key.to_owned()),
        )
        .unwrap();
        s
    }
    pub fn fail_next(&self, n: u32) {
        *self.fail.lock().unwrap() = n;
    }
    pub fn contains(&self, key: SecretKey) -> bool {
        self.map.lock().unwrap().contains_key(&key)
    }
}

impl SecretStore for MemorySecretStore {
    fn capabilities(&self) -> SecretCaps {
        let mut c = SecretCaps::NONE;
        c.persistent = Support::Unsupported;
        c
    }
    fn get(&self, key: SecretKey) -> Result<Option<SecretString>, SecretError> {
        {
            let mut f = self.fail.lock().unwrap();
            if *f > 0 {
                *f -= 1;
                return Err(SecretError::Unavailable);
            }
        }
        Ok(self
            .map
            .lock()
            .unwrap()
            .get(&key)
            .map(|s| SecretString::from(s.expose_secret().to_owned())))
    }
    fn put(&self, key: SecretKey, value: SecretString) -> Result<(), SecretError> {
        self.map.lock().unwrap().insert(key, value);
        Ok(())
    }
    fn delete(&self, key: SecretKey) -> Result<(), SecretError> {
        self.map.lock().unwrap().remove(&key);
        Ok(())
    }
}

/// Records every policy call; `minimal()` reports everything Unsupported.
#[derive(Debug)]
pub struct FakePowerPolicy {
    caps: PowerCaps,
    pub calls: Mutex<Vec<WorkClass>>,
    pub baseline_calls: AtomicU64,
}

impl FakePowerPolicy {
    pub fn healthy() -> Self {
        let mut caps = PowerCaps::NONE;
        caps.thread_class = Support::Native;
        caps.process_baseline = Support::Native;
        Self {
            caps,
            calls: Mutex::default(),
            baseline_calls: AtomicU64::new(0),
        }
    }
    pub fn minimal() -> Self {
        Self {
            caps: PowerCaps::NONE,
            calls: Mutex::default(),
            baseline_calls: AtomicU64::new(0),
        }
    }
}

impl PowerPolicy for FakePowerPolicy {
    fn capabilities(&self) -> PowerCaps {
        self.caps
    }
    fn apply_process_baseline(&self) -> Result<Applied, PowerError> {
        self.baseline_calls.fetch_add(1, Ordering::Relaxed);
        Ok(if self.caps.process_baseline.is_available() {
            Applied::Applied
        } else {
            Applied::NotSupported
        })
    }
    fn apply_current_thread(&self, class: WorkClass) -> Result<Applied, PowerError> {
        self.calls.lock().unwrap().push(class);
        Ok(if self.caps.thread_class.is_available() {
            Applied::Applied
        } else {
            Applied::NotSupported
        })
    }
}

/// Scripted tailnet: a fixed answer, or one that never comes (`hanging`, for the core's deadline).
#[derive(Debug)]
pub struct FakeTailnet {
    /// `None` = never answers.
    answer: Option<Result<Vec<IpAddr>, TailnetError>>,
    caps: Support,
    /// Shared: keep a clone before moving the fake into the agent.
    pub calls: std::sync::Arc<AtomicU64>,
}

impl FakeTailnet {
    pub fn with_peers(ips: &[&str]) -> Self {
        let ips = ips.iter().map(|s| s.parse().expect("ip")).collect();
        Self {
            answer: Some(Ok(ips)),
            caps: Support::Native,
            calls: std::sync::Arc::default(),
        }
    }
    pub fn unsupported() -> Self {
        Self {
            answer: Some(Err(TailnetError::Unavailable)),
            caps: Support::Unsupported,
            calls: std::sync::Arc::default(),
        }
    }
    pub fn not_running() -> Self {
        Self {
            answer: Some(Err(TailnetError::NotRunning)),
            caps: Support::Native,
            calls: std::sync::Arc::default(),
        }
    }
    pub fn hanging() -> Self {
        Self {
            answer: None,
            caps: Support::Native,
            calls: std::sync::Arc::default(),
        }
    }
}

impl TailnetPeers for FakeTailnet {
    fn capabilities(&self) -> Support {
        self.caps
    }
    async fn peers(&self) -> Result<Vec<IpAddr>, TailnetError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        match &self.answer {
            Some(a) => a.clone(),
            None => std::future::pending().await,
        }
    }
}
