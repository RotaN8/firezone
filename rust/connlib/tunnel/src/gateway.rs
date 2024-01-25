use crate::device_channel::Device;
use crate::peer::PacketTransformGateway;
use crate::sockets::{Socket, UdpSockets};
use crate::{sleep_until, ConnectedPeer, Event, RoleState, Tunnel, MAX_UDP_SIZE};
use boringtun::x25519::StaticSecret;
use connlib_shared::messages::{ClientId, Interface as InterfaceConfig};
use connlib_shared::Callbacks;
use firezone_connection::{ConnectionPool, ServerConnectionPool};
use futures_util::future::BoxFuture;
use futures_util::FutureExt;
use if_watch::tokio::IfWatcher;
use ip_network_table::IpNetworkTable;
use itertools::Itertools;
use rand_core::OsRng;
use std::collections::VecDeque;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::task::{Context, Poll};

const PEERS_IPV4: &str = "100.64.0.0/11";
const PEERS_IPV6: &str = "fd00:2021:1111::/107";

impl<CB> Tunnel<CB, GatewayState>
where
    CB: Callbacks + 'static,
{
    /// Sets the interface configuration and starts background tasks.
    #[tracing::instrument(level = "trace", skip(self))]
    pub fn set_interface(&self, config: &InterfaceConfig) -> connlib_shared::Result<()> {
        // Note: the dns fallback strategy is irrelevant for gateways
        let device = Arc::new(Device::new(config, vec![], self.callbacks())?);

        let result_v4 = device.add_route(PEERS_IPV4.parse().unwrap(), self.callbacks());
        let result_v6 = device.add_route(PEERS_IPV6.parse().unwrap(), self.callbacks());
        result_v4.or(result_v6)?;

        self.device.store(Some(device.clone()));
        self.no_device_waker.wake();

        tracing::debug!("background_loop_started");

        Ok(())
    }

    /// Clean up a connection to a resource.
    // FIXME: this cleanup connection is wrong!
    pub fn cleanup_connection(&self, id: ClientId) {
        // TODO:
        // self.peer_connections.lock().remove(&id);
    }
}

/// [`Tunnel`] state specific to gateways.
pub struct GatewayState {
    #[allow(clippy::type_complexity)]
    pub peers_by_ip: IpNetworkTable<ConnectedPeer<ClientId, PacketTransformGateway>>,
    pub connection_pool: ServerConnectionPool<ClientId>,
    connection_pool_timeout: BoxFuture<'static, std::time::Instant>,
    if_watcher: IfWatcher,
    udp_sockets: UdpSockets<MAX_UDP_SIZE>,
    relay_socket: Socket<MAX_UDP_SIZE>,
    write_buf: Box<[u8; MAX_UDP_SIZE]>,
}

impl Default for GatewayState {
    fn default() -> Self {
        let if_watcher = IfWatcher::new().expect(
            "Program should be able to list interfaces on the system. Check binary's permissions",
        );
        let mut connection_pool = ConnectionPool::new(
            StaticSecret::random_from_rng(OsRng),
            std::time::Instant::now(),
        );
        let mut udp_sockets = UdpSockets::default();

        for ip in if_watcher.iter() {
            tracing::info!(address = %ip.addr(), "New local interface address found");
            match udp_sockets.bind((ip.addr(), 0)) {
                Ok(addr) => connection_pool.add_local_interface(addr),
                Err(e) => {
                    tracing::debug!(address = %ip.addr(), err = ?e, "Couldn't bind socket to interface: {e:#?}")
                }
            }
        }

        let relay_socket = Socket::bind((IpAddr::from(Ipv4Addr::UNSPECIFIED), 0))
            .expect("Program should be able to bind to 0.0.0.0:0 to be able to connect to relays");

        Self {
            peers_by_ip: IpNetworkTable::new(),
            connection_pool,
            if_watcher,
            udp_sockets,
            relay_socket,
            connection_pool_timeout: sleep_until(std::time::Instant::now()).boxed(),
            write_buf: Box::new([0; MAX_UDP_SIZE]),
        }
    }
}

impl RoleState for GatewayState {
    type Id = ClientId;

    fn add_remote_candidate(&mut self, conn_id: ClientId, ice_candidate: String) {
        self.connection_pool
            .add_remote_candidate(conn_id, ice_candidate);
    }

    fn poll_next_event(&mut self, cx: &mut Context<'_>) -> Poll<Event<Self::Id>> {
        loop {
            while let Some(transmit) = self.connection_pool.poll_transmit() {
                if let Err(e) = match transmit.src {
                    Some(src) => self
                        .udp_sockets
                        .try_send_to(src, transmit.dst, &transmit.payload),
                    None => self
                        .relay_socket
                        .try_send_to(transmit.dst, &transmit.payload),
                } {
                    tracing::warn!(src = ?transmit.src, dst = %transmit.dst, "Failed to send UDP packet: {e:#?}");
                }
            }

            match self.connection_pool.poll_event() {
                Some(firezone_connection::Event::SignalIceCandidate {
                    connection,
                    candidate,
                }) => {
                    return Poll::Ready(Event::SignalIceCandidate {
                        conn_id: connection,
                        candidate,
                    })
                }
                Some(firezone_connection::Event::ConnectionEstablished(id)) => todo!(),
                Some(firezone_connection::Event::ConnectionFailed(id)) => todo!(),
                None => {}
            }

            if let Poll::Ready(instant) = self.connection_pool_timeout.poll_unpin(cx) {
                self.connection_pool.handle_timeout(instant);
                if let Some(timeout) = self.connection_pool.poll_timeout() {
                    self.connection_pool_timeout = sleep_until(timeout).boxed();
                }

                continue;
            }
            match self.udp_sockets.poll_recv_from(cx) {
                Poll::Ready((local, Ok((from, packet)))) => {
                    tracing::trace!(target: "wire", %local, %from, bytes = %packet.filled().len(), "read new packet");
                    match self.connection_pool.decapsulate(
                        local,
                        from,
                        packet.filled(),
                        std::time::Instant::now(),
                        self.write_buf.as_mut(),
                    ) {
                        Ok(_) => {
                            // TODO
                        }
                        Err(e) => {
                            tracing::error!(%local, %from, "Failed to decapsulate incoming packet: {e:#?}");
                        }
                    }

                    continue;
                }
                Poll::Ready((addr, Err(e))) => {
                    tracing::error!(%addr, "Failed to read socket: {e:#?}");
                }
                Poll::Pending => {}
            }

            match self.relay_socket.poll_recv_from(cx) {
                Poll::Ready((local, Ok((from, packet)))) => {
                    tracing::trace!(target: "wire", %from, bytes = %packet.filled().len(), "read new relay packet");
                    match self.connection_pool.decapsulate(
                        local,
                        from,
                        packet.filled(),
                        std::time::Instant::now(),
                        self.write_buf.as_mut(),
                    ) {
                        Ok(_) => {
                            // TODO
                        }
                        Err(e) => {
                            tracing::error!(%from, "Failed to decapsulate incoming relay packet: {e:#?}");
                        }
                    }

                    continue;
                }
                Poll::Ready((_, Err(e))) => {
                    tracing::error!("Failed to read relay socket: {e:#?}");
                }
                Poll::Pending => {}
            }

            match self.if_watcher.poll_if_event(cx) {
                Poll::Ready(Ok(ev)) => match ev {
                    if_watch::IfEvent::Up(ip) => {
                        tracing::info!(address = %ip.addr(), "New local interface address found");
                        match self.udp_sockets.bind((ip.addr(), 0)) {
                            Ok(addr) => self.connection_pool.add_local_interface(addr),
                            Err(e) => {
                                tracing::debug!(address = %ip.addr(), err = ?e, "Couldn't bind socket to interface: {e:#?}")
                            }
                        }
                    }
                    if_watch::IfEvent::Down(ip) => {
                        tracing::info!(address = %ip.addr(), "Interface IP no longer available");
                        todo!()
                    }
                },
                Poll::Ready(Err(e)) => {
                    tracing::debug!("Error while polling interfces: {e:#?}");
                }
                Poll::Pending => {}
            }
        }
    }

    fn remove_peers(&mut self, conn_id: ClientId) {
        self.peers_by_ip.retain(|_, p| p.inner.conn_id != conn_id);
    }

    fn refresh_peers(&mut self) -> VecDeque<Self::Id> {
        let mut peers_to_stop = VecDeque::new();
        for (_, peer) in self.peers_by_ip.iter().unique_by(|(_, p)| p.inner.conn_id) {
            let conn_id = peer.inner.conn_id;

            peer.inner.transform.expire_resources();

            if peer.inner.transform.is_emptied() {
                tracing::trace!(%conn_id, "peer_expired");
                peers_to_stop.push_back(conn_id);

                continue;
            }

            // TODO:
            // let bytes = match peer.inner.update_timers() {
            //     Ok(Some(bytes)) => bytes,
            //     Ok(None) => continue,
            //     Err(e) => {
            //         tracing::error!("Failed to update timers for peer: {e}");
            //         if e.is_fatal_connection_error() {
            //             peers_to_stop.push_back(conn_id);
            //         }

            //         continue;
            //     }
            // };

            let peer_channel = peer.channel.clone();

            tokio::spawn(async move {
                if let Err(e) = peer_channel.send(todo!()).await {
                    tracing::error!("Failed to send packet to peer: {e:#}");
                }
            });
        }

        peers_to_stop
    }
}
