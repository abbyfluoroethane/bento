use std::collections::HashSet;

use bento_hypervisor::DomainInfo;
use bento_types::Instance;

use crate::{Error, Manager, Result};

/// The disagreement between libvirt and the database (SPEC 6.1). Operators
/// inspect and correct it by hand.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReconcileReport {
    pub domains_without_rows: Vec<DomainInfo>,
    pub rows_without_domains: Vec<Instance>,
}

impl ReconcileReport {
    pub fn is_empty(&self) -> bool {
        self.domains_without_rows.is_empty() && self.rows_without_domains.is_empty()
    }
}

impl Manager {
    /// Compares domains and rows by UUID and reports differences (SPEC 6.1).
    /// It never writes or deletes: a reconciliation bug that deletes a domain
    /// is worse than a wrong row.
    pub async fn reconcile(&self) -> Result<ReconcileReport> {
        let domains = self.hyp.list().await.map_err(|error| {
            Error::operation(format!("lifecycle: reconcile: list domains: {error}"))
        })?;
        let rows = self.store.instances().await.map_err(|error| {
            Error::operation(format!("lifecycle: reconcile: list instances: {error}"))
        })?;
        let row_ids: HashSet<_> = rows.iter().map(|row| row.uuid.clone()).collect();
        let domain_ids: HashSet<_> = domains.iter().map(|domain| domain.uuid.clone()).collect();
        Ok(ReconcileReport {
            domains_without_rows: domains
                .into_iter()
                .filter(|domain| !row_ids.contains(domain.uuid.as_str()))
                .collect(),
            rows_without_domains: rows
                .into_iter()
                .filter(|row| !domain_ids.contains(row.uuid.as_str()))
                .collect(),
        })
    }
}
