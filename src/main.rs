use async_std::io;
use futures::{future::Either, AsyncBufReadExt, SinkExt, StreamExt};
use libp2p::{
    core::{muxing::StreamMuxerBox, transport::OrTransport, upgrade},
    gossipsub, identity, mdns, noise,
    swarm::NetworkBehaviour,
    swarm::{SwarmBuilder, SwarmEvent},
    tcp, yamux, PeerId, Transport,
};
use libp2p_quic as quic;
use std::hash::{Hash, Hasher};
use std::time::Duration;
use std::{collections::hash_map::DefaultHasher, net::SocketAddr};
use std::{error::Error, thread};
use tokio::{
    net::TcpListener,
    sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender},
};
use tokio_tungstenite::tungstenite::Message;

#[derive(NetworkBehaviour)]
struct MyBehaviour {
    gossipsub: gossipsub::Behaviour,
    mdns: mdns::async_io::Behaviour,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let addr = get_addr();
    let listener = TcpListener::bind(&addr).await?;
    println!("WS Listening on: {}", addr);

    let id_keys = identity::Keypair::generate_ed25519();
    let local_peer_id = PeerId::from(id_keys.public());
    println!("Local peer id: {local_peer_id}");

    let tcp_transport = tcp::async_io::Transport::new(tcp::Config::default().nodelay(true))
        .upgrade(upgrade::Version::V1Lazy)
        .authenticate(noise::Config::new(&id_keys).expect("signing libp2p-noise static keypair"))
        .multiplex(yamux::Config::default())
        .timeout(std::time::Duration::from_secs(20))
        .boxed();
    let quic_transport = quic::async_std::Transport::new(quic::Config::new(&id_keys));
    let transport = OrTransport::new(quic_transport, tcp_transport)
        .map(|either_output, _| match either_output {
            Either::Left((peer_id, muxer)) => (peer_id, StreamMuxerBox::new(muxer)),
            Either::Right((peer_id, muxer)) => (peer_id, StreamMuxerBox::new(muxer)),
        })
        .boxed();

    let message_id_fn = |message: &gossipsub::Message| {
        let mut s = DefaultHasher::new();
        message.data.hash(&mut s);
        gossipsub::MessageId::from(s.finish().to_string())
    };

    let gossipsub_config = gossipsub::ConfigBuilder::default()
        .heartbeat_interval(Duration::from_secs(10))
        .validation_mode(gossipsub::ValidationMode::Strict)
        .message_id_fn(message_id_fn)
        .build()
        .expect("Valid config");

    let mut gossipsub = gossipsub::Behaviour::new(
        gossipsub::MessageAuthenticity::Signed(id_keys),
        gossipsub_config,
    )
    .expect("Correct configuration");
    let topic = gossipsub::IdentTopic::new("test-net");
    gossipsub.subscribe(&topic)?;

    let mut swarm = {
        let mdns = mdns::async_io::Behaviour::new(mdns::Config::default(), local_peer_id)?;
        let behaviour = MyBehaviour { gossipsub, mdns };
        SwarmBuilder::with_async_std_executor(transport, behaviour, local_peer_id).build()
    };

    let mut stdin = io::BufReader::new(io::stdin()).lines().fuse();

    swarm.listen_on("/ip4/0.0.0.0/udp/0/quic-v1".parse()?)?;
    swarm.listen_on("/ip4/0.0.0.0/tcp/0".parse()?)?;

    println!("Enter messages via STDIN and they will be sent to connected peers using Gossipsub");

    let (sender_dr, mut receiver_dr) = unbounded_channel::<String>();
    let (sender_rd, receiver_rd) = unbounded_channel::<String>();

    _ = thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async move {
            ws_to_gossipsub(listener, addr, sender_dr, receiver_rd).await;
        });
    });

    loop {
        tokio::select! {
            line = stdin.select_next_some() => {
                if let Err(e) = swarm
                    .behaviour_mut().gossipsub
                    .publish(topic.clone(), line.expect("Stdin not to close").as_bytes()) {
                    println!("Publish error: {e:?}");
                }
            },
            event = swarm.select_next_some() => match event {
                SwarmEvent::Behaviour(MyBehaviourEvent::Mdns(mdns::Event::Discovered(list))) => {
                    for (peer_id, _multiaddr) in list {
                        println!("mDNS discovered a new peer: {peer_id}");
                        swarm.behaviour_mut().gossipsub.add_explicit_peer(&peer_id);
                    }
                },
                SwarmEvent::Behaviour(MyBehaviourEvent::Mdns(mdns::Event::Expired(list))) => {
                    for (peer_id, _multiaddr) in list {
                        println!("mDNS discover peer has expired: {peer_id}");
                        swarm.behaviour_mut().gossipsub.remove_explicit_peer(&peer_id);
                    }
                },
                SwarmEvent::Behaviour(MyBehaviourEvent::Gossipsub(gossipsub::Event::Message {
                    propagation_source: peer_id,
                    message_id: id,
                    message,
                })) => {
                    let msg = String::from_utf8_lossy(&message.data);
                    println!(
                        "Got message: '{}' with id: {id} from peer: {peer_id}",
                        msg
                    );
                    sender_rd.send(msg.to_string()).expect("Failed to send msg");
                }
                SwarmEvent::NewListenAddr { address, .. } => {
                    println!("Local node is listening on {address}");
                }
                _ => {}
            },
            msg = receiver_dr.recv() => {
                if let Some(msg) = msg{
                    swarm.behaviour_mut().gossipsub.publish(topic.clone(), msg.as_bytes()).expect("Failed to publish msg");
                }
            }
        }
    }
}

async fn ws_to_gossipsub(
    listener: TcpListener,
    addr: String,
    sender_dr: UnboundedSender<String>,
    mut receiver_rd: UnboundedReceiver<String>,
) {
    let (stream, _) = listener.accept().await.unwrap();
    println!("Incoming TCP connection from: {}", addr);
    let stream = tokio_tungstenite::accept_async(stream)
        .await
        .expect("Error during the websocket handshake occurred");
    println!("WebSocket connection established: {}", addr);
    let (mut write, mut read) = stream.split();
    loop {
        tokio::select! {
            Some(msg) = read.next() => {
                let msg = msg.expect("Failed to get msg");
                println!("Received a message from {}: {}", addr, msg);
                write.send(msg.clone()).await.expect("Failed to send msg");
                sender_dr.send(msg.to_string()).expect("Failed to send msg");
            }
            Some(msg) = receiver_rd.recv() => {
                let msg = Message::Text(msg);
                write.send(msg.clone()).await.expect("Failed to send msg");
            }
        }
    }
}

pub fn get_addr() -> String {
    for port in 8080..=8090 {
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        if std::net::TcpListener::bind(addr).is_ok() {
            return addr.to_string();
        }
    }
    return SocketAddr::from(([127, 0, 0, 1], 8080)).to_string();
}
