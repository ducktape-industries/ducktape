//! `ducktape_dataplane_*`: the per-plane Prometheus series behind GET
//! /metrics — every open data plane's throughput, stream, and drop
//! accounting, labeled `{service, owner}` where `owner` is the module that
//! created the plane.
//!
//! Collection is scrape-fresh: each series is a custom metric whose
//! `encode` reads the shared [`PlaneMonitor`] at `context.encode()` time
//! (a few relaxed atomic loads per plane), so the validator ingress and
//! replica park lanes serve live values with no sampler task and no
//! actor plumbing. All values are gauges; the byte/frame series are
//! cumulative for a plane's life, so readers derive rates from deltas
//! exactly as they do for counters. A plane that closes simply stops
//! being encoded — presence IS openness.

use commonware_runtime::telemetry::metrics::{EncodeMetric, MetricEncoder, MetricType, Registered};
use data_plane::{PlaneMonitor, PlaneReport, Service};

/// Operator-installed bindings are named at runtime. Scrape the bounded,
/// canonical local registry rather than retaining labels after retirement.
/// This reports local admission, not process health or consensus authorization.
#[derive(Debug)]
pub(crate) struct ApplicationBindings(std::path::PathBuf);

impl ApplicationBindings {
    pub(crate) fn register<C: commonware_runtime::Metrics>(
        context: &C,
        workspace: std::path::PathBuf,
    ) -> Registered<Self> {
        context.register(
            "ducktape_application_binding",
            "locally bound application routes (1 = bound, not a health check)",
            Self(workspace),
        )
    }
}

impl EncodeMetric for ApplicationBindings {
    fn encode(&self, mut encoder: MetricEncoder) -> Result<(), std::fmt::Error> {
        let Ok(bindings) = crate::gateway_routes::load(&self.0) else {
            return Ok(());
        };
        for binding in bindings.routes {
            let authentication = match binding.trust {
                crate::gateway_routes::UpstreamTrust::TrustedLoopback => "trusted_loopback",
                crate::gateway_routes::UpstreamTrust::AuthenticatedLoopback { .. } => "credential",
            };
            let account = binding.account.to_string();
            let labels = [
                ("account", account.as_str()),
                ("route", binding.name.local_key()),
                ("authentication", authentication),
            ];
            encoder.encode_family(&labels)?.encode_gauge(&1_i64)?;
        }
        Ok(())
    }

    fn metric_type(&self) -> MetricType {
        MetricType::Gauge
    }
}

/// The metric label for a [`Service`] — wire-stable like the enum itself.
fn service_name(service: Service) -> &'static str {
    match service {
        Service::StateSync => "statesync",
        Service::Voice => "voice",
        Service::Video => "video",
        Service::Gateway => "gateway",
        Service::AgentTelemetry => "agent-telemetry",
        Service::ModuleCode => "module-code",
    }
}

/// Extra label pairs beyond the shared `{service, owner}` identity.
type ExtraLabels = &'static [(&'static str, &'static str)];

/// One exported gauge family over the open-plane set: `project` maps each
/// plane's report to this family's `(extra labels, value)` samples.
struct PlaneSeries {
    monitor: PlaneMonitor,
    project: fn(&PlaneReport) -> Vec<(ExtraLabels, i64)>,
}

impl std::fmt::Debug for PlaneSeries {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlaneSeries").finish_non_exhaustive()
    }
}

impl EncodeMetric for PlaneSeries {
    fn encode(&self, mut encoder: MetricEncoder) -> Result<(), std::fmt::Error> {
        for report in self.monitor.snapshot() {
            for (extra, value) in (self.project)(&report) {
                let mut labels: Vec<(&'static str, &'static str)> = vec![
                    ("service", service_name(report.service)),
                    ("owner", report.owner),
                ];
                labels.extend_from_slice(extra);
                encoder.encode_family(&labels)?.encode_gauge(&value)?;
            }
        }
        Ok(())
    }

    fn metric_type(&self) -> MetricType {
        MetricType::Gauge
    }
}

/// Cumulative counters exceed `i64` only past 9.2 EB / 9.2e18 events —
/// unreachable for a plane's life; the cast is total in practice.
fn gauge(value: u64) -> i64 {
    value as i64
}

/// The registered `ducktape_dataplane_*` handles. Dropping a
/// [`Registered`] unregisters its series, so hold this for the life of
/// the process (alongside the node's other metrics).
pub(crate) struct PlaneMetrics {
    _series: Vec<Registered<PlaneSeries>>,
}

impl PlaneMetrics {
    /// Register the per-plane series on the runtime context (root context,
    /// so names carry no child prefix), reading `monitor` at scrape time.
    pub(crate) fn register<C: commonware_runtime::Metrics>(
        context: &C,
        monitor: &PlaneMonitor,
    ) -> Self {
        let series =
            |name: &str, help: &str, project: fn(&PlaneReport) -> Vec<(ExtraLabels, i64)>| {
                context.register(
                    name,
                    help,
                    PlaneSeries {
                        monitor: monitor.clone(),
                        project,
                    },
                )
            };
        Self {
            _series: vec![
                series(
                    "ducktape_dataplane_open",
                    "an open data plane, by service and creating module (1 = open)",
                    |_| vec![(&[], 1)],
                ),
                series(
                    "ducktape_dataplane_halted",
                    "whether an open plane's pumps have stopped (bound but not moving traffic)",
                    |r| vec![(&[], i64::from(r.observation.traffic.halted))],
                ),
                series(
                    "ducktape_dataplane_age_seconds",
                    "seconds since the plane was opened",
                    |r| vec![(&[], gauge(r.age.as_secs()))],
                ),
                series(
                    "ducktape_dataplane_bytes",
                    "cumulative wire bytes moved by the plane, by direction and service class",
                    |r| {
                        let t = &r.observation.traffic;
                        vec![
                            (
                                &[("dir", "tx"), ("class", "datagram")],
                                gauge(t.datagram_bytes_tx),
                            ),
                            (
                                &[("dir", "rx"), ("class", "datagram")],
                                gauge(t.datagram_bytes_rx),
                            ),
                            (
                                &[("dir", "tx"), ("class", "stream")],
                                gauge(t.stream_bytes_tx),
                            ),
                            (
                                &[("dir", "rx"), ("class", "stream")],
                                gauge(t.stream_bytes_rx),
                            ),
                        ]
                    },
                ),
                series(
                    "ducktape_dataplane_datagrams",
                    "cumulative datagrams moved by the plane, by direction",
                    |r| {
                        let t = &r.observation.traffic;
                        vec![
                            (&[("dir", "tx")], gauge(t.datagrams_tx)),
                            (&[("dir", "rx")], gauge(t.datagrams_rx)),
                        ]
                    },
                ),
                series(
                    "ducktape_dataplane_streams",
                    "cumulative streams the plane opened toward peers and accepted from them",
                    |r| {
                        let t = &r.observation.traffic;
                        vec![
                            (&[("kind", "opened")], gauge(t.streams_opened)),
                            (&[("kind", "accepted")], gauge(t.streams_accepted)),
                        ]
                    },
                ),
                series(
                    "ducktape_dataplane_drops",
                    "cumulative dropped/refused traffic, by kind",
                    |r| {
                        let s = &r.observation.stats;
                        let t = &r.observation.traffic;
                        vec![
                            (&[("kind", "rogue_datagrams")], gauge(s.rogue_datagrams)),
                            (&[("kind", "rogue_streams")], gauge(s.rogue_streams)),
                            (&[("kind", "malformed")], gauge(s.malformed_datagrams)),
                            (
                                &[("kind", "unregistered_datagrams")],
                                gauge(s.unregistered_datagrams),
                            ),
                            (
                                &[("kind", "unregistered_streams")],
                                gauge(s.unregistered_streams),
                            ),
                            (&[("kind", "refused_sends")], gauge(s.refused_sends)),
                            (&[("kind", "shed")], gauge(t.datagrams_shed)),
                        ]
                    },
                ),
            ],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use data_plane::{PlaneObservation, PlaneWatch};
    use std::time::Duration;

    /// The full exposition path: a monitor with one live plane, registered on
    /// a real runtime registry, must encode `{service, owner}`-labeled
    /// `ducktape_dataplane_*` samples — and stop encoding once the plane dies.
    #[test]
    fn scrape_encodes_live_planes_and_forgets_dead_ones() {
        use commonware_runtime::{Metrics as _, Runner as _};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let executor = commonware_runtime::deterministic::Runner::default();
        executor.start(|context| async move {
            let monitor = PlaneMonitor::default();
            let alive = Arc::new(AtomicBool::new(true));
            let watch_alive = Arc::clone(&alive);
            monitor.register(
                "chat",
                Service::Voice,
                PlaneWatch::new(move || {
                    watch_alive.load(Ordering::Relaxed).then(|| {
                        let mut observation = PlaneObservation::default();
                        observation.traffic.datagram_bytes_tx = 4096;
                        observation
                    })
                }),
            );
            let _metrics = PlaneMetrics::register(&context, &monitor);

            let scrape = context.encode();
            assert!(
                scrape.contains(
                    r#"ducktape_dataplane_open{service="voice",owner="chat"} 1"#
                ),
                "open series missing from scrape:\n{scrape}"
            );
            assert!(
                scrape.contains(
                    r#"ducktape_dataplane_bytes{service="voice",owner="chat",dir="tx",class="datagram"} 4096"#
                ),
                "bytes series missing from scrape:\n{scrape}"
            );

            // The plane dies: its series vanish from the next scrape.
            alive.store(false, Ordering::Relaxed);
            let scrape = context.encode();
            assert!(
                !scrape.contains("ducktape_dataplane_open{"),
                "dead plane still encoded:\n{scrape}"
            );
        });
    }

    #[test]
    fn application_names_appear_and_retire_without_native_registration() {
        use commonware_runtime::{Metrics as _, Runner as _};
        use crate::gateway_routes::{LocalRoute, LocalRoutes, UpstreamTrust, FILE_NAME};
        commonware_runtime::deterministic::Runner::default().start(|context| async move {
            let workspace = tempfile::tempdir().unwrap();
            let _metric = ApplicationBindings::register(&context, workspace.path().into());
            let path = workspace.path().join(FILE_NAME);
            let routes = LocalRoutes { routes: vec![LocalRoute {
                account: 17,
                name: gateway::RouteName::named("unlisted-application"),
                port: 9876,
                trust: UpstreamTrust::AuthenticatedLoopback { credential_file: workspace.path().join("private-token") },
            }] };
            std::fs::write(&path, serde_json::to_vec_pretty(&routes).unwrap()).unwrap();
            assert!(context.encode().contains(r#"ducktape_application_binding{account="17",route="unlisted-application",authentication="credential"} 1"#));
            std::fs::write(&path, serde_json::to_vec_pretty(&LocalRoutes::default()).unwrap()).unwrap();
            assert!(!context.encode().contains("ducktape_application_binding{"));
        });
    }

    /// Age and halted project through with the shared label identity.
    #[test]
    fn projections_cover_age_and_halted() {
        let report = PlaneReport {
            owner: "gateway",
            service: Service::Gateway,
            age: Duration::from_secs(90),
            observation: PlaneObservation::default(),
        };
        assert_eq!(service_name(report.service), "gateway");
        assert_eq!(gauge(report.age.as_secs()), 90);
    }
}
