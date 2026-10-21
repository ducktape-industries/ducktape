use consensus::CadenceError;

use super::*;
use std::num::NonZeroU64;

fn descriptor() -> Descriptor {
    Descriptor {
        network: "config".into(),
        time: 0,
        block_time_ms: 250,
    }
}

fn socket(port: u16) -> SocketAddr {
    SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port))
}

fn count(count: usize) -> NonZeroUsize {
    NonZeroUsize::new(count).unwrap()
}

fn required() -> Settings {
    Settings {
        listen: Some(socket(9000)),
        halo: Some(socket(9200)),
        ..Settings::default()
    }
}

const EVERY_KEY: &str = "listen = \"127.0.0.1:9000\"\n\
                         http = \"127.0.0.1:8844\"\n\
                         tls = \"127.0.0.1:9443\"\n\
                         halo = \"127.0.0.1:9200\"\n\
                         live = \"127.0.0.1:9300\"\n\
                         log = \"debug\"\n\
                         block_cpus = \"0-5\"\n\
                         [load]\n\
                         halo_egress = 1000000\n\
                         halo_inbound_streams = 32\n\
                         halo_datagrams = 128\n\
                         blob_serves_in_flight = 3\n\
                         blob_backlog = 40\n\
                         blob_gatherers = 5\n\
                         live_session_inbound = 64\n\
                         live_session_outbound = 256\n\
                         live_group_queue = 8\n\
                         live_peer_queue = 32\n\
                         live_lane_backlog = 4\n\
                         live_lane_per_sender = 16\n\
                         live_linger_ms = 700\n\
                         egress_lane_backlog = 2\n\
                         forward_lane_backlog = 3\n\
                         query_fuel = 5000000\n\
                         [consensus]\n\
                         leader_timeout_ms = 4000\n\
                         certification_timeout_ms = 6000\n\
                         timeout_retry_ms = 2000\n";

fn every_key() -> Settings {
    Settings {
        listen: Some(socket(9000)),
        http: Some(socket(8844)),
        tls: Some(socket(9443)),
        halo: Some(socket(9200)),
        live: Some(socket(9300)),
        log: Some("debug".into()),
        block_cpus: Some(CpuList::parse("0-5").unwrap()),
        load: LoadSettings {
            halo_egress: Some(1_000_000),
            halo_inbound_streams: Some(count(32)),
            halo_datagrams: Some(count(128)),
            blob_serves_in_flight: Some(count(3)),
            blob_backlog: Some(count(40)),
            blob_gatherers: Some(count(5)),
            live_session_inbound: Some(count(64)),
            live_session_outbound: Some(count(256)),
            live_group_queue: Some(count(8)),
            live_peer_queue: Some(count(32)),
            live_lane_backlog: Some(count(4)),
            live_lane_per_sender: Some(count(16)),
            live_linger_ms: NonZeroU64::new(700),
            egress_lane_backlog: Some(count(2)),
            forward_lane_backlog: Some(count(3)),
            query_fuel: NonZeroU64::new(5_000_000),
        },
        consensus: ConsensusSettings {
            leader_timeout_ms: Some(4000),
            certification_timeout_ms: Some(6000),
            timeout_retry_ms: Some(2000),
        },
        machine: None,
    }
}

#[test]
fn a_file_with_every_key_parses() {
    assert_eq!(Settings::parse(EVERY_KEY).unwrap(), every_key());
}

#[test]
fn an_empty_file_is_every_default() {
    assert_eq!(Settings::parse("").unwrap(), Settings::default());
}

#[test]
fn an_unknown_key_is_refused_at_its_line() {
    let refusal = Settings::parse("listen = \"127.0.0.1:9000\"\nlisten_on = 1\n").unwrap_err();
    assert!(
        refusal.starts_with("line 2: unknown field `listen_on`"),
        "{refusal}"
    );
}

#[test]
fn an_unknown_key_in_a_table_is_refused() {
    let consensus = Settings::parse("[consensus]\nskip_timeout_ms = 9\n").unwrap_err();
    assert!(
        consensus.starts_with("line 2: unknown field `skip_timeout_ms`"),
        "{consensus}"
    );
    let load = Settings::parse("[load]\nwrites_in_flight = 9\n").unwrap_err();
    assert!(
        load.starts_with("line 2: unknown field `writes_in_flight`"),
        "{load}"
    );
}

#[test]
fn a_count_the_file_names_must_be_nonzero_and_fit_a_tokio_semaphore() {
    let past_tokio = tokio::sync::Semaphore::MAX_PERMITS + 1;
    for key in [
        "halo_inbound_streams",
        "halo_datagrams",
        "blob_serves_in_flight",
        "blob_backlog",
        "egress_lane_backlog",
        "forward_lane_backlog",
    ] {
        assert!(
            Settings::parse(&format!("[load]\n{key} = 0\n")).is_err(),
            "{key}"
        );
        assert!(
            Settings::parse(&format!("[load]\n{key} = {past_tokio}\n")).is_err(),
            "{key}"
        );
    }
    assert!(Settings::parse("[load]\nblob_gatherers = 0\n").is_err());
}

#[test]
fn permits_refuses_a_count_past_a_tokio_semaphore() {
    let most = tokio::sync::Semaphore::MAX_PERMITS;
    assert_eq!(permits(count(most)).unwrap(), count(most));
    assert!(matches!(
        permits(count(most + 1)),
        Err(Error::PastPermits { count: past, most: held }) if past == count(most + 1) && held == most
    ));
}

#[test]
fn file_only_settings_reach_the_config() {
    let config = Settings::default()
        .over(every_key())
        .resolve(&descriptor())
        .unwrap();
    assert_eq!(
        config.listen,
        Listen {
            p2p: socket(9000),
            http: socket(8844),
            tls: Some(socket(9443)),
            halo: socket(9200),
            live: Some(socket(9300)),
        }
    );
    assert_eq!(config.log, "debug");
    assert_eq!(
        config.halo,
        HaloConfig {
            egress_bytes_per_sec: Some(1_000_000),
            inbound_streams: count(32),
            datagrams: count(128),
        }
    );
    assert_eq!(
        config.load,
        LoadConfig {
            blob_backlog: count(40),
            blob_serves_in_flight: count(3),
            blob_gatherers: count(5),
            live_session_inbound: count(64),
            live_session_outbound: count(256),
            live_group_queue: count(8),
            live_peer_queue: count(32),
            live_lane_backlog: count(4),
            live_lane_per_sender: count(16),
            live_linger: Duration::from_millis(700),
            egress_lane_backlog: count(2),
            forward_lane_backlog: count(3),
            rpc_cpus: None,
            rpc_blocking_threads: None,
            query_fuel: NonZeroU64::new(5_000_000),
        }
    );
    assert_eq!(config.block_cpus.unwrap().cpus(), &[0, 1, 2, 3, 4, 5]);
    assert_eq!(config.cadence.leader_timeout(), Duration::from_millis(4000));
    assert_eq!(
        config.cadence.certification_timeout(),
        Duration::from_millis(6000)
    );
    assert_eq!(config.cadence.timeout_retry(), Duration::from_millis(2000));
}

#[test]
fn block_cpus_the_file_cannot_read_is_refused_at_its_line() {
    let refusal = Settings::parse("log = \"info\"\nblock_cpus = \"5-0\"\n").unwrap_err();
    assert!(refusal.starts_with("line 2"), "{refusal}");
    assert!(Settings::parse("block_cpus = 5\n").is_err());
}

#[test]
fn a_flag_beats_the_file_and_the_file_beats_the_default() {
    let file = Settings {
        listen: Some(socket(9000)),
        http: Some(socket(8844)),
        live: Some(socket(9300)),
        log: Some("debug".into()),
        load: LoadSettings {
            halo_datagrams: Some(count(128)),
            blob_backlog: Some(count(16)),
            blob_serves_in_flight: Some(count(2)),
            ..LoadSettings::default()
        },
        consensus: ConsensusSettings {
            leader_timeout_ms: Some(4000),
            certification_timeout_ms: Some(6000),
            timeout_retry_ms: None,
        },
        ..Settings::default()
    };
    let flags = Settings {
        listen: Some(socket(9100)),
        halo: Some(socket(9200)),
        load: LoadSettings {
            halo_datagrams: Some(count(64)),
            blob_backlog: Some(count(8)),
            ..LoadSettings::default()
        },
        consensus: ConsensusSettings {
            leader_timeout_ms: Some(3000),
            ..ConsensusSettings::default()
        },
        ..Settings::default()
    };
    let config = flags.over(file).resolve(&descriptor()).unwrap();
    assert_eq!(
        config.listen,
        Listen {
            p2p: socket(9100),
            http: socket(8844),
            tls: None,
            halo: socket(9200),
            live: Some(socket(9300)),
        }
    );
    assert_eq!(config.log, "debug");
    assert_eq!(
        config.halo,
        HaloConfig {
            datagrams: count(64),
            ..HaloConfig::default()
        }
    );
    assert_eq!(
        config.load,
        LoadConfig {
            blob_backlog: count(8),
            blob_serves_in_flight: count(2),
            ..LoadConfig::default()
        }
    );
    assert_eq!(config.cadence.leader_timeout(), Duration::from_millis(3000));
    assert_eq!(
        config.cadence.certification_timeout(),
        Duration::from_millis(6000)
    );
    assert_eq!(config.cadence.timeout_retry(), Duration::from_millis(500));
}

#[test]
fn no_flag_and_no_file_is_the_default() {
    let config = required()
        .over(Settings::default())
        .resolve(&descriptor())
        .unwrap();
    assert_eq!(config.listen.http, DEFAULT_HTTP);
    assert_eq!(config.listen.tls, None);
    assert_eq!(config.log, DEFAULT_LOG);
    assert_eq!(config.halo, HaloConfig::default());
    assert_eq!(config.load, LoadConfig::default());
    assert_eq!(
        config.cadence,
        descriptor().cadence(Timeouts::default()).unwrap()
    );
}

#[test]
fn a_required_socket_named_nowhere_is_refused_naming_the_flag_and_the_key() {
    let no_listen = Settings {
        listen: None,
        ..required()
    };
    let refusal = no_listen.resolve(&descriptor()).unwrap_err();
    assert!(matches!(refusal, Error::NoListen), "{refusal}");
    let sentence = refusal.to_string();
    assert!(sentence.contains("--listen"), "{sentence}");
    assert!(sentence.contains("listen in node.toml"), "{sentence}");

    let no_halo = Settings {
        halo: None,
        ..required()
    };
    let refusal = no_halo.resolve(&descriptor()).unwrap_err();
    assert!(matches!(refusal, Error::NoHalo), "{refusal}");
    let sentence = refusal.to_string();
    assert!(sentence.contains("--halo"), "{sentence}");
    assert!(sentence.contains("halo in node.toml"), "{sentence}");
}

#[test]
fn a_required_socket_from_the_file_alone_is_enough() {
    let config = Settings::default()
        .over(required())
        .resolve(&descriptor())
        .unwrap();
    assert_eq!(config.listen.p2p, socket(9000));
    assert_eq!(config.listen.halo, socket(9200));
}

#[test]
fn invalid_timeouts_are_refused() {
    let file = Settings {
        consensus: ConsensusSettings {
            leader_timeout_ms: Some(6000),
            certification_timeout_ms: Some(6000),
            timeout_retry_ms: None,
        },
        ..required()
    };
    let refusal = file.resolve(&descriptor()).unwrap_err();
    assert!(
        matches!(
            refusal,
            Error::Cadence(CadenceError::CertificationNotAboveLeader {
                leader: 6000,
                certification: 6000
            })
        ),
        "{refusal}"
    );
}

#[test]
fn the_link_follows_the_block_time_and_not_the_leader_timeout() {
    let flags = Settings {
        consensus: ConsensusSettings {
            leader_timeout_ms: Some(8000),
            certification_timeout_ms: Some(12_000),
            timeout_retry_ms: None,
        },
        ..required()
    };
    let config = flags.resolve(&descriptor()).unwrap();
    assert_eq!(config.link_timeout(), Duration::from_secs(5));
}

#[test]
fn a_missing_file_is_every_default_and_a_bad_one_names_its_path() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Workspace::at(dir.path());
    assert_eq!(workspace.settings().unwrap(), Settings::default());
    std::fs::write(workspace.settings_file(), "lisen = \"127.0.0.1:9000\"\n").unwrap();
    let refusal = workspace.settings().unwrap_err();
    let Error::Settings { path, reason } = refusal else {
        panic!("{refusal}");
    };
    assert_eq!(path, workspace.settings_file());
    assert!(
        reason.starts_with("line 1: unknown field `lisen`"),
        "{reason}"
    );
}

#[test]
fn load_reads_the_file_under_the_flags() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Workspace::at(dir.path());
    workspace.write_descriptor(&descriptor()).unwrap();
    std::fs::write(workspace.settings_file(), EVERY_KEY).unwrap();
    let flags = Settings {
        http: Some(socket(8855)),
        load: LoadSettings {
            blob_gatherers: Some(count(9)),
            ..LoadSettings::default()
        },
        ..Settings::default()
    };
    let config = NodeConfig::load(&workspace, flags).unwrap();
    assert_eq!(config.listen.p2p, socket(9000));
    assert_eq!(config.listen.http, socket(8855));
    assert_eq!(config.listen.halo, socket(9200));
    assert_eq!(config.load.blob_gatherers, count(9));
    assert_eq!(config.load.blob_backlog, count(40));
    assert_eq!(config.cadence.leader_timeout(), Duration::from_millis(4000));
}

#[test]
fn a_machine_table_parses_and_refuses_a_key_or_a_quantity_it_does_not_know() {
    let text = "[machine]\non = true\ncpus = \"6-7\"\nnice = 10\nmemory = \"32Gi\"\nwork = \"/var/lib/ducktape-work\"\n\
                [[machine.machines]]\nname = \"warm\"\nlabels = [\"warm\"]\n\
                backend = \"shell\"\nslots = 2\nserves = [17]\n\
                [[machine.machines]]\nname = \"fresh\"\nbackend = \"podman\"\nslots = 1\n";
    let machine = Settings::parse(text).unwrap().machine.unwrap();
    assert_eq!(
        machine,
        MachineSettings {
            on: true,
            cpus: Some(CpuList::parse("6-7").unwrap()),
            nice: Some(10),
            memory: Some(32 << 30),
            work: Some("/var/lib/ducktape-work".into()),
            machines: vec![
                MachineEntry {
                    name: "warm".into(),
                    labels: vec!["warm".into()],
                    backend: Backend::Shell,
                    slots: 2,
                    serves: Some(vec![17]),
                },
                MachineEntry {
                    name: "fresh".into(),
                    labels: Vec::new(),
                    backend: Backend::Podman,
                    slots: 1,
                    serves: None,
                },
            ],
        }
    );
    let unknown = Settings::parse("[machine]\nram = 1\n").unwrap_err();
    assert!(
        unknown.starts_with("line 2: unknown field `ram`"),
        "{unknown}"
    );
    assert!(Settings::parse("[machine]\nmemory = \"lots\"\n").is_err());
    assert!(
        Settings::parse("[[machine.machines]]\nname = \"a\"\nbackend = \"k8s\"\nslots = 1\n")
            .is_err()
    );
    assert_eq!(Settings::parse("").unwrap().machine, None);
}

#[test]
fn a_memory_quantity_reads_as_kubernetes_writes_it() {
    assert_eq!(memory_quantity("512Mi"), Some(512 << 20));
    assert_eq!(memory_quantity("1.5G"), Some(1_500_000_000));
    assert_eq!(memory_quantity("1000"), Some(1000));
    assert_eq!(memory_quantity("lots"), None);
    assert_eq!(memory_quantity("1E"), Some(1_000_000_000_000_000_000));
    assert_eq!(memory_quantity("8Ei"), Some(8 << 60));
    assert_eq!(memory_quantity("16Ei"), None);
    let bytes = Settings::parse("[machine]\nmemory = 1073741824\n").unwrap();
    assert_eq!(bytes.machine.unwrap().memory, Some(1 << 30));
}
