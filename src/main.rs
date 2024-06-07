use libp2p::{
    core::ConnectedPoint,
    identify,
    identity,
    mdns,
    Multiaddr,
    PeerId,
    request_response::{self, ProtocolSupport},
    StreamProtocol,
    swarm::{dial_opts::{DialOpts, PeerCondition}, NetworkBehaviour, SwarmEvent},
};
use futures::StreamExt;
use serde::{Serialize, Deserialize};
use std::{collections::{BTreeSet, BTreeMap}, error::Error};
use tokio::{select, sync::mpsc, time::{interval, Duration}};
use tracing_subscriber::filter::EnvFilter;

/// agent version
const AGENT_VERSION: &'static str = "peer/0.0.1";
const PROTOCOL: &'static str = "/foo/1";

#[derive(Debug, Serialize, Deserialize)]
struct GreetRequest {
    message: String,
    address: Multiaddr,
}

#[derive(Debug, Serialize, Deserialize)]
struct GreetResponse {
    message: String,
    address: Multiaddr,
}

#[derive(NetworkBehaviour)]
struct Behaviour {
    identify: identify::Behaviour,
    mdns: mdns::tokio::Behaviour,
    request_response: request_response::cbor::Behaviour<GreetRequest, GreetResponse>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .try_init();

    let local_key = identity::Keypair::generate_ed25519();
    let local_peer_id = PeerId::from(local_key.public());
    println!("local peer id: {local_peer_id}");

    let mut swarm = libp2p::SwarmBuilder::with_existing_identity(local_key)
        .with_tokio()
        .with_quic()
        .with_behaviour(|key| Behaviour {
            identify: {
                let cfg = identify::Config::new(PROTOCOL.to_string(), key.public())
                    .with_push_listen_addr_updates(true)
                    .with_agent_version(AGENT_VERSION.to_string());
                identify::Behaviour::new(cfg)
            },
            mdns: {
                let cfg = mdns::Config::default();
                mdns::tokio::Behaviour::new(cfg, key.public().to_peer_id()).unwrap()
            },
            request_response: {
                let cfg = request_response::Config::default()
                    .with_max_concurrent_streams(10);
                request_response::cbor::Behaviour::<GreetRequest, GreetResponse>::new(
                    [(StreamProtocol::new(PROTOCOL), ProtocolSupport::Full)], cfg)
            },
        })?
        .with_swarm_config(|c| c.with_idle_connection_timeout(Duration::from_secs(60)))
        .build();

    if let Some(addr) = std::env::args().nth(1) {
        let remote: Multiaddr = addr.parse()?;
        swarm.dial(remote)?;
    }

    swarm.listen_on("/ip4/0.0.0.0/udp/0/quic-v1".parse()?)?;

    // set up a timer to tick every 30 seconds
    let mut timer = interval(Duration::from_secs(5));
    let mut my_addr = Box::pin(Multiaddr::empty());

    // peers we've seen from mDNS discovery
    let mut seen = Box::pin(BTreeSet::default());

    // connected peers that dialed us
    let mut in_peers = Box::pin(BTreeMap::<PeerId, Multiaddr>::default());

    // dialed peers we've connected to
    let mut out_peers = Box::pin(BTreeMap::<PeerId, Multiaddr>::default());

    // peers we should dial
    let (to_dial_send, mut to_dial_recv) = mpsc::unbounded_channel::<(PeerId, Multiaddr)>();

    loop {
        select! {
            _ = timer.tick() => {
                // go through connected peers and send them a request
                let connected: Vec<PeerId> = swarm.connected_peers().cloned().collect();
                if !connected.is_empty() {
                    for peer_id in &connected {
                        if let Some(address) = in_peers.get(peer_id) {
                            println!("IN Greeting: {peer_id}");
                            swarm.behaviour_mut()
                                .request_response
                                .send_request(peer_id, GreetRequest { message: format!("In Hello!!"), address: address.clone() });
                        }
                        if let Some(address) = out_peers.get(peer_id) {
                            println!("OUT Greeting: {peer_id}");
                            swarm.behaviour_mut()
                                .request_response
                                .send_request(peer_id, GreetRequest { message: format!("Out Hello!!"), address: address.clone() });
                        }
                    }
                }
            }
            Some((peer_id, multiaddr)) = to_dial_recv.recv() => {
                let dial_opts = DialOpts::peer_id(peer_id)
                    .condition(PeerCondition::DisconnectedAndNotDialing)
                    .addresses(vec![multiaddr])
                    .build();
                println!("Dialing {my_addr} -> {peer_id}");
                let _ = swarm.dial(dial_opts);
            }
            event = swarm.select_next_some() => match event {

                // Behavior Events

                SwarmEvent::Behaviour(BehaviourEvent::Identify(identify::Event::Received { peer_id, info })) => {
                    if peer_id != local_peer_id {
                        if info.protocol_version == PROTOCOL.to_string() {
                            println!("Peer {peer_id} speaks our protocol");
                        } else {
                            println!("{peer_id} doesn't speak our protocol");
                            println!("disconnecting from {peer_id}");
                            swarm.disconnect_peer_id(peer_id).expect(&format!("failed to disconnect from {peer_id}"));
                        }
                    }
                }
                SwarmEvent::Behaviour(BehaviourEvent::Mdns(event)) => match event {
                    mdns::Event::Discovered(list) => {
                        for (peer_id, multiaddr) in list {
                            if peer_id != local_peer_id && seen.insert(peer_id) {
                                println!("mDNS discovered a new peer: {peer_id}");
                                if !out_peers.contains_key(&peer_id) {
                                    to_dial_send.send((peer_id, multiaddr.clone()))
                                        .expect(&format!("failed to send dial for {peer_id}:{multiaddr}"));
                                }
                            }
                        }
                    }
                    mdns::Event::Expired(list) => {
                        for (peer_id, _multiaddr) in list {
                            if peer_id != local_peer_id {
                                println!("mDNS peer has expired: {peer_id}");
                                seen.remove(&peer_id);
                                swarm.disconnect_peer_id(peer_id).expect(&format!("failed to disconnect from {peer_id}"));
                            }
                        }
                    }
                }
                SwarmEvent::Behaviour(BehaviourEvent::RequestResponse(request_response::Event::Message { peer, message })) => match message {
                    request_response::Message::Request { request, channel, .. } => {
                        let peer_id = peer;
                        let req: GreetRequest = request;
                        print!("request from {peer_id}: \"{}\"", req.message);
                        // get the multiaddr we see the other peer at
                        let address = out_peers.get(&peer_id).unwrap_or(in_peers.get(&peer_id).unwrap_or(&Multiaddr::empty())).clone();
                        // send the response back along with the multiaddr we see the other peer at
                        swarm.behaviour_mut()
                            .request_response
                            .send_response(channel, GreetResponse { message: format!("Hello Back!!"), address: address.clone() })
                            .expect("peer connection closed?");
                        println!(" -> replied: \"Hello Back!!\"");

                        // since they told us what address they see us at, we can add that as our
                        // external address
                        swarm.add_external_address(req.address);
                    }
                    request_response::Message::Response { response, .. } => {
                        let peer_id = peer;
                        let resp: GreetResponse = response;
                        println!("response from {peer_id}: {}", resp.message);

                        // since they told us what address they see us at, we can add that as our
                        // external address
                        swarm.add_external_address(resp.address);
                    }
                }

                // Swarm Events

                SwarmEvent::ConnectionClosed { peer_id, connection_id, .. } => {
                    println!("connection to {peer_id}:{connection_id} closed");
                    in_peers.remove(&peer_id);
                    out_peers.remove(&peer_id);
                }
                SwarmEvent::ConnectionEstablished { peer_id, endpoint, .. } => {
                    match endpoint {
                        ConnectedPoint::Dialer { address, .. } => {
                            println!("dialed to {peer_id}: {address}");
                            out_peers.insert(peer_id, address.clone());
                        }
                        ConnectedPoint::Listener { send_back_addr, .. } => {
                            println!("received dial from {peer_id}: {send_back_addr}");
                            in_peers.insert(peer_id, send_back_addr.clone());

                            // if we don't have an outbound connection to this peer, add it to the
                            // list to be dialed
                            if !out_peers.contains_key(&peer_id) {
                                to_dial_send.send((peer_id, send_back_addr.clone()))
                                    .expect(&format!("failed to send dial for {peer_id}:{send_back_addr}"));
                            }
                        }
                    }
                }
                SwarmEvent::Dialing { peer_id, .. } => {
                    if let Some(peer_id) = peer_id {
                        println!("dialing {peer_id}...");
                    }
                }
                SwarmEvent::ExternalAddrConfirmed { address } => {
                    println!("external address confirmed as {address}");
                    *my_addr = address;
                }
                SwarmEvent::NewListenAddr { address, .. } => {
                    println!("local peer is listening on {address}");
                    *my_addr = address;
                }
                SwarmEvent::OutgoingConnectionError { peer_id, .. } => {
                    if let Some(peer_id) = peer_id {
                        println!("failed to dial {peer_id}...");
                    }
                }
                _ => {}
            }
        }
    }
}
