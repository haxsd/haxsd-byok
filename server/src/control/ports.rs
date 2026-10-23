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
    anthropic::ClaudeSettings,
    codex::CodexSettings,
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

/// Refuses a Codex settings save whose port could not be listened on.
///
/// Same reasoning as the Devin check: the Codex gateway binds its port without a
/// fallback, and a port taken by our own service/proxy/Devin listener (or by
/// another program) used to surface as "saved, not listening" with a raw io
/// error nobody could act on. `own_listener` is the port the running gateway
/// already holds — that one is ours, not an occupier.
pub(super) async fn ensure_codex_port_usable(
    store: &Store,
    settings: &CodexSettings,
    own_listener: Option<u16>,
) -> Result<()> {
    let port = settings.port;
    if port == 0 {
        return Err(Error::Config("Codex port must be non-zero".into()));
    }
    for (owned, description) in app_ports_for_codex(store).await? {
        if owned == port {
            return Err(Error::Config(format!(
                "Codex gateway port {port} is already used by haxsd byok as the {description}; choose a different port for the gateway"
            )));
        }
    }
    if own_listener == Some(port) {
        return Ok(());
    }
    if !settings.enabled {
        return Ok(());
    }
    if let Some(error) = bind_conflict(port).await {
        return Err(Error::Config(match error.kind() {
            std::io::ErrorKind::AddrInUse => format!(
                "Codex gateway port {port} is already in use by another program on this machine; \
                 close it or choose a different port for the gateway"
            ),
            _ => format!(
                "Codex gateway port {port} cannot be listened on by this app: {error}; \
                 choose a different port for the gateway"
            ),
        }));
    }
    Ok(())
}

/// The Claude gateway binds one loopback port with the same no-fallback rule as
/// Codex: a port held by another program, or by this app's own listeners, would
/// otherwise surface as "saved, not listening" after the settings were written.
/// `own_listener` is the port the running gateway already holds.
pub(super) async fn ensure_claude_port_usable(
    store: &Store,
    settings: &ClaudeSettings,
    own_listener: Option<u16>,
) -> Result<()> {
    let port = settings.port;
    if port == 0 {
        return Err(Error::Config("Claude port must be non-zero".into()));
    }
    for (owned, description) in app_ports_for_claude(store).await? {
        if owned == port {
            return Err(Error::Config(format!(
                "Claude gateway port {port} is already used by haxsd byok as the {description}; choose a different port for the gateway"
            )));
        }
    }
    if own_listener == Some(port) {
        return Ok(());
    }
    if !settings.enabled {
        return Ok(());
    }
    if let Some(error) = bind_conflict(port).await {
        return Err(Error::Config(match error.kind() {
            std::io::ErrorKind::AddrInUse => format!(
                "Claude gateway port {port} is already in use by another program on this machine; \
                 close it or choose a different port for the gateway"
            ),
            _ => format!(
                "Claude gateway port {port} cannot be listened on by this app: {error}; \
                 choose a different port for the gateway"
            ),
        }));
    }
    Ok(())
}

/// Listeners owned by the other integrations. A settings read that fails only
/// costs this check one comparison; its own page reports the real problem.
async fn app_ports_for_claude(store: &Store) -> Result<Vec<(u16, &'static str)>> {
    let ports = store.port_settings().await?;
    let mut own = Vec::new();
    for (port, description) in [
        (ports.service_port, "management service port"),
        (ports.proxy_port, "Cursor proxy port"),
    ] {
        if port != 0 {
            own.push((port, description));
        }
    }
    match store.devin_settings().await {
        Ok(devin) => {
            for (port, description) in [
                (devin.api_port, "Devin API port"),
                (devin.inference_port, "Devin inference port"),
                (devin.local_api_port, "Devin local API port"),
            ] {
                if port != 0 {
                    own.push((port, description));
                }
            }
        }
        Err(error) => tracing::warn!(
            %error,
            "Devin settings could not be read; the Claude port check skips its ports"
        ),
    }
    match store.codex_settings().await {
        Ok(codex) if codex.port != 0 => {
            own.push((codex.port, "Codex gateway port"));
        }
        Ok(_) => {}
        Err(error) => tracing::warn!(
            %error,
            "Codex settings could not be read; the Claude port check skips its port"
        ),
    }
    Ok(own)
}
/// The other listeners this app owns; the Codex port itself is deliberately not
/// in the list (saving the same port again must stay a no-op, not a conflict).
async fn app_ports_for_codex(store: &Store) -> Result<Vec<(u16, &'static str)>> {
    let ports = store.port_settings().await?;
    let mut own = Vec::new();
    for (port, description) in [
        (ports.service_port, "management service port"),
        (ports.proxy_port, "Cursor proxy port"),
    ] {
        if port != 0 {
            own.push((port, description));
        }
    }
    // 一条读不动的 Devin 设置不该拦住 Codex 页：Devin 页自己会报那一行的问题。
    match store.devin_settings().await {
        Ok(devin) => {
            for (port, description) in [
                (devin.api_port, "Devin API port"),
                (devin.inference_port, "Devin inference port"),
                (devin.local_api_port, "Devin local API port"),
            ] {
                if port != 0 {
                    own.push((port, description));
                }
            }
        }
        Err(error) => tracing::warn!(
            %error,
            "Devin settings could not be read; the Codex port check skips its ports"
        ),
    }
    match store.claude_settings().await {
        Ok(claude) if claude.port != 0 => {
            own.push((claude.port, "Claude gateway port"));
        }
        Ok(_) => {}
        Err(error) => tracing::warn!(
            %error,
            "Claude settings could not be read; the Codex port check skips its port"
        ),
    }
    Ok(own)
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
    match store.claude_settings().await {
        Ok(claude) if claude.port != 0 => own.push(AppPort {
            port: claude.port,
            description: "Claude gateway port",
        }),
        Ok(_) => {}
        Err(error) => tracing::warn!(
            %error,
            "Claude settings could not be read; the Devin port check skips its port"
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
    use crate::{anthropic::ClaudeSettings, codex::CodexSettings, store::Store};

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

    /// Codex 的端口准入：被本应用自己的监听器占着时要指名道姓，而不是让保存成功、
    /// 监听失败、重启也不管用。
    #[tokio::test]
    async fn a_codex_port_held_by_our_own_listeners_is_refused_by_name() {
        let (_directory, store) = temp_store().await;
        store.set_service_port(42_001).await.unwrap();
        store.set_proxy_port(42_002).await.unwrap();
        store
            .set_devin_settings(DevinSettings {
                api_port: 42_003,
                inference_port: 42_004,
                local_api_port: 42_005,
                ..DevinSettings::default()
            })
            .await
            .unwrap();

        for (port, expected) in [
            (42_001, "management service port"),
            (42_002, "Cursor proxy port"),
            (42_003, "Devin API port"),
            (42_004, "Devin inference port"),
            (42_005, "Devin local API port"),
        ] {
            let error = ensure_codex_port_usable(
                &store,
                &CodexSettings {
                    port,
                    ..CodexSettings::default()
                },
                None,
            )
            .await
            .unwrap_err()
            .to_string();
            assert!(
                error.contains(expected) && error.contains(&port.to_string()),
                "port {port} should name {expected}: {error}"
            );
        }
    }

    /// 同一个端口再存一次不算冲突（那是我们自己的监听器），空闲端口可以直接保存。
    #[tokio::test]
    async fn the_codex_gateways_own_port_and_a_free_port_are_accepted() {
        let (_directory, store) = temp_store().await;
        ensure_codex_port_usable(
            &store,
            &CodexSettings {
                port: 42_010,
                ..CodexSettings::default()
            },
            Some(42_010),
        )
        .await
        .unwrap();

        let port = free_port();
        ensure_codex_port_usable(
            &store,
            &CodexSettings {
                port,
                ..CodexSettings::default()
            },
            None,
        )
        .await
        .unwrap();
        tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, port))
            .await
            .unwrap();
    }

    /// 别的程序占着、或者开关没打开（不监听）时的两种结果。
    #[tokio::test]
    async fn a_codex_port_held_by_another_program_is_refused_unless_the_gateway_is_disabled() {
        let (_directory, store) = temp_store().await;
        let occupied = OccupiedPort::new();
        let error = ensure_codex_port_usable(
            &store,
            &CodexSettings {
                enabled: true,
                port: occupied.port(),
                ..CodexSettings::default()
            },
            None,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("another program") && error.contains(&occupied.port().to_string()),
            "{error}"
        );

        ensure_codex_port_usable(
            &store,
            &CodexSettings {
                enabled: false,
                port: occupied.port(),
                ..CodexSettings::default()
            },
            None,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn a_zero_codex_port_is_refused() {
        let (_directory, store) = temp_store().await;
        let error = ensure_codex_port_usable(
            &store,
            &CodexSettings {
                port: 0,
                ..CodexSettings::default()
            },
            None,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("Codex port must be non-zero"), "{error}");
    }

    /// Claude 页面的端口准入与 Codex 同规则：本应用自己的监听器按名字拒绝，
    /// 网关自己已持有的端口与空闲端口可以再存。
    #[tokio::test]
    async fn claude_ports_are_checked_against_our_own_listeners_and_the_gateway_itself() {
        let (_directory, store) = temp_store().await;
        store.set_service_port(43_001).await.unwrap();
        let error = ensure_claude_port_usable(
            &store,
            &ClaudeSettings {
                enabled: true,
                port: 43_001,
                ..ClaudeSettings::default()
            },
            None,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("management service port") && error.contains("43001"),
            "{error}"
        );

        ensure_claude_port_usable(
            &store,
            &ClaudeSettings {
                enabled: true,
                port: 43_002,
                ..ClaudeSettings::default()
            },
            Some(43_002),
        )
        .await
        .unwrap();

        let port = free_port();
        ensure_claude_port_usable(
            &store,
            &ClaudeSettings {
                enabled: true,
                port,
                ..ClaudeSettings::default()
            },
            None,
        )
        .await
        .unwrap();

        let error = ensure_claude_port_usable(
            &store,
            &ClaudeSettings {
                enabled: true,
                port: 0,
                ..ClaudeSettings::default()
            },
            None,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("Claude port must be non-zero"), "{error}");
    }
}
