// Copyright 2018 Parity Technologies (UK) Ltd.
//
// Permission is hereby granted, free of charge, to any person obtaining a
// copy of this software and associated documentation files (the "Software"),
// to deal in the Software without restriction, including without limitation
// the rights to use, copy, modify, merge, publish, distribute, sublicense,
// and/or sell copies of the Software, and to permit persons to whom the
// Software is furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS
// OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
// FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
// DEALINGS IN THE SOFTWARE.

use futures::stream::StreamExt;
use libp2p::{
    identity, mdns, multiaddr, noise, swarm::NetworkBehaviour, swarm::SwarmEvent, tcp, yamux,
    Multiaddr, StreamProtocol, Swarm,
};
use libp2p_dcutr as dcutr;
use libp2p_gossipsub as gossipsub;
use libp2p_identify as identify;
use libp2p_kad as kad;
use libp2p_ping as ping;

use libp2p::multiaddr::Protocol;
use libp2p_identity::PeerId;
use std::error::Error;
use std::hash::Hash;
use std::str::FromStr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tari_crypto::keys::SecretKey;
use tari_crypto::ristretto::RistrettoSecretKey;
use tari_crypto::tari_utilities::ByteArray;
use tokio::{io, io::AsyncBufReadExt, select};
use tracing::debug;
use tracing_subscriber::EnvFilter;

macro_rules! pdbg {
    ($($arg:tt)*) => {
        println!("🐞 DEBUG: {}", format!($($arg)*));
    };
}

// We create a custom network behaviour that combines Gossipsub and Mdns.
#[derive(NetworkBehaviour)]
struct ChatBehaviour {
    ping: ping::Behaviour,
    gossipsub: gossipsub::Behaviour,
    mdns: mdns::tokio::Behaviour,
    kad: kad::Behaviour<kad::store::MemoryStore>,
    identify: identify::Behaviour,
    dcutr: dcutr::Behaviour,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let relay_addr = Multiaddr::from_str(
        "/ip4/127.0.0.1/tcp/4001/p2p/12D3KooWDpJ7As7BWAwRMfu1VU2WCqNjvq387JEYKDBj4kx6nXTN",
    )
    .unwrap();
    let _ = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .try_init();

    let a = RistrettoSecretKey::random(&mut rand::thread_rng())
        .as_bytes()
        .to_vec();
    let identity = libp2p_identity::Keypair::ed25519_from_bytes(a).unwrap();

    let mut swarm = libp2p::SwarmBuilder::with_existing_identity(identity)
        .with_tokio()
        .with_tcp(
            tcp::Config::default(),
            noise::Config::new,
            yamux::Config::default,
        )?
        .with_quic()
        .with_behaviour(|key| {
            // To content-address message, we can take the hash of message and use it as an ID.
            let message_id_fn = {
                let id = AtomicUsize::new(0);
                move |_message: &gossipsub::Message| {
                    // let mut s = DefaultHasher::new();
                    // message.data.hash(&mut s);
                    let next = id.fetch_add(1, Ordering::Relaxed);
                    gossipsub::MessageId::from(next.to_be_bytes())
                }
            };

            // Set a custom gossipsub configuration
            let gossipsub_config = gossipsub::ConfigBuilder::default()
                .heartbeat_interval(Duration::from_secs(10)) // This is set to aid debugging by not cluttering the log space
                .validation_mode(gossipsub::ValidationMode::Strict) // This sets the kind of message validation. The default is Strict (enforce message signing)
                .message_id_fn(message_id_fn) // content-address messages. No two messages of the same content will be propagated.
                .build()
                .map_err(|msg| io::Error::new(io::ErrorKind::Other, msg))?; // Temporary hack because `build` does not return a proper `std::error::Error`.

            // build a gossipsub network behaviour
            let gossipsub = gossipsub::Behaviour::new(
                gossipsub::MessageAuthenticity::Signed(key.clone()),
                gossipsub_config,
            )?;

            let mdns =
                mdns::tokio::Behaviour::new(mdns::Config::default(), key.public().to_peer_id())?;
            let kad = kad::Behaviour::new(
                key.public().to_peer_id(),
                kad::store::MemoryStore::new(key.public().to_peer_id()),
            );

            let identify = identify::Behaviour::new(
                identify::Config::new("/tari/0.0.1".to_string(), key.public())
                    .with_agent_version("tari/1.1.1".into()),
            );

            let dcutr = dcutr::Behaviour::new(key.public().to_peer_id());

            Ok(ChatBehaviour {
                ping: ping::Behaviour::new(ping::Config::new()),
                gossipsub,
                mdns,
                kad,
                identify,
                dcutr,
            })
        })?
        .with_swarm_config(|c| c.with_idle_connection_timeout(Duration::from_secs(60)))
        .build();

    // Create a Gossipsub topic
    let topic = gossipsub::IdentTopic::new("test-net");
    // subscribes to our topic
    swarm.behaviour_mut().gossipsub.subscribe(&topic)?;

    swarm.dial(relay_addr.clone()).unwrap();

    // Read full lines from stdin
    let mut stdin = io::BufReader::new(io::stdin()).lines();

    // Listen on all interfaces and whatever port the OS assigns
    swarm.listen_on("/ip4/0.0.0.0/udp/0/quic-v1".parse()?)?;
    swarm.listen_on("/ip4/0.0.0.0/tcp/0".parse()?)?;

    pdbg!("Local peer id: {:?}", swarm.local_peer_id());
    pdbg!("Enter messages via STDIN and they will be sent to connected peers using Gossipsub");

    // Kick it off
    loop {
        select! {
            Ok(Some(line)) = stdin.next_line() => {
                if let Err(e) = swarm
                    .behaviour_mut().gossipsub
                    .publish(topic.clone(), line.as_bytes()) {
                    pdbg!("Publish error: {e:?}");
                }
            }
            event = swarm.select_next_some() => match event {
                SwarmEvent::Behaviour(ChatBehaviourEvent::Mdns(mdns::Event::Discovered(list))) => {
                    for (peer_id, multiaddr) in & list {
                        swarm.behaviour_mut().kad.add_address(&peer_id, multiaddr.clone());

                        swarm.behaviour_mut().identify.push(Some(peer_id.clone()));
                        // let peer = wait_for_identify(&mut swarm, peer_id.clone()).await;

                        // pdbg!("mDNS discovered a new peer: {:?}", peer);
                        pdbg!("mDNS discovered a peer: {:?}", peer_id);

                        // if peer.protocols.contains(&StreamProtocol::new("/meshsub/1.1.0")) {
                            swarm.behaviour_mut().gossipsub.add_explicit_peer(&peer_id);
                        // } else {
                        //     pdbg!("Peer does not support meshsub, ignoring");
                        // }
                    }
                    swarm.dial(relay_addr.clone()
                        .with(Protocol::P2pCircuit)
                        .with(Protocol::P2p(list[0].0.clone()))).unwrap();
                },
                SwarmEvent::Behaviour(ChatBehaviourEvent::Mdns(mdns::Event::Expired(list))) => {
                    for (peer_id, _multiaddr) in list {
                        pdbg!("mDNS discover peer has expired: {peer_id}");
                        pdbg!("mDNS discover peer has expired: {peer_id}");
                        swarm.behaviour_mut().gossipsub.remove_explicit_peer(&peer_id);
                    }
                },
                SwarmEvent::Behaviour(ChatBehaviourEvent::Dcutr(dcutr::Event {result, remote_peer_id})) => {
                    match result {
                        Ok(conn_id) => {
                            pdbg!("Successfully connected to peer: {conn_id}");
                        },
                        Err(e) => {
                            pdbg!("Failed to connect to peer: {remote_peer_id} with error: {e:?}");
                        }
                    }
                },
                SwarmEvent::Behaviour(ChatBehaviourEvent::Gossipsub(gossipsub::Event::Message {
                    propagation_source: peer_id,
                    message_id: id,
                    message,
                })) => pdbg!(
                        "Got message: '{}' with id: {id} from peer: {peer_id}",
                        String::from_utf8_lossy(&message.data),
                    ),
                SwarmEvent::NewListenAddr { address, .. } => {
                    pdbg!("Local node is listening on {address}");
                },
                 SwarmEvent::Behaviour(ChatBehaviourEvent::Identify(identify::Event::Received {
                peer_id,
                info,
                    // identify::Info {
                    //     protocol_version,
                    //     agent_version,
                    //     listen_addrs,
                    //     protocols,
                    //     observed_addr,
                    //     ..
                    // },
            })) => {
                pdbg!("Got identify event: {:?}", info);
            },
                evt => {
                    pdbg!("Got event: {:?}", evt);
                }
            }
        }
    }
}
async fn wait_for_identify(swarm: &mut Swarm<ChatBehaviour>, peer: PeerId) -> Peer {
    loop {
        match swarm.next().await.expect("Infinite Stream.") {
            SwarmEvent::Behaviour(ChatBehaviourEvent::Identify(identify::Event::Received {
                peer_id,
                info:
                    identify::Info {
                        protocol_version,
                        agent_version,
                        listen_addrs,
                        protocols,
                        observed_addr,
                        ..
                    },
            })) => {
                if peer_id == peer {
                    return Peer {
                        peer_id,
                        protocol_version,
                        agent_version,
                        listen_addrs,
                        protocols,
                        observed_addr,
                    };
                }
            }
            e => {
                pdbg!("[wait_for_identify] event: {:?}", e);
                debug!("{e:?}")
            }
        }
    }
}

#[derive(Debug, Clone)]
struct Peer {
    peer_id: PeerId,
    protocol_version: String,
    agent_version: String,
    listen_addrs: Vec<Multiaddr>,
    protocols: Vec<StreamProtocol>,
    observed_addr: Multiaddr,
}
