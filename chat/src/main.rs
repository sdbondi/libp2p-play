mod peer;

use peer::Peer;

use clap::builder::styling::Style;
use clap::Parser;
use futures::stream::StreamExt;
use libp2p::{
    core::multiaddr::{Multiaddr, Protocol},
    identify, identity, noise, ping, relay,
    swarm::{NetworkBehaviour, SwarmEvent},
    tcp, yamux, PeerId,
};
use libp2p_dcutr as dcutr;
use libp2p_gossipsub as gossipsub;
use std::error::Error;
use std::str::FromStr;
use std::sync::atomic;
use std::sync::atomic::AtomicUsize;
use std::time::Duration;
use tokio::io;
use tokio::io::AsyncBufReadExt;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[clap(name = "libp2p DCUtR client")]
struct Opts {
    /// The mode (client-listen, client-dial).
    #[clap(long)]
    mode: Mode,

    /// Fixed value to generate deterministic peer id.
    #[clap(long)]
    secret_key_seed: u8,

    /// The listening address
    #[clap(long)]
    relay_address: Option<Multiaddr>,

    /// Peer ID of the remote peer to hole punch to.
    #[clap(long)]
    remote_peer_id: Option<PeerId>,
}

#[derive(Clone, Debug, PartialEq, Parser)]
enum Mode {
    Dial,
    Listen,
}

impl FromStr for Mode {
    type Err = String;
    fn from_str(mode: &str) -> Result<Self, Self::Err> {
        match mode {
            "dial" => Ok(Mode::Dial),
            "listen" => Ok(Mode::Listen),
            _ => Err("Expected either 'dial' or 'listen'".to_string()),
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .try_init();

    let opts = Opts::parse();

    let relay_addr = opts.relay_address.unwrap_or_else(|| {
        Multiaddr::from_str(
            "/ip4/178.128.46.8/tcp/4001/p2p/12D3KooWDpJ7As7BWAwRMfu1VU2WCqNjvq387JEYKDBj4kx6nXTN",
        )
        .unwrap()
    });
    #[derive(NetworkBehaviour)]
    struct ChatBehaviour {
        relay: relay::client::Behaviour,
        ping: ping::Behaviour,
        identify: identify::Behaviour,
        dcutr: dcutr::Behaviour,
        gossipsub: gossipsub::Behaviour,
    }
    let key = generate_ed25519(opts.secret_key_seed);

    println!("🔑 peer id: {peer_id}", peer_id = key.public().to_peer_id());

    let mut swarm = libp2p::SwarmBuilder::with_existing_identity(key)
        .with_tokio()
        .with_tcp(
            tcp::Config::default().port_reuse(true).nodelay(true),
            noise::Config::new,
            yamux::Config::default,
        )?
        .with_quic()
        // .with_dns()?
        .with_relay_client(noise::Config::new, yamux::Config::default)?
        .with_behaviour(|keypair, relay_behaviour| {
            let message_id_fn = {
                let id = AtomicUsize::new(0);
                move |_message: &gossipsub::Message| {
                    // let mut s = DefaultHasher::new();
                    // message.data.hash(&mut s);
                    let next = id.fetch_add(1, atomic::Ordering::Relaxed);
                    gossipsub::MessageId::from(next.to_be_bytes())
                }
            };

            // Set a custom gossipsub configuration
            let gossipsub_config = gossipsub::ConfigBuilder::default()
                .heartbeat_interval(Duration::from_secs(10)) // This is set to aid debugging by not cluttering the log space
                .validation_mode(gossipsub::ValidationMode::Strict) // This sets the kind of message validation. The default is Strict (enforce message signing)
                .message_id_fn(message_id_fn) // content-address messages. No two messages of the same content will be propagated.
                .build()
                .unwrap();
            // .map_err(|msg| io::Error::new(io::ErrorKind::Other, msg))?; // Temporary hack because `build` does not return a proper `std::error::Error`.

            // build a gossipsub network behaviour
            let gossipsub = gossipsub::Behaviour::new(
                gossipsub::MessageAuthenticity::Signed(keypair.clone()),
                gossipsub_config,
            )
            .unwrap();

            ChatBehaviour {
                relay: relay_behaviour,
                ping: ping::Behaviour::new(
                    ping::Config::new()
                        .with_timeout(Duration::from_secs(10))
                        .with_interval(Duration::from_secs(10)),
                ),
                identify: identify::Behaviour::new(identify::Config::new(
                    "/jank-chat/0.0.1".to_string(),
                    keypair.public(),
                )),
                dcutr: dcutr::Behaviour::new(keypair.public().to_peer_id()),
                gossipsub,
            }
        })?
        .build();

    swarm
        .listen_on("/ip4/0.0.0.0/udp/0/quic-v1".parse().unwrap())
        .unwrap();
    swarm
        .listen_on("/ip4/0.0.0.0/tcp/0".parse().unwrap())
        .unwrap();

    // Wait to listen on all interfaces.
    let delay = tokio::time::sleep(std::time::Duration::from_secs(1));
    tokio::pin!(delay);
    loop {
        tokio::select! {
            event = swarm.next() => {
                match event.unwrap() {
                    SwarmEvent::NewListenAddr { address, .. } => {
                        tracing::info!(%address, "Listening on address");
                    }
                    event => panic!("{event:?}"),
                }
            }
            _ = &mut delay => {
                // Likely listening on all interfaces now, thus continuing by breaking the loop.
                break;
            }
        }
    }

    // Connect to the relay server. Not for the reservation or relayed connection, but to (a) learn
    // our local public address and (b) enable a freshly started relay to learn its public address.
    // swarm.dial(relay_addr.clone()).unwrap();
    // let mut learned_observed_addr = false;
    // let mut told_relay_observed_addr = false;
    //
    // loop {
    //     match swarm.next().await.unwrap() {
    //         SwarmEvent::NewListenAddr { .. } => {}
    //         SwarmEvent::Dialing { .. } => {}
    //         SwarmEvent::ConnectionEstablished { .. } => {}
    //         SwarmEvent::Behaviour(ChatBehaviourEvent::Ping(_)) => {}
    //         SwarmEvent::Behaviour(ChatBehaviourEvent::Identify(identify::Event::Sent {
    //             ..
    //         })) => {
    //             tracing::info!("Told relay its public address");
    //             told_relay_observed_addr = true;
    //         }
    //         SwarmEvent::Behaviour(ChatBehaviourEvent::Identify(identify::Event::Received {
    //             info: identify::Info { observed_addr, .. },
    //             ..
    //         })) => {
    //             tracing::info!(address=%observed_addr, "Relay told us our observed address");
    //             learned_observed_addr = true;
    //         }
    //         event => panic!("{event:?}"),
    //     }
    //
    //     if learned_observed_addr && told_relay_observed_addr {
    //         break;
    //     }
    // }

    match opts.mode {
        Mode::Dial => {
            let addr = relay_addr
                .with(Protocol::P2pCircuit)
                .with(Protocol::P2p(opts.remote_peer_id.clone().unwrap()));
            tracing::info!("Dialing peer via relay");
            println!("📞 Dialing peer via relay address '{addr}'");
            swarm.dial(addr).unwrap();
        }
        Mode::Listen => {
            // Connect to the relay server. Not for the reservation or relayed connection, but to (a) learn
            // our local public address and (b) enable a freshly started relay to learn its public address.
            swarm.dial(relay_addr.clone()).unwrap();
            let mut learned_observed_addr = false;
            let mut told_relay_observed_addr = false;

            loop {
                match swarm.next().await.unwrap() {
                    SwarmEvent::NewListenAddr { .. } => {}
                    SwarmEvent::Dialing { .. } => {}
                    SwarmEvent::ConnectionEstablished { established_in, .. } => {
                        println!("🌟 Connection established to relay in {:?}", established_in);
                    }
                    SwarmEvent::Behaviour(ChatBehaviourEvent::Ping(_)) => {}
                    SwarmEvent::Behaviour(ChatBehaviourEvent::Identify(
                        identify::Event::Sent { .. },
                    )) => {
                        println!("🤝 We told relay its public address");
                        tracing::info!("Told relay its public address");
                        told_relay_observed_addr = true;
                    }
                    SwarmEvent::Behaviour(ChatBehaviourEvent::Identify(
                        identify::Event::Received {
                            info: identify::Info { observed_addr, .. },
                            ..
                        },
                    )) => {
                        tracing::info!(address=%observed_addr, "Relay told us our observed address");
                        println!("🤝 Relay told us our observed address '{observed_addr}'");
                        learned_observed_addr = true;
                    }
                    event => tracing::info!("{event:?}"),
                }

                if learned_observed_addr && told_relay_observed_addr {
                    break;
                }
            }

            swarm
                .listen_on(relay_addr.with(Protocol::P2pCircuit))
                .unwrap();
        }
    }

    tracing::info!(
        "connected = {}",
        swarm
            .connected_peers()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    );

    let mut stdin = io::BufReader::new(io::stdin()).lines();

    let topic = gossipsub::IdentTopic::new("chat");

    swarm.behaviour_mut().gossipsub.subscribe(&topic).unwrap();

    // Kick it off
    loop {
        let maybe_event = tokio::select! {
            Ok(Some(line)) = stdin.next_line() => {
                if let Err(e) = swarm
                    .behaviour_mut().gossipsub
                    .publish(topic.clone(), line.as_bytes()) {
                    tracing::error!("Publish error: {e:?}");
                }
                None
            }
            event = swarm.select_next_some() => Some(event),
        };

        let Some(event) = maybe_event else {
            continue;
        };

        tracing::info!(
            ?event,
            "connected = {}",
            swarm
                .connected_peers()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        );

        match event {
            SwarmEvent::NewListenAddr { address, .. } => {
                println!("👂️ Listening on relay address '{address}'");
                tracing::info!(%address, "Listening on address");
            }
            SwarmEvent::Behaviour(ChatBehaviourEvent::Relay(
                relay::client::Event::ReservationReqAccepted { .. },
            )) => {
                assert_eq!(opts.mode, Mode::Listen);
                println!("🤝 Relay accepted our reservation request");
                tracing::info!("Relay accepted our reservation request");
            }
            SwarmEvent::Behaviour(ChatBehaviourEvent::Relay(
                relay::client::Event::OutboundCircuitEstablished { relay_peer_id, .. },
            )) => {
                println!("🌟 Outbound circuit established for {relay_peer_id}");
            }
            SwarmEvent::Behaviour(ChatBehaviourEvent::Relay(
                relay::client::Event::InboundCircuitEstablished { src_peer_id, limit },
            )) => {
                println!("🌟 Inbound circuit established for {src_peer_id}: {limit:?}");
            }
            SwarmEvent::Behaviour(ChatBehaviourEvent::Dcutr(dcutr::Event {
                remote_peer_id,
                result,
            })) => match result {
                Ok(conn_id) => {
                    println!("✂️ DCUtR connection {conn_id} established {remote_peer_id}");
                }
                Err(e) => {
                    println!("✂️ DCUtR connection failed for {remote_peer_id}: {e}");
                }
            },
            SwarmEvent::Behaviour(ChatBehaviourEvent::Identify(identify::Event::Received {
                peer_id,
                info,
            })) => {
                let peer = Peer {
                    peer_id,
                    protocol_version: info.protocol_version,
                    agent_version: info.agent_version,
                    listen_addrs: info.listen_addrs,
                    protocols: info.protocols,
                    observed_addr: info.observed_addr,
                };
                println!("🤝 Peer ident: {peer}");
            }
            SwarmEvent::Behaviour(ChatBehaviourEvent::Ping(ping::Event {
                peer,
                connection,
                result,
            })) => match result {
                Ok(duration) => {
                    println!("🏓 Ping {peer} {connection} {duration:?}");
                }
                Err(e) => {
                    println!("🏓 Ping {peer} {connection} {e:?}");
                }
            },
            SwarmEvent::ConnectionEstablished {
                peer_id, endpoint, ..
            } => {
                tracing::info!("Connection established to {peer_id:?} ({endpoint:?})");
                println!("👨‍🔧 Connection established to {peer_id:?} ({endpoint:?})");
                swarm.behaviour_mut().gossipsub.add_explicit_peer(&peer_id);
            }
            SwarmEvent::OutgoingConnectionError { peer_id, error, .. } => {
                println!(
                    "☹️ Outgoing connection failed to {peer}: {error}",
                    peer = peer_id
                        .map(|p| p.to_string())
                        .unwrap_or_else(|| "--".to_string())
                );
                // tracing::info!(peer=?peer_id, "Outgoing connection failed: {error}");
            }

            SwarmEvent::Behaviour(ChatBehaviourEvent::Gossipsub(gossipsub::Event::Message {
                propagation_source: peer_id,
                message_id: id,
                message,
            })) => {
                println!(
                    "✉️ {}{peer_id}{}: [{id} {}] '{}'",
                    Style::new().bold().render(),
                    Style::new().render_reset(),
                    message.topic,
                    String::from_utf8_lossy(&message.data),
                )
            }
            SwarmEvent::ConnectionClosed {
                peer_id,
                endpoint,
                connection_id,
                num_established,
                cause,
            } => {
                println!(
                    "👋 Connection to {peer_id:?} closed  (conn_id={connection_id:?}, {cause:?}"
                );
                tracing::info!("Connection to {peer_id:?} closed: {endpoint:?} (conn_id={connection_id:?}, num_established={num_established:?}) {cause:?}");
            }
            evt => {
                tracing::info!("Got event: {:?}", evt);
            }
        }
    }
}

fn generate_ed25519(secret_key_seed: u8) -> identity::Keypair {
    let mut bytes = [0u8; 32];
    bytes[0] = secret_key_seed;

    identity::Keypair::ed25519_from_bytes(bytes).expect("only errors on wrong length")
}
