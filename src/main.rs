use libp2p::{
    core::ConnectedPoint,
    futures::{stream, StreamExt},
    identify,
    identity,
    Multiaddr,
    PeerId,
    request_response::{self, ProtocolSupport},
    StreamProtocol,
    swarm::{/*dial_opts::{DialOpts, PeerCondition},*/ NetworkBehaviour, SwarmEvent},
};
use serde::{Serialize, Deserialize};
use std::{collections::BTreeMap, error::Error};
use tokio::{select, time::{interval, Duration}};
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

    // set up a timer to tick every 10 seconds
    let mut timer = Box::pin(stream::unfold(interval(Duration::from_secs(10)), |mut interval| async {
        interval.tick().await;
        Some(((), interval))
    }));

    let mut peers = Box::pin(BTreeMap::<PeerId, Multiaddr>::default());
    let mut clients = Box::pin(BTreeMap::<PeerId, Multiaddr>::default());
    let mut my_addr = Box::pin(Multiaddr::empty());

    loop {
        select! {
            Some(_) = timer.next() => {
                let connected: Vec<PeerId> = swarm.connected_peers().cloned().collect();
                if !connected.is_empty() {
                    println!("Greeting {} Peers!", connected.len());
                    for peer_id in &connected {
                        match peers.get(peer_id) {
                            Some(address) => {
                                println!("Greeting: {peer_id}");
                                swarm.behaviour_mut()
                                    .request_response
                                    .send_request(peer_id, GreetRequest { message: format!("Hello!!"), address: address.clone() });
                            }
                            None => {
                                println!("Peer {peer_id} not in list of peers");
                            }
                        }
                    }
                }
            }
            event = swarm.select_next_some() => match event {
                SwarmEvent::Behaviour(BehaviourEvent::RequestResponse(request_response::Event::Message { peer, message })) => match message {
                    request_response::Message::Request { request, channel, .. } => {
                        let req: GreetRequest = request;
                        println!("Request from {}: {}", &req.address, req.message);
                        let address = clients.get(&peer).unwrap_or(&Multiaddr::empty()).clone();
                        swarm.behaviour_mut()
                            .request_response
                            .send_response(channel, GreetResponse { message: format!("Hello Back!!"), address: address.clone() })
                            .expect("peer connection closed?");
                        *my_addr = req.address;

                        swarm.dial(address)?;
                    }
                    request_response::Message::Response { response, .. } => {
                        let resp: GreetResponse = response;
                        println!("Response from {}: {}", resp.address, resp.message);
                        *my_addr = resp.address;
                    }
                }
                SwarmEvent::Behaviour(BehaviourEvent::Identify(identify::Event::Received { peer_id, info })) => {
                    if peer_id != local_peer_id {
                        if info.agent_version == AGENT_VERSION.to_string() {
                            println!("Peer {peer_id} speaks our protocol");
                        } else {
                            println!("{peer_id} doesn't speak our protocol");
                            println!("Disconnecting from {peer_id}");
                            peers.remove(&peer_id);
                            swarm.disconnect_peer_id(peer_id).expect(&format!("failed to disconnect from {peer_id}"));
                        }
                    }
                }
                SwarmEvent::NewListenAddr { address, .. } => {
                    println!("Local peer is listening on {address}");
                    *my_addr = address;
                }
                SwarmEvent::Dialing { peer_id, .. } => {
                    if let Some(peer_id) = peer_id {
                        println!("Dialing {peer_id}");
                    }
                }
                SwarmEvent::ConnectionEstablished { peer_id, endpoint, .. } => {
                    match endpoint {
                        ConnectedPoint::Dialer { address, .. } => {
                            println!("Successfully dialed to {peer_id}: {address}");
                            peers.insert(peer_id, address.clone());
                        }
                        ConnectedPoint::Listener { send_back_addr, .. } => {
                            clients.insert(peer_id, send_back_addr.clone());
                            println!("Successfully received dial from {peer_id}: {send_back_addr}");
                        }
                    }
                }
                SwarmEvent::ConnectionClosed { peer_id, connection_id, .. } => {
                    println!("Connection to {peer_id}:{connection_id} closed");
                    clients.remove(&peer_id);
                    peers.remove(&peer_id);
                }
                SwarmEvent::ExternalAddrConfirmed { address } => {
                    println!("External address confirmed as {address}");
                    *my_addr = address;
                }
                _ => {}
            }
        }
    }
}
