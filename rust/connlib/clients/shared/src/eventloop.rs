use crate::{
    messages::{
        Connect, ConnectionDetails, EgressMessages, GatewayIceCandidates, GatewaysIceCandidates,
        IngressMessages, InitClient, ReplyMessages,
    },
    PHOENIX_TOPIC,
};
use anyhow::Result;
use connlib_shared::{
    messages::{ConnectionAccepted, GatewayResponse, RelaysPresence, ResourceAccepted, ResourceId},
    Callbacks,
};
use firezone_tunnel::{ClientTunnel, Tun};
use phoenix_channel::{ErrorReply, OutboundRequestId, PhoenixChannel};
use std::{
    collections::{HashMap, HashSet},
    net::IpAddr,
    task::{Context, Poll},
};

pub struct Eventloop<C: Callbacks> {
    tunnel: ClientTunnel<C>,

    portal: PhoenixChannel<(), IngressMessages, ReplyMessages>,
    rx: tokio::sync::mpsc::UnboundedReceiver<Command>,

    connection_intents: SentConnectionIntents,
}

/// Commands that can be sent to the [`Eventloop`].
pub enum Command {
    Stop,
    Reconnect,
    SetDns(Vec<IpAddr>),
    SetTun(Tun),
}

impl<C: Callbacks> Eventloop<C> {
    pub(crate) fn new(
        tunnel: ClientTunnel<C>,
        portal: PhoenixChannel<(), IngressMessages, ReplyMessages>,
        rx: tokio::sync::mpsc::UnboundedReceiver<Command>,
    ) -> Self {
        Self {
            tunnel,
            portal,
            connection_intents: SentConnectionIntents::default(),
            rx,
        }
    }
}

impl<C> Eventloop<C>
where
    C: Callbacks + 'static,
{
    pub fn poll(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), phoenix_channel::Error>> {
        loop {
            match self.rx.poll_recv(cx) {
                Poll::Ready(Some(Command::Stop)) | Poll::Ready(None) => return Poll::Ready(Ok(())),
                Poll::Ready(Some(Command::SetDns(dns))) => {
                    self.tunnel.set_new_dns(dns);

                    continue;
                }
                Poll::Ready(Some(Command::SetTun(tun))) => {
                    self.tunnel.set_tun(tun);
                    continue;
                }
                Poll::Ready(Some(Command::Reconnect)) => {
                    self.portal.reconnect();
                    if let Err(e) = self.tunnel.reset() {
                        tracing::warn!("Failed to reconnect tunnel: {e}");
                    }

                    continue;
                }
                Poll::Pending => {}
            }

            match self.tunnel.poll_next_event(cx) {
                Poll::Ready(Ok(event)) => {
                    self.handle_tunnel_event(event);
                    continue;
                }
                Poll::Ready(Err(e)) => {
                    tracing::warn!("Tunnel error: {e}");
                    continue;
                }
                Poll::Pending => {}
            }

            match self.portal.poll(cx)? {
                Poll::Ready(event) => {
                    self.handle_portal_event(event);
                    continue;
                }
                Poll::Pending => {}
            }

            return Poll::Pending;
        }
    }

    fn handle_tunnel_event(&mut self, event: firezone_tunnel::ClientEvent) {
        match event {
            firezone_tunnel::ClientEvent::AddedIceCandidates {
                conn_id: gateway,
                candidates,
            } => {
                tracing::debug!(%gateway, ?candidates, "Sending new ICE candidates to gateway");

                self.portal.send(
                    PHOENIX_TOPIC,
                    EgressMessages::BroadcastIceCandidates(GatewaysIceCandidates {
                        gateway_ids: vec![gateway],
                        candidates,
                    }),
                );
            }
            firezone_tunnel::ClientEvent::RemovedIceCandidates {
                conn_id: gateway,
                candidates,
            } => {
                tracing::debug!(%gateway, ?candidates, "Sending invalidated ICE candidates to gateway");

                self.portal.send(
                    PHOENIX_TOPIC,
                    EgressMessages::BroadcastInvalidatedIceCandidates(GatewaysIceCandidates {
                        gateway_ids: vec![gateway],
                        candidates,
                    }),
                );
            }
            firezone_tunnel::ClientEvent::ConnectionIntent {
                connected_gateway_ids,
                resource,
                ..
            } => {
                let id = self.portal.send(
                    PHOENIX_TOPIC,
                    EgressMessages::PrepareConnection {
                        resource_id: resource,
                        connected_gateway_ids,
                    },
                );
                self.connection_intents.register_new_intent(id, resource);
            }
            firezone_tunnel::ClientEvent::SendProxyIps { connections } => {
                for connection in connections {
                    self.portal
                        .send(PHOENIX_TOPIC, EgressMessages::ReuseConnection(connection));
                }
            }
            firezone_tunnel::ClientEvent::ResourcesChanged { resources } => {
                // Note: This may look a bit weird: We are reading an event from the tunnel and yet delegate back to the tunnel here.
                // Couldn't the tunnel just do this internally?
                // Technically, yes.
                // But, we are only accessing the callbacks here which _eventually_ will be removed from `Tunnel`.
                // At that point, the tunnel has to emit this event and we need to handle it without delegating back to the tunnel.
                // We only access the callbacks here because `Tunnel` already has them and the callbacks are the current way of talking to the UI.
                // At a later point, we will probably map to another event here that gets pushed further up.

                self.tunnel.callbacks.on_update_resources(resources)
            }
            firezone_tunnel::ClientEvent::DnsServersChanged { .. } => {
                // Unhandled for now.
                // As we decouple the core of connlib from the callbacks, this is where we will hook into the DNS server change and notify our clients to set new DNS servers on their platform.
                // See https://github.com/firezone/firezone/issues/5106 for details.
            }
        }
    }

    fn handle_portal_event(
        &mut self,
        event: phoenix_channel::Event<IngressMessages, ReplyMessages>,
    ) {
        match event {
            phoenix_channel::Event::InboundMessage { msg, .. } => {
                self.handle_portal_inbound_message(msg);
            }
            phoenix_channel::Event::SuccessResponse { res, req_id, .. } => {
                self.handle_portal_success_reply(res, req_id);
            }
            phoenix_channel::Event::ErrorResponse { res, req_id, topic } => {
                self.handle_portal_error_reply(res, topic, req_id);
            }
            phoenix_channel::Event::HeartbeatSent => {}
            phoenix_channel::Event::JoinedRoom { .. } => {}
            phoenix_channel::Event::Closed => {
                unimplemented!("Client never actively closes the portal connection")
            }
        }
    }

    fn handle_portal_inbound_message(&mut self, msg: IngressMessages) {
        match msg {
            IngressMessages::ConfigChanged(config) => {
                if let Err(e) = self
                    .tunnel
                    .set_new_interface_config(config.interface.clone())
                {
                    tracing::warn!(?config, "Failed to update configuration: {e:?}");
                }
            }
            IngressMessages::IceCandidates(GatewayIceCandidates {
                gateway_id,
                candidates,
            }) => {
                for candidate in candidates {
                    self.tunnel.add_ice_candidate(gateway_id, candidate)
                }
            }
            IngressMessages::Init(InitClient {
                interface,
                resources,
                relays,
            }) => {
                if let Err(e) = self.tunnel.set_new_interface_config(interface) {
                    tracing::warn!("Failed to set interface on tunnel: {e}");
                    return;
                }

                tracing::info!("Firezone Started!");
                self.tunnel.set_resources(resources);
                self.tunnel.update_relays(HashSet::default(), relays)
            }
            IngressMessages::ResourceCreatedOrUpdated(resource) => {
                self.tunnel.add_resources(&[resource]);
            }
            IngressMessages::ResourceDeleted(resource) => {
                self.tunnel.remove_resources(&[resource]);
            }
            IngressMessages::RelaysPresence(RelaysPresence {
                disconnected_ids,
                connected,
            }) => self
                .tunnel
                .update_relays(HashSet::from_iter(disconnected_ids), connected),
            IngressMessages::InvalidateIceCandidates(GatewayIceCandidates {
                gateway_id,
                candidates,
            }) => {
                for candidate in candidates {
                    self.tunnel.remove_ice_candidate(gateway_id, candidate)
                }
            }
        }
    }

    fn handle_portal_success_reply(&mut self, res: ReplyMessages, req_id: OutboundRequestId) {
        match res {
            ReplyMessages::Connect(Connect {
                gateway_payload:
                    GatewayResponse::ConnectionAccepted(ConnectionAccepted { ice_parameters, .. }),
                gateway_public_key,
                resource_id,
                ..
            }) => {
                if let Err(e) = self.tunnel.received_offer_response(
                    resource_id,
                    ice_parameters,
                    gateway_public_key.0.into(),
                ) {
                    tracing::warn!("Failed to accept connection: {e}");
                }
            }
            ReplyMessages::Connect(Connect {
                gateway_payload: GatewayResponse::ResourceAccepted(ResourceAccepted { .. }),
                ..
            }) => {
                tracing::trace!("Connection response received, ignored as it's deprecated")
            }
            ReplyMessages::ConnectionDetails(ConnectionDetails {
                gateway_id,
                resource_id,
                site_id,
                ..
            }) => {
                let should_accept = self
                    .connection_intents
                    .handle_connection_details_received(req_id, resource_id);

                if !should_accept {
                    tracing::debug!(%resource_id, "Ignoring stale connection details");
                    return;
                }

                match self
                    .tunnel
                    .create_or_reuse_connection(resource_id, gateway_id, site_id)
                {
                    Ok(Some(firezone_tunnel::Request::NewConnection(connection_request))) => {
                        // TODO: keep track for the response
                        let _id = self.portal.send(
                            PHOENIX_TOPIC,
                            EgressMessages::RequestConnection(connection_request),
                        );
                    }
                    Ok(Some(firezone_tunnel::Request::ReuseConnection(connection_request))) => {
                        // TODO: keep track for the response
                        let _id = self.portal.send(
                            PHOENIX_TOPIC,
                            EgressMessages::ReuseConnection(connection_request),
                        );
                    }
                    Ok(None) => {
                        tracing::debug!(%resource_id, %gateway_id, "Pending connection");
                    }
                    Err(e) => {
                        tracing::warn!("Failed to request new connection: {e}");
                    }
                };
            }
        }
    }

    fn handle_portal_error_reply(
        &mut self,
        res: ErrorReply,
        topic: String,
        req_id: OutboundRequestId,
    ) {
        match res {
            ErrorReply::Offline => {
                let Some(offline_resource) = self.connection_intents.handle_error(req_id) else {
                    return;
                };

                tracing::debug!(resource_id = %offline_resource, "Resource is offline");

                self.tunnel.set_resource_offline(offline_resource);
            }

            ErrorReply::Disabled => {
                tracing::debug!(%req_id, "Functionality is disabled");
            }
            ErrorReply::UnmatchedTopic => {
                self.portal.join(topic, ());
            }
            reason @ (ErrorReply::InvalidVersion | ErrorReply::NotFound | ErrorReply::Other) => {
                tracing::debug!(%req_id, %reason, "Request failed");
            }
        }
    }
}

#[derive(Default)]
struct SentConnectionIntents {
    inner: HashMap<OutboundRequestId, ResourceId>,
}

impl SentConnectionIntents {
    fn register_new_intent(&mut self, id: OutboundRequestId, resource: ResourceId) {
        self.inner.insert(id, resource);
    }

    /// To be called when we receive the connection details for a particular resource.
    ///
    /// Returns whether we should accept them.
    fn handle_connection_details_received(
        &mut self,
        reference: OutboundRequestId,
        r: ResourceId,
    ) -> bool {
        let has_more_recent_intent = self
            .inner
            .iter()
            .any(|(req, resource)| req > &reference && resource == &r);

        if has_more_recent_intent {
            return false;
        }

        let has_intent = self
            .inner
            .get(&reference)
            .is_some_and(|resource| resource == &r);

        if !has_intent {
            return false;
        }

        self.inner.retain(|_, v| v != &r);

        true
    }

    fn handle_error(&mut self, req: OutboundRequestId) -> Option<ResourceId> {
        self.inner.remove(&req)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discards_old_connection_intent() {
        let mut intents = SentConnectionIntents::default();

        let resource = ResourceId::random();

        intents.register_new_intent(OutboundRequestId::for_test(1), resource);
        intents.register_new_intent(OutboundRequestId::for_test(2), resource);

        let should_accept =
            intents.handle_connection_details_received(OutboundRequestId::for_test(1), resource);

        assert!(!should_accept);
    }

    #[test]
    fn allows_unrelated_intents() {
        let mut intents = SentConnectionIntents::default();

        let resource1 = ResourceId::random();
        let resource2 = ResourceId::random();

        intents.register_new_intent(OutboundRequestId::for_test(1), resource1);
        intents.register_new_intent(OutboundRequestId::for_test(2), resource2);

        let should_accept_1 =
            intents.handle_connection_details_received(OutboundRequestId::for_test(1), resource1);
        let should_accept_2 =
            intents.handle_connection_details_received(OutboundRequestId::for_test(2), resource2);

        assert!(should_accept_1);
        assert!(should_accept_2);
    }

    #[test]
    fn handles_out_of_order_responses() {
        let mut intents = SentConnectionIntents::default();

        let resource = ResourceId::random();

        intents.register_new_intent(OutboundRequestId::for_test(1), resource);
        intents.register_new_intent(OutboundRequestId::for_test(2), resource);

        let should_accept_2 =
            intents.handle_connection_details_received(OutboundRequestId::for_test(2), resource);
        let should_accept_1 =
            intents.handle_connection_details_received(OutboundRequestId::for_test(1), resource);

        assert!(should_accept_2);
        assert!(!should_accept_1);
    }
}
