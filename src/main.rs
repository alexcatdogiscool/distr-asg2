use tokio::{
    io::{self, stdin},
    select,
    fs::File
};

use libp2p::{
    noise,
    request_response::{self, ProtocolSupport},
    swarm::{NetworkBehaviour, SwarmEvent},
    tcp, yamux, Multiaddr, PeerId, StreamProtocol,
    kad::{store::MemoryStore, Behaviour, Config, Event, Mode, QueryResult},
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
struct ReqResBehaviour {
    request_response: request_response::cbor::Behaviour<String, String>,
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

    // set up the swarm!
    let mut swarm = libp2p::SwarmBuilder::with_new_identity()
        .with_tokio()
        .with_tcp(
            tcp::Config::default(),
            noise::Config::new,
            yamux::Config::default
        )?
        .with_behaviour(|_key| ReqResBehaviour{
            request_response: request_response::cbor::Behaviour::new(
                [(StreamProtocol::new("/test/v1"),
                ProtocolSupport::Full,
            )],
            request_response::Config::default(),
            )
        })?
        .with_swarm_config(|cfg| cfg.with_idle_connection_timeout(Duration::from_secs(1000)))
        .build();

    let listen_port = "0".to_string();
    let multiaddr = format!("/ip4/0.0.0.0/tcp/{listen_port}");

    swarm.listen_on(multiaddr.parse()?);

    if let Some(peer) = cli.peer {
        swarm.dial(peer);
    }

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
                _ => println!("unhandled: {:?}", event),
            }
            
        }
    }



    Ok(())
}
