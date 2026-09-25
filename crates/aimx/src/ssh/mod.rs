//! OpenSSH transport, binary bootstrap and agentless remote workspace.

pub mod agentless;
pub mod bootstrap;
pub mod conn;
pub mod forward;
mod reconnect;
pub mod resident;

/// Seam for a future resident `aimx serve` proxy over a multiplexed SSH channel.
pub struct RemoteHarness;

#[cfg(test)]
mod live_tests;

/// POSIX shell single quoting, including embedded apostrophes.
#[must_use]
pub fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    #[test]
    fn quote_round_trip() {
        assert_eq!(super::quote("a'b $HOME"), "'a'\\''b $HOME'");
    }
}
