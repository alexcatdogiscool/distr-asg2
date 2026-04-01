use tokio::{
    io::{self, stdin},
    select,
    fs::File
};

use libp2p::{
    Multiaddr, PeerId, StreamProtocol, identity::Keypair, kad::{Behaviour as KadBehaviour, Config as KadConfig, Event as KadEvent, Mode, QueryResult, store::MemoryStore}, noise, request_response::{self, ProtocolSupport}, swarm::{NetworkBehaviour, SwarmEvent}, tcp, yamux
};

use clap::Parser;
use futures::StreamExt;
use ed25519_dalek::Signature;
use ed25519_dalek::SigningKey;
use rand::rngs::OsRng;
use std::{error::Error, fs, time::Duration};


#[derive(Parser)]
#[clap(name = "bulitin board!!! :)")]
struct Cli {
    #[arg(long)]
    ident_key: Option<String>,

    #[arg(long)]
    peer: Option<Multiaddr>,

}

#[derive(NetworkBehaviour)]
struct MyBehaviour {
    request_response: request_response::cbor::Behaviour<String, String>,
    kademlia: KadBehaviour<MemoryStore>,
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
    let mut swarm = libp2p::SwarmBuilder::with_new_identity()
        .with_tokio()
        .with_tcp(
            tcp::Config::default(),
            noise::Config::new,
            yamux::Config::default
        )?
        .with_behaviour(| key| {
            let peer_id = key.public().to_peer_id();

            let store = MemoryStore::new(peer_id);
            let mut kad_config = KadConfig::new(StreamProtocol::new("/peerboard/kad/1.0.0"));
            kad_config.set_query_timeout(Duration::from_secs(30));
            let mut kademlia = KadBehaviour::with_config(peer_id, store, kad_config);

            MyBehaviour {
                request_response: request_response::cbor::Behaviour::new(
                    [(StreamProtocol::new("/bulletin/msg/v1"), ProtocolSupport::Full)],
                    request_response::Config::default(),
                ),
                kademlia,
            }
        })?
        .with_swarm_config(|cfg| cfg.with_idle_connection_timeout(Duration::from_secs(1000)))
        .build();

    let listen_port = "0".to_string();
    let multiaddr: Multiaddr = format!("/ip4/0.0.0.0/tcp/{listen_port}").parse()?;

    let _ = swarm.listen_on(multiaddr);

    // dial the bootdtrap node vvv
    // bootstrap multiaddr v
    let bootstrap_multiaddr: Multiaddr = format!("/ip4/170.64.177.57/tcp/8000/p2p/12D3KooWCvwqT3JUzVQczCvAVFa9EGzNqjHHSMVHVhm3RVyscCNY").parse()?;
    
    let bootstrap_peer_id = match bootstrap_multiaddr.iter().last() {
        Some(libp2p::multiaddr::Protocol::P2p(id)) => id,
        _ => return Err("bad bootstrap node!".into()),
    };

    swarm.behaviour_mut().kademlia.add_address(&bootstrap_peer_id, bootstrap_multiaddr.clone());
    swarm.dial(bootstrap_multiaddr)?;
    swarm.behaviour_mut().kademlia.bootstrap()?;
    println!("bootstrapping from bootstrap id");


    let mut other_peer_id: Option<PeerId> = None;

    loop {
        select! {


            event = swarm.select_next_some() => match event {
                SwarmEvent::NewListenAddr { address, .. } => {
                    println!("Listening on addr: {address}");
                }
                SwarmEvent::ConnectionEstablished { peer_id, .. } => {
                    other_peer_id = Some(peer_id);
                    println!("connection established with: {}", other_peer_id.unwrap().to_string());
                }
                SwarmEvent::Behaviour(MyBehaviourEvent::Kademlia(e)) => {
                    println!("kademlia event {e:?}");
                }
                _ => println!("unhandled: {:?}", event),
            }
            
        }
    }



    Ok(())
}
