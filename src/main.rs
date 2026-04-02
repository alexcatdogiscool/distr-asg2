use tokio::{
    io::{self, stdin, BufReader, AsyncBufReadExt, AsyncReadExt},
    select,
    fs::File
};

use libp2p::{
    Multiaddr,
    PeerId,
    StreamProtocol,
    identity::Keypair,
    kad::{Behaviour as KadBehaviour, Config as KadConfig, Event as KadEvent, Mode, QueryResult, store::MemoryStore},
    noise,
    request_response::{self, ProtocolSupport},
    swarm::{NetworkBehaviour, SwarmEvent},
    tcp,
    yamux,
    gossipsub
};

use clap::Parser;
use futures::StreamExt;
use ed25519_dalek::Signature;
use ed25519_dalek::SigningKey;
use rand::rngs::OsRng;
use std::{error::Error, fs,
    hash::{
        DefaultHasher,
        Hash,
        Hasher
    },
    time::Duration};
use uuid::Uuid;


#[derive(Parser)]
#[clap(name = "bulitin board!!! :)")]
struct Cli {
    #[arg(long)]
    ident_key: Option<String>,

    #[arg(long)]
    peer: Option<Multiaddr>,

    #[arg(long)]
    port: Option<String>,

}

#[derive(NetworkBehaviour)]
struct MyBehaviour {
    request_response: request_response::cbor::Behaviour<String, String>,
    kademlia: KadBehaviour<MemoryStore>,
    gossipsub: gossipsub::Behaviour,
}

struct PeerBoardMessage {
    peer_id: String,
    topic: String,
    content: String,
    timestamp: i64,
    message_id: String,
    nickname: String,
}


#[tokio::main]
async fn main() ->Result<(), Box<dyn Error>> {
    
    let cli = Cli::parse();

    let signing_key: SigningKey;
    if cli.ident_key.is_none() {
        println!("no key given, generating key now...");
        let mut csprng = OsRng;
        signing_key = SigningKey::generate(&mut csprng);
        fs::write("ident-keypair.key", signing_key.to_keypair_bytes())?;
        println!("key saved to 'ident-keypair.key'");
        
    }
    else {
        println!("loading keypair");
        let bytes = fs::read(cli.ident_key.unwrap()).expect("error reading file.");
        let keypair_bytes: [u8; 64] = bytes[..64].try_into().expect("file contins incorrect data");
        signing_key = SigningKey::from_keypair_bytes(&keypair_bytes)?;
        print!("finished loading");
    }

    // set up peer_id from key
    let secret_bytes = signing_key.to_bytes();
    let libp2p_keypair = Keypair::ed25519_from_bytes(secret_bytes)
        .expect("Given key could not be used to make peer_id");

    let local_peer_id = PeerId::from(libp2p_keypair.public());
    println!("local peer id: {}", local_peer_id.to_string());

    // set up the swarm!
    let mut swarm = libp2p::SwarmBuilder::with_existing_identity(libp2p_keypair)
        .with_tokio()
        .with_tcp(
            tcp::Config::default(),
            noise::Config::new,
            yamux::Config::default
        )?
        .with_behaviour(| key| {

            // set up kademlia
            let peer_id = key.public().to_peer_id();

            let store = MemoryStore::new(peer_id);
            let mut kad_config = KadConfig::new(StreamProtocol::new("/peerboard/kad/1.0.0"));
            kad_config.set_query_timeout(Duration::from_secs(30));
            let mut kademlia = KadBehaviour::with_config(peer_id, store, kad_config);

            // set up gossipsub
            // msg id auth
            let msg_id_fn = |msg: &gossipsub::Message| {
                // random uuid
                gossipsub::MessageId::from(Uuid::new_v4().to_string())
            };

            // cfg
            let gossipsub_config = gossipsub::ConfigBuilder::default()
                .heartbeat_interval(Duration::from_secs(10))
                .validation_mode(gossipsub::ValidationMode::None)// temporary, maybe
                .message_id_fn(msg_id_fn)
                .build()
                .expect("poop");

            let gossipsub = gossipsub::Behaviour::new(
                gossipsub::MessageAuthenticity::Signed(key.clone()),
                gossipsub_config,
            ).expect("poop2");

            MyBehaviour {
                request_response: request_response::cbor::Behaviour::new(
                    [(StreamProtocol::new("/bulletin/msg/v1"), ProtocolSupport::Full)],
                    request_response::Config::default(),
                ),
                kademlia,
                gossipsub,
            }
        })?
        .with_swarm_config(|cfg| cfg.with_idle_connection_timeout(Duration::from_secs(1000)))
        .build();

    // set up my nodes id stuffs v
    let listen_port: String = if cli.port.is_some() {
        cli.port.unwrap()
    } else {
        "0".to_string()
    };

    let multiaddr: Multiaddr = format!("/ip4/0.0.0.0/tcp/{listen_port}").parse()?;

    let _ = swarm.listen_on(multiaddr);

    // dial the bootdtrap node vvv
    // bootstrap multiaddr v
    if cli.peer.is_none() {
        // dail the hardcoded(v) bootstrap node
        let bootstrap_multiaddr: Multiaddr = format!("/ip4/170.64.177.57/tcp/8000/p2p/12D3KooWCvwqT3JUzVQczCvAVFa9EGzNqjHHSMVHVhm3RVyscCNY").parse()?;
        
        let bootstrap_peer_id = match bootstrap_multiaddr.iter().last() {
            Some(libp2p::multiaddr::Protocol::P2p(id)) => id,
            _ => return Err("bad bootstrap node!".into()),
        };

        swarm.behaviour_mut().kademlia.add_address(&bootstrap_peer_id, bootstrap_multiaddr.clone());
        swarm.dial(bootstrap_multiaddr)?;
        // self lookup!!! v
        swarm.behaviour_mut().kademlia.bootstrap()?;
        println!("bootstrapping from bootstrap id");
    }
    else {
        // dail the peer give by the cli
        let peer: Multiaddr = format!("{}", cli.peer.unwrap().clone()).parse()?;
        let peer_id = match peer.iter().last() {
            Some(libp2p::multiaddr::Protocol::P2p(id)) => id,
            _ => return Err("bad bootstrap node!".into()),
        };

        swarm.behaviour_mut().kademlia.add_address(&peer_id, peer.clone());
        swarm.dial(peer)?;
        swarm.behaviour_mut().kademlia.bootstrap()?;
        println!("dialed given peer");
    }

    // set up a topic v
    let topic_string = "peerboard/v1/general".to_string();
    let topic = gossipsub::IdentTopic::new(topic_string.clone());
    swarm.behaviour_mut().gossipsub.subscribe(&topic)?;





    let mut other_peer_id: Option<PeerId> = None;

    let mut stdin: io::Lines<BufReader<io::Stdin>> = io::BufReader::new(io::stdin()).lines();

    loop {
        select! {

            // simple msg match
            Ok(Some(line)) = stdin.next_line() => {
                // construct a message based on the input

                let msg = PeerBoardMessage {
                    peer_id: local_peer_id.to_string(),
                    topic: topic_string.clone(),
                    content: line,
                    timestamp: 0,
                    message_id: Uuid::new_v4().to_string(),
                    nickname: "alex".to_string()
                };

                
                swarm.behaviour_mut().gossipsub.publish(topic.clone(), line.as_bytes())?;
            }

            // swam match
            event = swarm.select_next_some() => match event {
                // listening on ...
                SwarmEvent::NewListenAddr { address, .. } => {
                    println!("Listening on addr: {address}");
                }
                // new connection made ...
                SwarmEvent::ConnectionEstablished { peer_id, .. } => {
                    other_peer_id = Some(peer_id);
                    println!("connection established with: {}", other_peer_id.unwrap().to_string());
                }
                // some kademlia stuff
                SwarmEvent::Behaviour(MyBehaviourEvent::Kademlia(e)) => {
                    println!("kademlia event {e:?}");
                }
                // gossipsub listen
                SwarmEvent::Behaviour(MyBehaviourEvent::Gossipsub(gossipsub::Event::Message {
                    propagation_source: peer_id,
                    message_id: id,
                    message,
                    
                    })) => {
                        println!("got a gossip msg: {:?} with id: {}, from peer: {}",
                            &message.data, id, peer_id)
                    }
                _ => println!("unhandled: {:?}", event),
            }

            
            
        }
    }



    Ok(())
}
