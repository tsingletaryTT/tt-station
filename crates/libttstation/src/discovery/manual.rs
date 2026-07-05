//! `ManualProvider`: discovery via a user-supplied list of hosts.
//!
//! The provider doesn't do network I/O itself -- it delegates the actual
//! "probe this host for its status" step to a closure, so it can be
//! unit-tested without a real box on the LAN. In production the closure
//! wraps a `GET /status` HTTP call against the agent.

use super::DiscoveryProvider;
use crate::model::BoxRecord;
use std::time::Duration;

/// Discovers boxes from a fixed, user-supplied `(host, ctrl_port)` list.
///
/// `fetch` is called once per host and is expected to probe that host's
/// `GET /status` endpoint and parse the response into a `BoxRecord`. Taking
/// `fetch` as a generic closure (rather than hardcoding an HTTP client here)
/// keeps this provider unit-testable without any real network I/O.
pub struct ManualProvider<F>
where
    F: Fn(&str, u16) -> anyhow::Result<BoxRecord>,
{
    pub hosts: Vec<(String, u16)>,
    pub fetch: F,
}

impl<F> ManualProvider<F>
where
    F: Fn(&str, u16) -> anyhow::Result<BoxRecord>,
{
    pub fn new(hosts: Vec<(String, u16)>, fetch: F) -> Self {
        Self { hosts, fetch }
    }
}

impl<F> DiscoveryProvider for ManualProvider<F>
where
    F: Fn(&str, u16) -> anyhow::Result<BoxRecord>,
{
    /// Probe every configured host. `timeout` is accepted for interface
    /// parity with other providers; enforcing it per-host is the
    /// caller-supplied `fetch` closure's responsibility (e.g. the real HTTP
    /// client sets a request timeout).
    fn discover(&self, _timeout: Duration) -> anyhow::Result<Vec<BoxRecord>> {
        let mut out = Vec::new();
        for (host, port) in &self.hosts {
            match (self.fetch)(host, *port) {
                Ok(rec) => out.push(rec),
                Err(e) => {
                    eprintln!("manual probe of {host}:{port} failed: {e:#}");
                }
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::super::DiscoveryProvider;
    use super::ManualProvider;
    use crate::model::ServingStatus;
    use std::time::Duration;

    #[test]
    fn manual_provider_probes_each_host_via_fetch_closure() {
        let provider = ManualProvider::new(vec![("qb2.local".to_string(), 8765)], |host, port| {
            Ok(crate::model::BoxRecord {
                name: "qb2".into(),
                host: host.to_string(),
                ctrl_port: port,
                chips: "4xBH".into(),
                status: ServingStatus::Idle,
                apiver: 1,
                device_mesh: Some("p300x2".into()),
            })
        });

        let out = provider.discover(Duration::from_millis(10)).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "qb2");
        assert_eq!(out[0].host, "qb2.local");
        assert_eq!(out[0].ctrl_port, 8765);
    }

    #[test]
    fn manual_provider_skips_hosts_whose_fetch_errors() {
        let provider = ManualProvider::new(
            vec![
                ("bad.local".to_string(), 8765),
                ("good.local".to_string(), 8765),
            ],
            |host, port| {
                if host == "bad.local" {
                    Err(anyhow::anyhow!("connection refused"))
                } else {
                    Ok(crate::model::BoxRecord {
                        name: "good".into(),
                        host: host.to_string(),
                        ctrl_port: port,
                        chips: "1xBH".into(),
                        status: ServingStatus::Idle,
                        apiver: 1,
                        device_mesh: None,
                    })
                }
            },
        );

        let out = provider.discover(Duration::from_millis(10)).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "good");
    }
}
