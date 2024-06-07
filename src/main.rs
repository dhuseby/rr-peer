use libp2p::{
    futures::StreamExt,
    identify,
    identity,
    mdns,
    Multiaddr,
    PeerId,
    request_response::{self, ProtocolSupport},
    StreamProtocol,
    swarm::{dial_opts::{DialOpts, PeerCondition}, NetworkBehaviour, SwarmEvent},
};
use serde::{Serialize, Deserialize};
use std::{collections::BTreeSet, error::Error};
use tokio::{select, time::{interval, Duration}};
use tracing_subscriber::filter::EnvFilter;

/// agent version
const AGENT_VERSION: &'static str = "peer/0.0.1";

#[derive(Debug, Serialize, Deserialize)]
struct GreetRequest {
    message: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct GreetResponse {
    message: String,
}

#[derive(NetworkBehaviour)]
struct Behaviour {
    identify: identify::Behaviour,
    request_response: request_response::cbor::Behaviour<GreetRequest, GreetResponse>,
    mdns: mdns::tokio::Behaviour,
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
                let cfg = identify::Config::new("/foo/bar/1".to_string(), key.public())
                    .with_push_listen_addr_updates(true)
                    .with_agent_version(AGENT_VERSION.to_string());
                identify::Behaviour::new(cfg)
            },
            request_response: {
                let cfg = request_response::Config::default()
                    .with_max_concurrent_streams(10);
                request_response::cbor::Behaviour::<GreetRequest, GreetResponse>::new(
                    [(StreamProtocol::new("/foo/1"), ProtocolSupport::Full)], cfg)
            },
            mdns: {
                let cfg = mdns::Config::default();
                mdns::tokio::Behaviour::new(cfg, key.public().to_peer_id()).unwrap()
            },
        })?
        .with_swarm_config(|c| c.with_idle_connection_timeout(Duration::from_secs(60)))
        .build();

    swarm.listen_on("/ip4/0.0.0.0/udp/0/quic-v1".parse()?)?;

    // set up a timer to tick every 30 seconds
    let mut timer = interval(Duration::from_secs(5));
    let mut seen = Box::pin(BTreeSet::default());
    let mut peers = Box::pin(BTreeSet::default());
    let mut my_addr = Box::pin(Multiaddr::empty());

    loop {
        select! {
            _ = timer.tick() => {
                if !connected.is_empty() {
                    println!("Greeting Peers!");
                    let connected: Vec<PeerId> = swarm.connected_peers().cloned().collect();
                    for peer_id in &connected {
                        if peers.contains(peer_id) {
                            println!("Greeting: {peer_id}");
                            swarm.behaviour_mut()
                                .request_response
                                .send_request(peer_id, GreetRequest { message: format!("Hello from {my_addr}") });
                        }
                    }
                }
            }
            event = swarm.select_next_some() => match event {
                SwarmEvent::Behaviour(BehaviourEvent::RequestResponse(request_response::Event::Message { message, .. })) => match message {
                    request_response::Message::Request { request, channel, .. } => {
                        let req: GreetRequest = request;
                        println!("received request: {}", req.message);
                        swarm.behaviour_mut()
                            .request_response
                            .send_response(channel, GreetResponse { message: format!("Hello back from {my_addr}") })
                            .expect("peer connection closed?");
                    }
                    request_response::Message::Response { response, .. } => {
                        let resp: GreetResponse = response;
                        println!("received response: {}", resp.message);
                    }
                }
                SwarmEvent::Behaviour(BehaviourEvent::Mdns(mdns::Event::Discovered(list))) => {
                    for (peer_id, multiaddr) in list {
                        if peer_id != local_peer_id && seen.insert(peer_id) {
                            println!("mDNS discovered a new peer: {peer_id}");
                            let dial_opts = DialOpts::peer_id(peer_id)
                                .condition(PeerCondition::DisconnectedAndNotDialing)
                                .addresses(vec![multiaddr])
                                .build();
                            println!("Dialing {my_addr} -> {peer_id}");
                            let _ = swarm.dial(dial_opts);
                        }
                    }
                }
                SwarmEvent::Behaviour(BehaviourEvent::Mdns(mdns::Event::Expired(list))) => {
                    for (peer_id, _multiaddr) in list {
                        if peer_id != local_peer_id {
                            println!("mDNS peer has expired: {peer_id}");
                            println!("Disconnecting from {peer_id}");
                            seen.remove(&peer_id);
                            peers.remove(&peer_id);
                            swarm.disconnect_peer_id(peer_id).expect(&format!("failed to disconnect from {peer_id}"));
                        }
                    }
                }
                SwarmEvent::Behaviour(BehaviourEvent::Identify(identify::Event::Received { peer_id, info })) => {
                    if peer_id != local_peer_id {
                        if info.agent_version == AGENT_VERSION.to_string() {
                            if peers.insert(peer_id) {
                                println!("Found another peer: {peer_id}");
                            }
                        } else {
                            println!("{peer_id} doesn't speak {}", info.protocol_version);
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
                _ => {}
            }
        }
    }
}
