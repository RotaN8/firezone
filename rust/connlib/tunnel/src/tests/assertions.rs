use super::{
    sim_client::{RefClient, SimClient},
    sim_gateway::SimGateway,
};
use crate::tests::reference::ResourceDst;
use connlib_shared::{messages::GatewayId, DomainName};
use ip_packet::IpPacket;
use pretty_assertions::assert_eq;
use std::{
    collections::{hash_map::Entry, BTreeMap, HashMap, HashSet, VecDeque},
    net::IpAddr,
};

/// Asserts the following properties for all ICMP handshakes:
/// 1. An ICMP request on the client MUST result in an ICMP response using the same sequence, identifier and flipped src & dst IP.
/// 2. An ICMP request on the gateway MUST target the intended resource:
///     - For CIDR resources, that is the actual CIDR resource IP.
///     - For DNS resources, the IP must match one of the resolved IPs for the domain.
/// 3. For DNS resources, the mapping of proxy IP to actual resource IP must be stable.
pub(crate) fn assert_icmp_packets_properties(
    ref_client: &RefClient,
    sim_client: &SimClient,
    sim_gateways: HashMap<GatewayId, &SimGateway>,
    global_dns_records: &BTreeMap<DomainName, HashSet<IpAddr>>,
) {
    let unexpected_icmp_replies = find_unexpected_entries(
        &ref_client
            .expected_icmp_handshakes
            .values()
            .flatten()
            .collect(),
        &sim_client.received_icmp_replies,
        |(_, seq_a, id_a), (seq_b, id_b)| seq_a == seq_b && id_a == id_b,
    );
    assert_eq!(
        unexpected_icmp_replies,
        Vec::<&IpPacket>::new(),
        "Unexpected ICMP replies on client"
    );

    for (id, expected_icmp_handshakes) in ref_client.expected_icmp_handshakes.iter() {
        let gateway = sim_gateways.get(id).unwrap();

        assert_eq!(
            expected_icmp_handshakes.len(),
            gateway.received_icmp_requests.len(),
            "Unexpected ICMP requests on gateway {id}"
        );

        tracing::info!(target: "assertions", "✅ Performed the expected {} ICMP handshakes with gateway {id}", expected_icmp_handshakes.len());
    }

    let mut mapping = HashMap::new();

    // Assert properties of the individual ICMP handshakes per gateway.
    // Due to connlib's implementation of NAT64, we cannot match the packets sent by the client to the packets arriving at the resource by port or ICMP identifier.
    // Thus, we rely on the _order_ here which is why the packets are indexed by gateway in the `RefClient`.
    for (gateway, expected_icmp_handshakes) in &ref_client.expected_icmp_handshakes {
        let received_icmp_requests = &sim_gateways.get(gateway).unwrap().received_icmp_requests;

        for ((resource_dst, seq, identifier), gateway_received_request) in
            expected_icmp_handshakes.iter().zip(received_icmp_requests)
        {
            let _guard =
                tracing::info_span!(target: "assertions", "icmp", %seq, %identifier).entered();

            let client_sent_request = &sim_client
                .sent_icmp_requests
                .get(&(*seq, *identifier))
                .expect("to have ICMP request on client");
            let client_received_reply = &sim_client
                .received_icmp_replies
                .get(&(*seq, *identifier))
                .expect("to have ICMP reply on client");

            assert_correct_src_and_dst_ips(client_sent_request, client_received_reply);

            assert_eq!(
                gateway_received_request.source(),
                ref_client.tunnel_ip_for(gateway_received_request.source()),
                "ICMP request on gateway to originate from client"
            );

            match resource_dst {
                ResourceDst::Cidr(resource_dst) => {
                    assert_destination_is_cdir_resource(gateway_received_request, resource_dst)
                }
                ResourceDst::Dns(domain) => {
                    assert_destination_is_dns_resource(
                        gateway_received_request,
                        global_dns_records,
                        domain,
                    );

                    assert_proxy_ip_mapping_is_stable(
                        client_sent_request,
                        gateway_received_request,
                        &mut mapping,
                    )
                }
            }
        }
    }
}

pub(crate) fn assert_known_hosts_are_valid(ref_client: &RefClient, sim_client: &SimClient) {
    for (record, actual_addrs) in &sim_client.dns_records {
        if let Some(expected_addrs) = ref_client.known_hosts.get(&record.to_string()) {
            assert_eq!(actual_addrs, expected_addrs);
        }
    }
}

pub(crate) fn assert_dns_packets_properties(ref_client: &RefClient, sim_client: &SimClient) {
    let unexpected_icmp_replies = find_unexpected_entries(
        &ref_client.expected_dns_handshakes,
        &sim_client.received_dns_responses,
        |id_a, id_b| id_a == id_b,
    );

    assert_eq!(
        unexpected_icmp_replies,
        Vec::<&IpPacket>::new(),
        "Unexpected DNS replies on client"
    );

    for query_id in ref_client.expected_dns_handshakes.iter() {
        let _guard = tracing::info_span!(target: "assertions", "dns", %query_id).entered();

        let client_sent_query = sim_client
            .sent_dns_queries
            .get(query_id)
            .expect("to have DNS query on client");
        let client_received_response = sim_client
            .received_dns_responses
            .get(query_id)
            .expect("to have DNS response on client");

        assert_correct_src_and_dst_ips(client_sent_query, client_received_response);
        assert_correct_src_and_dst_udp_ports(client_sent_query, client_received_response);
    }
}

fn assert_correct_src_and_dst_ips(
    client_sent_request: &IpPacket<'_>,
    client_received_reply: &IpPacket<'_>,
) {
    assert_eq!(
        client_sent_request.destination(),
        client_received_reply.source(),
        "request destination == reply source"
    );

    tracing::info!(target: "assertions", "✅ dst IP of request matches src IP of response: {}", client_sent_request.destination());

    assert_eq!(
        client_sent_request.source(),
        client_received_reply.destination(),
        "request source == reply destination"
    );

    tracing::info!(target: "assertions", "✅ src IP of request matches dst IP of response: {}", client_sent_request.source());
}

fn assert_correct_src_and_dst_udp_ports(
    client_sent_request: &IpPacket<'_>,
    client_received_reply: &IpPacket<'_>,
) {
    let client_sent_request = client_sent_request.unwrap_as_udp();
    let client_received_reply = client_received_reply.unwrap_as_udp();

    assert_eq!(
        client_sent_request.get_destination(),
        client_received_reply.get_source(),
        "request destination == reply source"
    );

    tracing::info!(target: "assertions", "✅ dst port of request matches src port of response: {}", client_sent_request.get_destination());

    assert_eq!(
        client_sent_request.get_source(),
        client_received_reply.get_destination(),
        "request source == reply destination"
    );

    tracing::info!(target: "assertions", "✅ src port of request matches dst port of response: {}", client_sent_request.get_source());
}

fn assert_destination_is_cdir_resource(
    gateway_received_request: &IpPacket<'_>,
    expected_resource: &IpAddr,
) {
    let gateway_dst = gateway_received_request.destination();

    assert_eq!(
        gateway_dst, *expected_resource,
        "ICMP request on gateway to target correct CIDR resource"
    );

    tracing::info!(target: "assertions", "✅ {gateway_dst} is the correct resource");
}

fn assert_destination_is_dns_resource(
    gateway_received_request: &IpPacket<'_>,
    global_dns_records: &BTreeMap<DomainName, HashSet<IpAddr>>,
    expected_resource: &DomainName,
) {
    let actual_destination = gateway_received_request.destination();
    let possible_resource_ips = global_dns_records
        .get(expected_resource)
        .expect("ICMP packet for DNS resource to target known domain");

    assert!(
        possible_resource_ips.contains(&actual_destination),
        "ICMP request on gateway to target a known resource IP"
    );

    tracing::info!(target: "assertions", "✅ {actual_destination} is a valid IP for {expected_resource}");
}

/// Assert that the mapping of proxy IP to resource destination is stable.
///
/// How connlib assigns proxy IPs for domains is an implementation detail.
/// Yet, we care that it remains stable to ensure that any form of sticky sessions don't get broken (i.e. packets to one IP are always routed to the same IP on the gateway).
/// To assert this, we build up a map as we iterate through all packets that have been sent.
fn assert_proxy_ip_mapping_is_stable(
    client_sent_request: &IpPacket<'_>,
    gateway_received_request: &IpPacket<'_>,
    mapping: &mut HashMap<IpAddr, IpAddr>,
) {
    let client_dst = client_sent_request.destination();
    let gateway_dst = gateway_received_request.destination();

    match mapping.entry(client_dst) {
        Entry::Vacant(v) => {
            // We have to gradually discover connlib's mapping ...
            // For the first packet, we just save the IP that we ended up talking to.
            v.insert(gateway_dst);
        }
        Entry::Occupied(o) => {
            assert_eq!(
                gateway_dst,
                *o.get(),
                "ICMP request on client to target correct same IP of DNS resource"
            );
            tracing::info!(target: "assertions", "✅ {client_dst} maps to {gateway_dst}");
        }
    }
}

fn find_unexpected_entries<'a, E, K, V>(
    expected: &VecDeque<E>,
    actual: &'a HashMap<K, V>,
    is_equal: impl Fn(&E, &K) -> bool,
) -> Vec<&'a V> {
    actual
        .iter()
        .filter(|(k, _)| !expected.iter().any(|e| is_equal(e, k)))
        .map(|(_, v)| v)
        .collect()
}
