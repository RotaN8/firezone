//! Main connlib library for clients.
pub use crate::serde_routelist::{V4RouteList, V6RouteList};
pub use connlib_shared::messages::client::ResourceDescription;
pub use connlib_shared::{
    callbacks, keypair, Callbacks, Error, LoginUrl, LoginUrlError, StaticSecret,
};
pub use eventloop::Eventloop;
pub use tracing_appender::non_blocking::WorkerGuard;

use backoff::ExponentialBackoffBuilder;
use connlib_shared::get_user_agent;
use firezone_tunnel::ClientTunnel;
use phoenix_channel::PhoenixChannel;
use socket_factory::SocketFactory;
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedReceiver;

mod eventloop;
pub mod file_logger;
mod messages;
mod serde_routelist;

const PHOENIX_TOPIC: &str = "client";

use eventloop::Command;
use secrecy::Secret;
use tokio::task::JoinHandle;

/// A session is the entry-point for connlib, maintains the runtime and the tunnel.
///
/// A session is created using [Session::connect], then to stop a session we use [Session::disconnect].
pub struct Session {
    channel: tokio::sync::mpsc::UnboundedSender<Command>,
}

/// Arguments for `connect`, since Clippy said 8 args is too many
pub struct ConnectArgs<CB> {
    pub url: LoginUrl,
    pub tcp_socket_factory: Arc<dyn SocketFactory<tokio::net::TcpSocket>>,
    pub udp_socket_factory: Arc<dyn SocketFactory<tokio::net::UdpSocket>>,
    pub private_key: StaticSecret,
    pub os_version_override: Option<String>,
    pub app_version: String,
    pub callbacks: CB,
    pub max_partition_time: Option<Duration>,
}

impl Session {
    /// Creates a new [`Session`].
    ///
    /// This connects to the portal a specified using [`LoginUrl`] and creates a wireguard tunnel using the provided private key.
    pub fn connect<CB: Callbacks + 'static>(
        args: ConnectArgs<CB>,
        handle: tokio::runtime::Handle,
    ) -> Self {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();

        let callbacks = args.callbacks.clone();
        let connect_handle = handle.spawn(connect(args, rx));
        handle.spawn(connect_supervisor(connect_handle, callbacks));

        Self { channel: tx }
    }

    /// Attempts to reconnect a [`Session`].
    ///
    /// Reconnecting a session will:
    ///
    /// - Close and re-open a connection to the portal.
    /// - Refresh all allocations
    /// - Rebind local UDP sockets
    ///
    /// # Implementation note
    ///
    /// The reason we rebind the UDP sockets are:
    ///
    /// 1. On MacOS, as socket bound to the unspecified IP cannot send to interfaces attached after the socket has been created.
    /// 2. Switching between networks changes the 3-tuple of the client.
    ///    The TURN protocol identifies a client's allocation based on the 3-tuple.
    ///    Consequently, an allocation is invalid after switching networks and we clear the state.
    ///    Changing the IP would be enough for that.
    ///    However, if the user would now change _back_ to the previous network,
    ///    the TURN server would recognise the old allocation but the client already lost all its state associated with it.
    ///    To avoid race-conditions like this, we rebind the sockets to a new port.
    pub fn reconnect(&self) {
        let _ = self.channel.send(Command::Reconnect);
    }

    /// Sets a new set of upstream DNS servers for this [`Session`].
    ///
    /// Changing the DNS servers clears all cached DNS requests which may be disruptive to the UX.
    /// Clients should only call this when relevant.
    ///
    /// The implementation is idempotent; calling it with the same set of servers is safe.
    pub fn set_dns(&self, new_dns: Vec<IpAddr>) {
        let _ = self.channel.send(Command::SetDns(new_dns));
    }

    /// Disconnect a [`Session`].
    ///
    /// This consumes [`Session`] which cleans up all state associated with it.
    pub fn disconnect(self) {
        let _ = self.channel.send(Command::Stop);
    }
}

/// Connects to the portal and starts a tunnel.
///
/// When this function exits, the tunnel failed unrecoverably and you need to call it again.
async fn connect<CB>(args: ConnectArgs<CB>, rx: UnboundedReceiver<Command>) -> Result<(), Error>
where
    CB: Callbacks + 'static,
{
    let ConnectArgs {
        url,
        private_key,
        os_version_override,
        app_version,
        callbacks,
        udp_socket_factory,
        tcp_socket_factory,
        max_partition_time,
    } = args;

    // Note on the first connect these addresses won't be used yet, though coincidentally phoenix_channel might resolve to the same ones, however thereafter they will.
    // also we don't care that we are blocking here.
    let addrs = url
        .inner()
        .socket_addrs(|| None)?
        .iter()
        .map(|addr| addr.ip())
        .collect();

    let tunnel = ClientTunnel::new(
        private_key,
        tcp_socket_factory.clone(),
        udp_socket_factory,
        callbacks,
        HashMap::from([(url.host().to_string(), addrs)]),
    )?;

    let portal = PhoenixChannel::connect(
        Secret::new(url),
        get_user_agent(os_version_override, &app_version),
        PHOENIX_TOPIC,
        (),
        ExponentialBackoffBuilder::default()
            .with_max_elapsed_time(max_partition_time)
            .build(),
        tcp_socket_factory,
    );

    let mut eventloop = Eventloop::new(tunnel, portal, rx);

    std::future::poll_fn(|cx| eventloop.poll(cx))
        .await
        .map_err(Error::PortalConnectionFailed)?;

    Ok(())
}

/// A supervisor task that handles, when [`connect`] exits.
async fn connect_supervisor<CB>(connect_handle: JoinHandle<Result<(), Error>>, callbacks: CB)
where
    CB: Callbacks,
{
    match connect_handle.await {
        Ok(Ok(())) => {
            tracing::info!("connlib exited gracefully");
        }
        Ok(Err(e)) => {
            tracing::error!("connlib failed: {e}");
            callbacks.on_disconnect(&e);
        }
        Err(e) => match e.try_into_panic() {
            Ok(panic) => {
                tracing::error!("connlib panicked");
                if let Some(msg) = panic.downcast_ref::<&str>() {
                    callbacks.on_disconnect(&Error::Panic(msg.to_string()));
                    return;
                }
                if let Some(msg) = panic.downcast_ref::<String>() {
                    callbacks.on_disconnect(&Error::Panic(msg.to_string()));
                    return;
                }

                callbacks.on_disconnect(&Error::PanicNonStringPayload);
            }
            Err(_) => {
                tracing::error!("connlib task was cancelled");
                callbacks.on_disconnect(&Error::Cancelled);
            }
        },
    }
}

#[cfg(test)]
mod tests {
    #[derive(Clone, Default)]
    struct Callbacks {}
    impl connlib_shared::Callbacks for Callbacks {}

    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[ignore = "Performs system-wide I/O, needs sudo"]
    async fn device_linux() {
        device_common().await;
    }

    #[cfg(target_os = "windows")]
    #[tokio::test]
    #[ignore = "Performs system-wide I/O, needs sudo"]
    async fn device_windows() {
        // Install wintun so the test can run
        let wintun_path = connlib_shared::windows::wintun_dll_path().unwrap();
        tokio::fs::create_dir_all(wintun_path.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(&wintun_path, connlib_shared::windows::wintun_bytes())
            .await
            .unwrap();

        device_common().await;
    }

    #[cfg(any(target_os = "windows", target_os = "linux"))]
    async fn device_common() {
        use std::{collections::HashMap, sync::Arc};

        let (private_key, _public_key) = connlib_shared::keypair();
        let callbacks = Callbacks::default();
        let mut tunnel = firezone_tunnel::ClientTunnel::new(
            private_key,
            Arc::new(socket_factory::tcp),
            Arc::new(socket_factory::udp),
            callbacks,
            HashMap::new(),
        )
        .unwrap();
        let upstream_dns = vec![([192, 168, 1, 1], 53).into()];
        let interface = connlib_shared::messages::Interface {
            ipv4: [100, 71, 96, 96].into(),
            ipv6: [0xfd00, 0x2021, 0x1111, 0x0, 0x0, 0x0, 0x0019, 0x6538].into(),
            upstream_dns,
        };
        tunnel.set_new_interface_config(interface).unwrap();
        let resources = vec![];
        tunnel.add_resources(&resources).unwrap();

        let tunnel = tokio::spawn(async move {
            std::future::poll_fn(|cx| tunnel.poll_next_event(cx))
                .await
                .unwrap()
        });

        tokio::time::sleep(std::time::Duration::from_secs(5)).await;

        if tunnel.is_finished() {
            tunnel.await.unwrap();
        }
    }
}
