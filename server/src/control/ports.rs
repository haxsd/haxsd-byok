//! Save-time admission checks for the ports this app is about to listen on.
//!
//! The Devin gateway binds its three ports once, at startup, with no retry and no
//! fallback: a port that is already taken by another program — or by this app's
//! own service or proxy listener — makes `serve` fail with a log line while the
//! settings page keeps reporting "saved, not listening, restart". Restarting does
//! not help, and nothing told the user why. These checks refuse such a save
//! upfront and name whoever holds the port.
//!
//! Only the save path is guarded on purpose: changing the port at runtime to dodge
//! an occupier is a separate product decision.

use tokio::net::TcpListener;

use crate::{
    devin::{gateway::DevinBoundPorts, DevinSettings},
    store::Store,
    Error, Result,
};

/// One listener this app owns, named the way the settings page names it.
struct AppPort {
    port: u16,
    description: &'static str,
}

/// Refuses a Devin settings save whose ports could not be listened on.
///
/// `bound` is the running gateway's ports: those are held by this process, so
/// re-saving them (or swapping them between roles) is not a conflict. `settings`
/// must already have passed `DevinSettings::validate`.
pub(super) async fn ensure_devin_ports_usable(
    store: &Store,
    settings: &DevinSettings,
    bound: Option<DevinBoundPorts>,
) -> Result<()> {
    let own = app_ports(store).await?;
    for (role, port) in [
        ("api", settings.api_port),
        ("inference", settings.inference_port),
        ("local API", settings.local_api_port),
    ] {
        // Clashing with our own listeners is a contradiction in the stored
        // configuration, so it is refused whether or not the gateway is enabled.
        if let Some(owner) = own.iter().find(|owner| owner.port == port) {
            return Err(Error::Config(format!(
                "Devin {role} port {port} is already used by haxsd byok as the {}; choose a different port for the gateway",
                owner.description
            )));
        }
        // A port this process already holds is ours, not an occupier: the running
        // gateway keeps its listeners until the process restarts.
        if bound.is_some_and(|bound| bound.contains(port)) {
            continue;
        }
        // A disabled gateway binds nothing, so neither does it conflict with
        // anything yet. Enabling it later goes through this same check.
        if !settings.enabled {
            continue;
        }
        if let Some(error) = bind_conflict(port).await {
            return Err(Error::Config(match error.kind() {
                std::io::ErrorKind::AddrInUse => format!(
                    "Devin {role} port {port} is already in use by another program on this machine; \
                     close it or choose a different port for the gateway"
                ),
                _ => format!(
                    "Devin {role} port {port} cannot be listened on by this app: {error}; \
                     choose a different port for the gateway"
                ),
            }));
        }
    }
    Ok(())
}

/// The app's own listeners, read from the settings that are in effect.
///
/// `service_port` and `proxy_port` are the ports actually bound: startup writes
/// the real one back when the configured value was 0 or already taken. 0 means
/// "pick one at startup", so it is not a port anyone can collide with.
async fn app_ports(store: &Store) -> Result<Vec<AppPort>> {
    let ports = store.port_settings().await?;
    let mut own = Vec::new();
    for (port, description) in [
        (ports.service_port, "management service port"),
        (ports.proxy_port, "Cursor proxy port"),
    ] {
        if port != 0 {
            own.push(AppPort { port, description });
        }
    }
    // 一条读不动的 Codex 设置不该让 Devin 页存不下东西：Codex 页自己会报那一行
    // 的问题，这里少比一个端口，只留下一条警告。
    match store.codex_settings().await {
        Ok(codex) if codex.port != 0 => own.push(AppPort {
            port: codex.port,
            description: "Codex gateway port",
        }),
        Ok(_) => {}
        Err(error) => tracing::warn!(
            %error,
            "Codex settings could not be read; the Devin port check skips its port"
        ),
    }
    Ok(own)
}

/// Whether something on this machine already holds the loopback port. Binding is
/// the probe: the gateway listens on `127.0.0.1`, so a program that only holds
/// another interface does not block it. The probe listener is dropped before this
/// function returns, so the check leaves no socket of its own behind.
async fn bind_conflict(port: u16) -> Option<std::io::Error> {
    match TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await {
        Ok(listener) => {
            drop(listener);
            None
        }
        Err(error) => Some(error),
    }
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, TcpListener as StdTcpListener};

    use super::*;
    use crate::{codex::CodexSettings, store::Store};

    /// A temp store with an isolated database file.
    async fn temp_store() -> (tempfile::TempDir, Store) {
        let directory = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}", directory.path().join("test.db").display());
        let store = Store::connect(&url).await.unwrap();
        (directory, store)
    }

    /// An enabled gateway whose ports are free unless a test says otherwise.
    fn settings() -> DevinSettings {
        DevinSettings {
            enabled: true,
            ..DevinSettings::default()
        }
    }

    /// A loopback port held for as long as the value lives, standing in for
    /// another program on this machine.
    struct OccupiedPort {
        _listener: StdTcpListener,
        port: u16,
    }

    impl OccupiedPort {
        fn new() -> Self {
            let listener = StdTcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            let port = listener.local_addr().unwrap().port();
            Self {
                _listener: listener,
                port,
            }
        }

        fn port(&self) -> u16 {
            self.port
        }
    }

    fn free_port() -> u16 {
        let listener = StdTcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        port
    }

    #[tokio::test]
    async fn a_port_the_app_already_uses_is_refused_by_name() {
        let (_directory, store) = temp_store().await;
        store.set_service_port(41_001).await.unwrap();
        store.set_proxy_port(41_002).await.unwrap();
        store
            .set_codex_settings(CodexSettings {
                port: 41_003,
                ..CodexSettings::default()
            })
            .await
            .unwrap();

        for (port, expected) in [
            (41_001, "management service port"),
            (41_002, "Cursor proxy port"),
            (41_003, "Codex gateway port"),
        ] {
            let error = ensure_devin_ports_usable(
                &store,
                &DevinSettings {
                    api_port: port,
                    ..settings()
                },
                None,
            )
            .await
            .unwrap_err()
            .to_string();
            assert!(
                error.contains(expected) && error.contains(&port.to_string()),
                "port {port} must be refused as the {expected}: {error}"
            );
        }
    }

    #[tokio::test]
    async fn a_port_another_program_holds_is_refused() {
        let (_directory, store) = temp_store().await;
        let occupied = OccupiedPort::new();

        let error = ensure_devin_ports_usable(
            &store,
            &DevinSettings {
                inference_port: occupied.port(),
                ..settings()
            },
            None,
        )
        .await
        .unwrap_err()
        .to_string();

        assert!(
            error.contains("another program") && error.contains(&occupied.port().to_string()),
            "a foreign listener must be named as the occupier: {error}"
        );
        assert!(
            error.contains("inference"),
            "the refused role must be named: {error}"
        );
    }

    #[tokio::test]
    async fn the_running_gateways_own_ports_are_not_reported_as_occupied() {
        let (_directory, store) = temp_store().await;
        // The gateway is serving, so all three ports answer to this process.
        let api = OccupiedPort::new();
        let inference = OccupiedPort::new();
        let local_api = OccupiedPort::new();
        let bound = Some(DevinBoundPorts {
            api: api.port(),
            inference: inference.port(),
            local_api: local_api.port(),
        });
        let settings = DevinSettings {
            api_port: api.port(),
            inference_port: inference.port(),
            local_api_port: local_api.port(),
            ..settings()
        };

        // Re-saving the ports the gateway holds, and swapping two of them, are
        // both legitimate: the listeners move on the next restart.
        ensure_devin_ports_usable(&store, &settings, bound)
            .await
            .unwrap();
        ensure_devin_ports_usable(
            &store,
            &DevinSettings {
                api_port: local_api.port(),
                local_api_port: api.port(),
                ..settings
            },
            bound,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn a_disabled_gateway_does_not_reserve_ports_yet() {
        let (_directory, store) = temp_store().await;
        let occupied = OccupiedPort::new();

        ensure_devin_ports_usable(
            &store,
            &DevinSettings {
                enabled: false,
                api_port: occupied.port(),
                ..settings()
            },
            None,
        )
        .await
        .unwrap();

        // Enabling it goes through the same check, which is where the failure
        // finally has to surface.
        let error = ensure_devin_ports_usable(
            &store,
            &DevinSettings {
                api_port: occupied.port(),
                ..settings()
            },
            None,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("another program"), "{error}");
    }

    #[tokio::test]
    async fn a_free_port_is_accepted_and_the_probe_leaves_nothing_behind() {
        let (_directory, store) = temp_store().await;
        let port = free_port();

        ensure_devin_ports_usable(
            &store,
            &DevinSettings {
                api_port: port,
                ..settings()
            },
            None,
        )
        .await
        .unwrap();

        // The probe listener must be gone: a second bind has to succeed, which is
        // also what the gateway does at startup.
        tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, port))
            .await
            .unwrap();
    }
}
