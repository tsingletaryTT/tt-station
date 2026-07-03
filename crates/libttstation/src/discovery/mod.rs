//! Discovery module: defines the `DiscoveryProvider` trait that every
//! box-discovery mechanism (manual host list, mDNS, ...) implements, plus
//! `aggregate()` to run a set of providers and merge their results.

pub mod manual;

use crate::model::BoxRecord;
use std::time::Duration;

/// A source of `BoxRecord`s (e.g. a manual host list, mDNS browsing, ...).
///
/// Implementations should do their best to respect `timeout`, but
/// `aggregate` does not enforce it itself -- that's on each provider.
pub trait DiscoveryProvider {
    fn discover(&self, timeout: Duration) -> anyhow::Result<Vec<BoxRecord>>;
}

/// Run every provider, collect all discovered boxes, and dedup by `name`.
///
/// A provider that errors does not fail the whole aggregation: the error is
/// logged to stderr and that provider simply contributes no records.
pub fn aggregate(providers: &[Box<dyn DiscoveryProvider>], timeout: Duration) -> Vec<BoxRecord> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();

    for provider in providers {
        match provider.discover(timeout) {
            Ok(records) => {
                for rec in records {
                    if seen.insert(rec.name.clone()) {
                        out.push(rec);
                    }
                }
            }
            Err(e) => {
                eprintln!("discovery provider failed: {e:#}");
            }
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ServingStatus;

    struct Fake(Vec<BoxRecord>);
    impl DiscoveryProvider for Fake {
        fn discover(&self, _t: std::time::Duration) -> anyhow::Result<Vec<BoxRecord>> {
            Ok(self.0.clone())
        }
    }

    #[test]
    fn aggregate_dedups_by_name() {
        let r = BoxRecord {
            name: "qb2".into(),
            host: "qb2.local".into(),
            ctrl_port: 8765,
            chips: "4xBH".into(),
            status: ServingStatus::Idle,
            apiver: 1,
        };
        let providers: Vec<Box<dyn DiscoveryProvider>> = vec![
            Box::new(Fake(vec![r.clone()])),
            Box::new(Fake(vec![r.clone()])),
        ];
        let out = aggregate(&providers, std::time::Duration::from_millis(10));
        assert_eq!(out.len(), 1);
    }
}
