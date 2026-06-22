//! OTel meter instruments for simulacra-integration.
//!
//! `IntegrationMeters` is created per `IntegrationRegistry` instance so the
//! observable gauge callback can capture the live credential list and
//! dynamically report how many integrations are non-degraded.

use std::sync::Arc;

use opentelemetry::metrics::{Counter, ObservableGauge};

use crate::types::IntegrationCredential;

/// OTel instruments for the integration layer.
pub struct IntegrationMeters {
    /// Credential injections broken down by `integration` label.
    pub credential_injections: Counter<u64>,
    /// OAuth2 token refresh failures broken down by `integration` label.
    pub refresh_failures: Counter<u64>,
    /// Keeps the active-integration gauge registration alive.
    _active_gauge: ObservableGauge<i64>,
}

impl IntegrationMeters {
    /// Create instruments backed by the global meter provider.
    ///
    /// The `credentials` slice is captured by the observable gauge callback
    /// so it can dynamically count non-degraded integrations on each export.
    pub fn new(credentials: Arc<Vec<Arc<IntegrationCredential>>>) -> Self {
        let meter = opentelemetry::global::meter("simulacra-integration");

        let creds_cb = credentials.clone();
        let _active_gauge = meter
            .i64_observable_gauge("simulacra.integration.active")
            .with_description("Number of healthy (non-degraded) integrations")
            .with_callback(move |observer| {
                let count = creds_cb.iter().filter(|c| !c.is_degraded()).count() as i64;
                observer.observe(count, &[]);
            })
            .build();

        IntegrationMeters {
            credential_injections: meter
                .u64_counter("simulacra.integration.credential_injections")
                .with_description("Credential injections by integration")
                .build(),
            refresh_failures: meter
                .u64_counter("simulacra.integration.refresh_failures")
                .with_description("OAuth2 token refresh failures by integration")
                .build(),
            _active_gauge,
        }
    }
}
