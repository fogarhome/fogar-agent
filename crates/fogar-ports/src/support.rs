/// Capability level an adapter declares for one feature of a port.
///
/// Lets an adapter say "not supported" without failing, and lets the core pick a
/// strategy (e.g. poll slower when a value is only `Emulated` by polling).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Support {
    /// Backed by a native OS facility (push notifications, cheap syscalls).
    Native,
    /// Works, but by emulation (e.g. polling) - costs more energy or is less precise.
    Emulated,
    /// Not available on this platform/adapter. Calls return `None`/`Unsupported`.
    Unsupported,
}

impl Support {
    pub fn is_available(self) -> bool {
        !matches!(self, Support::Unsupported)
    }
}
