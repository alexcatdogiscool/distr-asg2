use tokio::{
    io::{self, stdin, BufReader, AsyncBufReadExt, AsyncReadExt},
    select,
    fs::File,
};

use libp2p::{
    Multiaddr,
    PeerId,
    StreamProtocol,
    gossipsub::{self, PublishError},
    identity::Keypair,
    kad::{Behaviour as KadBehaviour, Config as KadConfig, Event as KadEvent, Mode, QueryResult, store::MemoryStore},
    mdns, noise,
    request_response::{self, Behaviour, ProtocolSupport, cbor::codec::Codec},
    swarm::{self, NetworkBehaviour, SwarmEvent},
    tcp, yamux,
    identify
};

use clap::Parser;
use futures::{StreamExt, task::Poll};
use ed25519_dalek::Signature;
use ed25519_dalek::SigningKey;
use rand::{Rng, rngs::OsRng};
use std::{collections::HashMap, error::Error, fs, hash::{
        DefaultHasher,
        Hash,
        Hasher
    }, io::stdout, net::Incoming, num::NonZero, process::exit, time::Duration, u8::MIN};
use uuid::Uuid;
use prost::Message;

use bulletin::PeerBoardMessage;
use BattleShip::*;

use rusqlite::{Connection, Result as SqResult};
use crossterm::{cursor, event::{self, Event, KeyCode, KeyEvent, KeyEventKind}, terminal::{disable_raw_mode, enable_raw_mode}};
use ratatui::{
    DefaultTerminal,
    Frame, Terminal,
    buffer::{Buffer, Cell},
    layout::{Constraint, Direction, Layout, Rect},
    prelude::CrosstermBackend,
    style::Stylize,
    symbols::border,
    text::{Line, Text},
    widgets::{Block, Borders, List, ListDirection, ListItem, ListState, Paragraph, Widget},
    style::{Style, Color, Modifier},
};
use local_ip_address::local_ip;


static BOOTSTRAP_PEER_ID: &str = "12D3KooWCvwqT3JUzVQczCvAVFa9EGzNqjHHSMVHVhm3RVyscCNY";

#[derive(Parser)]
#[clap(name = "bulletin board!!!")]
struct Cli {
    #[arg(long)]
    ident_key: Option<String>,

    #[arg(long)]
    peer: Option<Multiaddr>,

    #[arg(long)]
    port: Option<String>,

    #[arg(long)]
    sqldb: String,

    #[arg(short, long, action = clap::ArgAction::SetTrue)]
    debug: bool,// if true, dont do TUI stuff, else, do TUI stuff

    #[arg(long)]
    public_ip: Option<String>,

}

#[derive(NetworkBehaviour)]
struct MyBehaviour {
    challenge: request_response::cbor::Behaviour<Vec<u8>, Vec<u8>>,
    battleship: request_response::cbor::Behaviour<Vec<u8>, Vec<u8>>,
    rendezvous: libp2p::rendezvous::client::Behaviour,
    kademlia: KadBehaviour<MemoryStore>,
    gossipsub: gossipsub::Behaviour,
    mdns: mdns::tokio::Behaviour,
}





pub mod bulletin {
    include!(concat!(env!("OUT_DIR"), "/peerboard.v1.rs"));
}

pub mod BattleShip {
    include!(concat!(env!("OUT_DIR"), "/peerboard.challenge.v1.rs"));
}





fn check_msg(msg: &PeerBoardMessage) -> bool {

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;

    if (msg.content.as_bytes().len() > 4096) {
        println!("1");
        return false;
    }
    if (!msg.topic.starts_with("peerboard/v1/")) {
        println!("2");
        return false;
    }
    if (msg.timestamp - now > 300) {
        println!("3");
        return false;
    }
    if (msg.nickname.as_bytes().len() > 32) {
        println!("4");
        return false;
    }
    return true;
}

struct DisplayMessage {
    nickname: String,
    peer_id: String,
    content: String,
    timestamp: i64,
}


enum GameState {
    MESSAGE,
    RENDEZVOUS(RendezvousState),
    BATTLE(BattleState),
    ACCEPT(AcceptDecline),
    BUILD(BuildState),
}

struct AcceptDecline {
    selected: bool,
    peer: Option<SimplePeer>,
    rend_state: RendezvousState,
    channel: Option<request_response::ResponseChannel<Vec<u8>>>,
}

struct BuildState {
    opponent_peer_id: PeerId,
    opponent_nickname: String,
    my_board: [[bool; 10];10],
    current_ship: Ship,
    cursor: [i32;2],
    is_placing: bool,
    placing_cursor: QuadDirection,
    board_ready: bool,
    am_challenger: bool
}

impl BuildState {
    fn place(&mut self) {
        // we know that placing is possible because we checked before running this (hopefully)
        match self.placing_cursor {
            QuadDirection::UP => {
                for r in (self.cursor[1] - (self.current_ship.length() as i32 - 1))..=self.cursor[1] {
                    self.my_board[r as usize][self.cursor[0] as usize] = true;
                }
            }
            QuadDirection::RIGHT => {
                for c in self.cursor[0]..self.cursor[0] + self.current_ship.length() as i32 {
                    self.my_board[self.cursor[1] as usize][c as usize] = true;
                }
            }
            QuadDirection::DOWN => {
                for r in self.cursor[1]..self.cursor[1] + self.current_ship.length() as i32 {
                    self.my_board[r as usize][self.cursor[0] as usize] = true;
                }
            }
            QuadDirection::LEFT => {
                for c in (self.cursor[0] - (self.current_ship.length() as i32 - 1))..=self.cursor[0] {
                    self.my_board[self.cursor[1] as usize][c as usize] = true;
                }
            }
        }
        self.current_ship = self.current_ship.next();
    }
}

enum QuadDirection {
    UP,
    LEFT,
    DOWN,
    RIGHT,
}

#[derive(PartialEq)]
enum Ship {
    CARRIER,
    BATTLESHIP,
    CRUISER,
    SUBMARINE,
    DESTROYER,
    None
}

impl Ship {
    fn length(&self) -> u8 {
        match self {
            Ship::CARRIER => 5,
            Ship::BATTLESHIP => 4,
            Ship::CRUISER => 3,
            Ship::SUBMARINE => 3,
            Ship::DESTROYER => 2,
            Ship::None => 0,
        }
    }

    fn next(&self) -> Ship {
        match self {
            Ship::CARRIER => Ship::BATTLESHIP,
            Ship::BATTLESHIP => Ship::CRUISER,
            Ship::CRUISER => Ship::SUBMARINE,
            Ship::SUBMARINE => Ship::DESTROYER,
            Ship::DESTROYER => Ship::None, // all ships placed, send BoardReady 
            Ship::None => Ship::None
        }
    }
}


struct BattleState {
    opponent_peer_id: PeerId,
    opponent_nickname: String,
    my_board: [[bool; 10];10],
    their_board: [[bool; 10];10],
    my_shots: [[bool;10];10],
    their_shots: [[bool;10];10],
    my_turn: bool,
    shot_seq: u32,
    phase: BattlePhase,
    cursor: [i32;2],
    am_challenger: bool,
    pening_shot: Option<[usize;2]>,
}

#[derive(Clone)]
struct RendezvousState {
    seeking_peers: Vec<PeerId>,
    refresh: bool,
    selected: Option<usize>,
}

struct SimplePeer {
    peer_id: PeerId,
    nickname: String,
}

#[derive(PartialEq)]
enum BattlePhase {
    WaitingForBoardAck,
    WaitingForOpponent,
    MyTurn,
    GameOver { i_won: bool },
}

struct AppState {
    current_topic: String,
    subscribed_topics: Vec<String>,
    msg_buffer: String,
    topic_buffer: String,
    messages: Vec<DisplayMessage>, // oldest first
    recent_topics: Vec<String>,
    selected_area: u8, // 0 is the left area, 1 is the right area
    peer_counts: std::collections::HashMap<String, usize>,
    game_state: GameState,
    exit: bool,
    total_connections: usize,
    self_lookup_done: bool,
    my_peer_id: PeerId,

}

fn load_messages(conn: &Connection, topic: &str) -> Vec<DisplayMessage> {
    let mut stmt = conn.prepare(
        "SELECT nickname, peer_id, content, timestamp
        FROM msgs WHERE topic = ?1
        ORDER BY TIMESTAMP ASC"
    ).unwrap();

    stmt.query_map([topic], |row| {
        Ok(DisplayMessage {
            nickname: row.get(0)?,
            peer_id: row.get(1)?,
            content: row.get(2)?,
            timestamp: row.get(3)?,
        })
    })
    .unwrap()
    .filter_map(|r| r.ok())
    .collect()
}

async fn run_tui(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    swarm: &mut libp2p::Swarm<MyBehaviour>,
    state: &mut AppState,
    conn: &Connection
) -> Result<(), Box<dyn Error>> {
    // start
    
    // load all messages from the db
    state.messages = load_messages(conn, &state.current_topic);

    let mut draw_interval = tokio::time::interval(Duration::from_millis(50));

    let mut bootstrap_int = tokio::time::interval(Duration::from_secs(60));


    while !state.exit {

        
        tokio::select! {

            _ = bootstrap_int.tick() => {
                // make peer discovery faster
                swarm.behaviour_mut().kademlia.bootstrap()?;
                swarm.behaviour_mut().kademlia.get_closest_peers(state.my_peer_id);
            }

            _ = draw_interval.tick() => {
                // draw the tui
                terminal.draw(|frame| {

                    match &mut state.game_state {
                        GameState::MESSAGE => {
                            draw_ui(frame, state);
                        }
                        GameState::RENDEZVOUS(rend) => {
                            draw_ren(frame, state);
                        }
                        GameState::BATTLE(battle) => {
                            draw_battle(frame, state);
                        }
                        GameState::ACCEPT(acpt) => {
                            draw_proposition(frame, state);
                        }
                        GameState::BUILD(build) => {
                            draw_build(frame, state);
                        }
                    };
                    
                })?;

                while event::poll(Duration::from_millis(0))? {
                    if let Event::Key(key) = event::read()? {
                        // handle all the key presses

                        match &mut state.game_state {
                            // messaging
                            GameState::MESSAGE => {
                                match key.code {
                                    KeyCode::Enter => {
                                        if state.selected_area == 1 {
                                            // send the message!
                                            let now = std::time::SystemTime::now()
                                                .duration_since(std::time::UNIX_EPOCH)
                                                .unwrap()
                                                .as_secs() as i64;

                                            let msg = PeerBoardMessage {
                                                peer_id: swarm.local_peer_id().to_string(),
                                                topic: state.current_topic.clone(),
                                                content: state.msg_buffer.clone(),
                                                timestamp: now,
                                                message_id: Uuid::new_v4().to_string(),
                                                nickname: "alex".to_string(),
                                            };

                                            // check the message for validity
                                            if (check_msg(&msg)) {
                                                let mut buf = Vec::new();
                                                msg.encode(&mut buf).unwrap();
                                                // construct the topic
                                                let topic = gossipsub::IdentTopic::new(&state.current_topic);
                                                // send it out!
                                                match swarm.behaviour_mut().gossipsub.publish(topic, buf) {
                                                    Ok(_) => {},
                                                    Err(gossipsub::PublishError::NoPeersSubscribedToTopic) => {},// dont care!
                                                    Err(e) => return Err(e.into()),
                                                }

                                                // add it to the db
                                                conn.execute(
                                                    "INSERT INTO msgs (peer_id, topic, content, timestamp, message_id, nickname)
                                                    VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                                                (msg.peer_id, msg.topic, msg.content, msg.timestamp, msg.message_id, msg.nickname),
                                                ).expect("couldnt add msg to the db");
                                                // clear the input buffer
                                                state.msg_buffer.clear();
                                                // update the states msgs'
                                                state.messages = load_messages(conn, &state.current_topic);
                                            }
                                        }
                                        else {
                                            // sub to the new topic.
                                            // stay subbed to the previous topic.
                                            // this is a design choice, so you can have 2 coversations in different topics at the same time.
                                            // unsubbing only happens when the app closes (neccasarily)

                                            // use can type "/rendezvous" to go to the rendezvous area

                                            if state.topic_buffer.eq("/rendezvous") {
                                                state.game_state = GameState::RENDEZVOUS(RendezvousState { seeking_peers: vec![], refresh: false, selected: None });
                                                // register to the rendezvous node
                                                swarm.behaviour_mut().rendezvous.register(
                                                    libp2p::rendezvous::Namespace::new("peerboard/challenge/seeking".to_string()).unwrap(),
                                                    BOOTSTRAP_PEER_ID.parse().unwrap(),
                                                    None
                                                )?;
                                                // get initial list of seeking peers
                                                swarm.behaviour_mut().rendezvous.discover(
                                                    Some(libp2p::rendezvous::Namespace::new("peerboard/challenge/seeking".to_string()).unwrap()),
                                                    None,
                                                    None,
                                                    BOOTSTRAP_PEER_ID.parse().unwrap()
                                                );

                                            }
                                            else {
                                                let topic_str = format!("peerboard/v1/{}", state.topic_buffer.clone());
                                                let new_topic = gossipsub::IdentTopic::new(topic_str.clone());
                                                swarm.behaviour_mut().gossipsub.subscribe(&new_topic)?;
                                                if (!(state.recent_topics.contains(&topic_str))) {
                                                    state.recent_topics.push(topic_str.clone());
                                                }
                                                state.current_topic = topic_str.clone();
                                                

                                                // update the messages buffer too
                                                state.messages = load_messages(conn, &state.current_topic);
                                            }
                                            // so many evil nested if's >:(
                                            state.topic_buffer.clear();// clear buffer in all cases
                                            

                                        }
                                    },

                                    KeyCode::Char(c) => {// add it to buffer
                                        if state.selected_area == 1 {
                                            state.msg_buffer.push(c);
                                        } else {
                                            state.topic_buffer.push(c);
                                        }
                                        
                                    },

                                    KeyCode::Backspace => {//remove latest from buffer
                                        if state.selected_area == 1 {
                                            state.msg_buffer.pop();
                                        } else {
                                        state.topic_buffer.pop();
                                        }
                                    },

                                    KeyCode::Up => {// change selected area
                                        state.selected_area = 0;
                                    }

                                    KeyCode::Down => {// change selected area
                                        state.selected_area = 1;
                                    }

                                    

                                    KeyCode::Esc => {
                                        safe_exit(swarm, state);
                                        break;
                                    },// close da app!

                                    _ => {}
                                }
                                
                            }
                            // battle
                            GameState::BATTLE(battle) => {
                                match key.code {
                                    KeyCode::Up => {
                                        battle.cursor[1] = 0.max(battle.cursor[1] - 1);
                                    }
                                    KeyCode::Down => {
                                        battle.cursor[1] = 9.min(battle.cursor[1] + 1);
                                    }
                                    KeyCode::Left => {
                                        battle.cursor[0] = 0.max(battle.cursor[0] - 1);
                                    }
                                    KeyCode::Right => {
                                        battle.cursor[0] = 9.min(battle.cursor[0] + 1);
                                    }

                                    
                                    KeyCode::Enter => {
                                        if battle.phase == BattlePhase::MyTurn {//fire a shot
                                            battle.pening_shot = Some([battle.cursor[1] as usize, battle.cursor[0] as usize]);
                                            //battle.my_shots[battle.cursor[1] as usize][battle.cursor[0] as usize] = true;
                                            battle.phase = BattlePhase::WaitingForOpponent;

                                            // send the req and wait for response
                                            let request = BattleshipRequest{ msg: Some(battleship_request::Msg::Shot(Shot {
                                                seq: battle.shot_seq,
                                                col: battle.cursor[0] as u32,
                                                row: battle.cursor[1] as u32
                                            })) };
                                            let mut buf = vec![];
                                            request.encode(&mut buf);
                                            swarm.behaviour_mut().battleship.send_request(&battle.opponent_peer_id, buf);

                                        }// ese do nothing
                                    }

                                    KeyCode::Esc => {
                                        safe_exit(swarm, state);
                                        break;
                                    }

                                    _ => {}
                                }
                            }

                            GameState::RENDEZVOUS(rend) => {
                                if rend.refresh {
                                    // refresh the list
                                    swarm.behaviour_mut().rendezvous.discover(
                                        Some(libp2p::rendezvous::Namespace::new("peerboard/challenge/seeking".to_string()).unwrap()),
                                        None,
                                        None,
                                        BOOTSTRAP_PEER_ID.parse().unwrap()
                                    );
                                    rend.refresh = false;
                                }
                                match key.code {
                                    KeyCode::Char('r') => {
                                        rend.refresh = true;
                                    },

                                    KeyCode::Down => {
                                        rend.selected  = Some((rend.seeking_peers.len()-1).min(rend.selected.unwrap_or(0) + 1));
                                    }
                                    KeyCode::Up => {
                                        if rend.selected.is_some() {
                                            if rend.selected.unwrap() != 0 {
                                                rend.selected = Some(rend.selected.unwrap() - 1);
                                            }
                                        }
                                    }
                                    KeyCode::Enter => {
                                        if rend.selected.is_some() {
                                            // send a battle request to the selected peer
                                            let peer_id = rend.seeking_peers[rend.selected.unwrap()];

                                            let propose = ChallengePropose{ nickname: "alex".to_string() };
                                            let mut buf = vec![];
                                            propose.encode(&mut buf).unwrap();
                                            swarm.behaviour_mut().challenge.send_request(&peer_id, buf);

                                        }
                                    }
                                    

                                    KeyCode::Esc => {
                                        state.game_state = GameState::MESSAGE;
                                    },// close da app!

                                    _ => {}
                                }
                            }

                            GameState::ACCEPT(acpt) => {
                                match key.code {
                                    KeyCode::Right => acpt.selected = false,
                                    KeyCode::Left => acpt.selected = true,

                                    KeyCode::Enter => {
                                        match acpt.selected {// silly to use a match here?? idk/
                                            true => {
                                                // accept. go to battle!
                                                let respone = ChallengeResponse { accepted: true };
                                                let mut buf = vec![];
                                                respone.encode(&mut buf).unwrap();
                                                swarm.behaviour_mut().challenge.send_response(acpt.channel.take().unwrap(), buf).unwrap();// ewwww .take()...
                                                
                                                state.game_state = GameState::BUILD(BuildState {
                                                    opponent_peer_id: acpt.peer.as_ref().unwrap().peer_id,
                                                    opponent_nickname: acpt.peer.as_ref().unwrap().nickname.clone(),// hell
                                                    my_board: [[false;10];10],
                                                    current_ship: Ship::CARRIER,
                                                    cursor: [0;2],
                                                    is_placing: false,
                                                    placing_cursor: QuadDirection::UP,
                                                    board_ready: false,
                                                    am_challenger: false,// i am accepting, i am not the challenger
                                                });
                                            }
                                            false => {
                                                // ignore, go back to rendezvous
                                                let respone = ChallengeResponse { accepted: false };
                                                let mut buf = vec![];
                                                respone.encode(&mut buf).unwrap();
                                                swarm.behaviour_mut().challenge.send_response(acpt.channel.take().unwrap(), buf).unwrap();// ewwww .take()...
                                                state.game_state = GameState::RENDEZVOUS(acpt.rend_state.clone());// yuck
                                            }
                                        }
                                    }

                                    _ => {}
                                    
                                }
                            }

                            GameState::BUILD(build) => {

                                match build.is_placing {
                                    false => {//moving cursor
                                        match key.code {

                                            // cursor movement
                                            KeyCode::Up => {
                                                build.cursor[1] = 0.max(build.cursor[1] - 1);
                                            }
                                            KeyCode::Down => {
                                                build.cursor[1] = 9.min(build.cursor[1] + 1);
                                            }
                                            KeyCode::Left => {
                                                build.cursor[0] = 0.max(build.cursor[0] - 1);
                                            }
                                            KeyCode::Right => {
                                                build.cursor[0] = 9.min(build.cursor[0] + 1);
                                            }

                                            KeyCode::Enter => {
                                                build.is_placing = true;
                                            }
                                            
                                            
                                            

                                            KeyCode::Esc => {
                                                safe_exit(swarm, state);
                                                break;
                                            }
                                            _ => {},
                                        }
                                    }
                                    true => {// placing a peice
                                        match key.code {

                                            KeyCode::Up => build.placing_cursor = QuadDirection::UP,
                                            KeyCode::Down => build.placing_cursor = QuadDirection::DOWN,
                                            KeyCode::Left => build.placing_cursor = QuadDirection::LEFT,
                                            KeyCode::Right => build.placing_cursor = QuadDirection::RIGHT,

                                            KeyCode::Backspace => {// dont place, go back
                                                build.is_placing = false;
                                            }

                                            KeyCode::Enter => {// place the peice here!
                                                
                                                if is_place_possible(&build) {
                                                    //place the jit
                                                    build.place();
                                                    build.is_placing = false;// in any case
                                                }// else do nothing

                                                // check if we just palced the last peice
                                                // if so, go to the next state
                                                if build.current_ship == Ship::None {
                                                    // set board_done to true
                                                    // when we recevie a BoardReady, send BoardAck
                                                    build.board_ready = true;
                                                }
                                                
                                                

                                            }
                                            



                                            KeyCode::Esc => {
                                                safe_exit(swarm, state);
                                                
                                                break;
                                            }

                                            _ => {}
                                        }

                                    }
                                }

                                if build.board_ready {
                                    if build.am_challenger {
                                        // request BoardReady
                                        let request = BattleshipRequest { msg: Some(battleship_request::Msg::BoardReady(BoardReady {})) };// ???
                                        let mut buf = vec![];
                                        request.encode(&mut buf);
                                        swarm.behaviour_mut().battleship.send_request(&build.opponent_peer_id, buf);

                                        // move into battle state
                                        state.game_state = GameState::BATTLE(BattleState {
                                            opponent_peer_id: build.opponent_peer_id,
                                            opponent_nickname: build.opponent_nickname.clone(),
                                            my_board: build.my_board,
                                            their_board: [[false;10];10],
                                            my_shots: [[false;10];10],
                                            their_shots: [[false;10];10],
                                            my_turn: true,
                                            shot_seq: 0,
                                            phase: BattlePhase::MyTurn,
                                            cursor: build.cursor,
                                            am_challenger: true,
                                            pening_shot: None,
                                        });
                                    }// else wait for BoardReady request
                                }

                                
                            }


                            _ => {},
                        }

                        // batleship


                        // messaging
                        
                    
                    }
                }
            }

            // all swarm stuff
            event = swarm.select_next_some() => {
                match event {

                    

                    SwarmEvent::Behaviour(MyBehaviourEvent::Kademlia(libp2p::kad::Event::OutboundQueryProgressed { result, .. })) => {
                        match result {
                            libp2p::kad::QueryResult::GetClosestPeers(Ok(ok)) => {
                                state.messages.push(DisplayMessage {
                                    nickname: "KAD".to_string(),
                                    peer_id: "local".to_string(),
                                    content: format!("got closestPeer OK"),
                                    timestamp: 0,
                                });
                                for peer in ok.peers {
                                    state.messages.push(DisplayMessage {
                                        nickname: "KAD".to_string(),
                                        peer_id: "local".to_string(),
                                        content: format!("got here!"),
                                        timestamp: 0,
                                    });
                                    for addr in peer.addrs {
                                        swarm.behaviour_mut().kademlia.add_address(&peer.peer_id, addr.clone());
                                        // DEBUG
                                        state.messages.push(DisplayMessage {
                                            nickname: "KAD".to_string(),
                                            peer_id: "local".to_string(),
                                            content: format!("added peer_id {}, and addr: {}", peer.peer_id, addr),
                                            timestamp: 0,
                                        });
                                    }
                                }
                            }

                            _ => {}
                        }
                    }

                    
                    

                    SwarmEvent::NewExternalAddrOfPeer { peer_id, address, .. } => {
                        state.messages.push(DisplayMessage {
                            nickname: "SWARM".to_string(),
                            peer_id: "local".to_string(),
                            content: format!("new peer: peer_id {}, and addr: {}", peer_id, address),
                            timestamp: 0,
                        });
                    }
                    
                    // gossipsub listen
                    SwarmEvent::Behaviour(MyBehaviourEvent::Gossipsub(gossipsub::Event::Message {
                        message,
                        ..
                        })) => {
                            match PeerBoardMessage::decode(message.data.as_slice()) {
                                Ok(msg) => {
                                    // check the message for validity
                                    if (check_msg(&msg)) {
                                        // chgeck if it exists within the db v

                                        let msg_exists: bool = conn
                                            .query_row(
                                                "SELECT COUNT(*) FROM msgs WHERE message_id = ?1",
                                                [&msg.message_id],
                                                |row| row.get::<_, i64>(0),
                                            )
                                            .unwrap_or(0) > 0;

                                        if !msg_exists {
                                            // valid good non duplicate message
                                            // add it to the db
                                            conn.execute(
                                                "INSERT INTO msgs (peer_id, topic, content, timestamp, message_id, nickname)
                                                VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                                            (msg.peer_id, msg.topic, msg.content, msg.timestamp, msg.message_id, msg.nickname),
                                            ).expect("couldnt add msg to the db");
                                            // add it toi the states msgs'
                                            state.messages = load_messages(conn, &state.current_topic);
                                        }
                                    }
                                },                            
                                Err(_e) => {},
                            }
                        },

                    SwarmEvent::Behaviour(MyBehaviourEvent::Gossipsub(gossipsub::Event::Subscribed { peer_id, topic })) => {
                        *state.peer_counts.entry(topic.to_string()).or_insert(0) += 1;
                    }

                    SwarmEvent::Behaviour(MyBehaviourEvent::Gossipsub(gossipsub::Event::Unsubscribed { peer_id, topic })) => {
                        *state.peer_counts.entry(topic.to_string()).or_insert(1) -= 1;
                    }

                        //battleship stuff!
                    // someone challenged me
                    SwarmEvent::Behaviour(MyBehaviourEvent::Challenge(
                        request_response::Event::Message {
                            peer, message: request_response::Message::Request { request, channel, .. } ,
                            ..
                        }
                    )) => {

                        if let GameState::RENDEZVOUS(rend) = &mut state.game_state {
                            let propose = ChallengePropose::decode(request.as_slice()).unwrap();
                            let other_peer = SimplePeer{ nickname: propose.nickname, peer_id: peer };
                            state.game_state = GameState::ACCEPT(AcceptDecline { selected: false, peer: Some(other_peer), rend_state: rend.clone(), channel: Some(channel) } );
                        }

                        
                        // accept or decline?
                        //let respone = ChallengeResponse { accepted: true };
                        //let mut buf = vec![];
                        //respone.encode(&mut buf).unwrap();
                        //swarm.behaviour_mut().challenge.send_response(channel, buf).unwrap();


                        // check game state to see what to do eith this message
                        if let GameState::BATTLE(battle) = &mut state.game_state {
                            // we are in a game
                            match request {

                                _ => {}
                            }
                        }
                        // in message state. ignore this
                    },

                    // someone trying to talk to me
                    SwarmEvent::Behaviour(MyBehaviourEvent::Challenge(
                        request_response::Event::Message {
                            peer,
                            message: request_response::Message::Response { response, .. },
                            ..
                        }
                    )) => {
                        let msg = ChallengeResponse::decode(response.as_slice()).unwrap();
                        if msg.accepted {
                            // someone accepted my challenge request
                            if let GameState::RENDEZVOUS(rend) = &mut state.game_state {

                                state.game_state = GameState::BUILD(BuildState {
                                    opponent_peer_id: peer,
                                    opponent_nickname: "idk".to_string(),
                                    my_board: [[false;10];10],
                                    current_ship: Ship::CARRIER,
                                    cursor: [0,0],
                                    is_placing: false,
                                    placing_cursor: QuadDirection::UP,
                                    board_ready: false,
                                    am_challenger: true,// i challenged them
                                });
                            }
                        }// else, do nothing
                    },

                    SwarmEvent::Behaviour(MyBehaviourEvent::Rendezvous(
                        libp2p::rendezvous::client::Event::Discovered { registrations, cookie, .. }
                    )) => {
                        if let GameState::RENDEZVOUS(rend) = &mut state.game_state {
                            for regs in registrations {
                                let peer_id = regs.record.peer_id();
                                if peer_id == *swarm.local_peer_id() {
                                    continue;
                                }
                                rend.seeking_peers.push(peer_id);// add peers seeking peers to the list (super cool patern matching!!)
                                
                                for addr in regs.record.addresses() {
                                    swarm.dial(addr.clone())?;
                                    
                                }
                                swarm.behaviour_mut().gossipsub.add_explicit_peer(&peer_id);
                                
                            }
                        }
                        
                    }

                        // game logic stuff
                    // incoming request
                    SwarmEvent::Behaviour(MyBehaviourEvent::Battleship(
                        request_response::Event::Message {
                            peer,
                            message: request_response::Message::Request { request, channel, .. },
                            ..
                        }
                    )) => {
                        let msg = BattleshipRequest::decode(request.as_slice()).unwrap();

                        match msg {
                            // challengers board is ready
                            BattleshipRequest {msg: Some(msg)} => {
                                match msg {
                                    // board setup
                                    BattleShip::battleship_request::Msg::BoardReady(BoardReady) => {
                                        if let GameState::BUILD(build) = &mut state.game_state {
                                            if build.board_ready {// send BoardAck
                                                let response = BattleshipResponse { msg: Some(battleship_response::Msg::BoardAck(BoardAck {  })) };
                                                let mut buf = vec![];
                                                response.encode(&mut buf)?;
                                                swarm.behaviour_mut().battleship.send_response(channel, buf);

                                                // change state to battle
                                                state.game_state = GameState::BATTLE(BattleState {
                                                    opponent_peer_id: build.opponent_peer_id,
                                                    opponent_nickname: build.opponent_nickname.clone(),
                                                    my_board: build.my_board,
                                                    their_board: [[false;10];10],
                                                    my_shots: [[false;10];10],
                                                    their_shots: [[false;10];10],
                                                    my_turn: false,
                                                    shot_seq: 0,
                                                    phase: BattlePhase::WaitingForOpponent,
                                                    cursor: build.cursor,
                                                    am_challenger: false,
                                                    pening_shot: None,
                                                })
                                            }
                                        }
                                    }

                                    // recv a shot
                                    BattleShip::battleship_request::Msg::Shot(shot) => {
                                        if let GameState::BATTLE(battle) = &mut state.game_state {
                                            battle.phase = BattlePhase::MyTurn;
                                            battle.shot_seq = shot.seq + 1;
                                            
                                            battle.their_shots[shot.row as usize][shot.col as usize] = true;
                                            
                                            // checkif the shot was a hit
                                            if battle.my_board[shot.row as usize][shot.col as usize] {
                                                
                                                // it hit
                                                // set this cell to false so it cant be hit again
                                                battle.my_board[shot.row as usize][shot.col as usize] = false;

                                                // was this ship sunk?
                                                // FUUUUUUUUCK gotta re-write the whole jit

                                                // did i just lose?
                                                let mut am_alive: bool = false;
                                                for r in battle.my_board.iter() {
                                                    for c in r.iter() {
                                                        if *c {
                                                            am_alive = true;
                                                            break;
                                                        }
                                                    }
                                                    if am_alive {
                                                        break;
                                                    }
                                                }

                                                // send hit response
                                                let response = BattleshipResponse {msg: Some(battleship_response::Msg::ShotResult(ShotResult{
                                                    seq: battle.shot_seq,
                                                    hit: true,
                                                    sunk: false,
                                                    won: !am_alive,
                                                }))};
                                                let mut buf = vec![];
                                                response.encode(&mut buf);
                                                swarm.behaviour_mut().battleship.send_response(channel, buf);


                                            } else {
                                                // send hit response
                                                let response = BattleshipResponse {msg: Some(battleship_response::Msg::ShotResult(ShotResult{
                                                    seq: battle.shot_seq,
                                                    hit: false,
                                                    sunk: false,
                                                    won: false
                                                }))};
                                                let mut buf = vec![];
                                                response.encode(&mut buf);
                                                swarm.behaviour_mut().battleship.send_response(channel, buf);

                                            }
                                        }
                                    }

                                    _ => {}
                                }
                                
                            }

                            _ => {}
                        }

                    }
                    // icoming response
                    SwarmEvent::Behaviour(MyBehaviourEvent::Battleship(
                        request_response::Event::Message {
                            peer,
                            message: request_response::Message::Response {
                                response,
                                ..},
                            ..
                        }
                    )) => {
                        if let GameState::BATTLE(battle) = &mut state.game_state {
                            let msg = BattleshipResponse::decode(response.as_slice()).unwrap();
                            match msg {
                                BattleshipResponse { msg: Some(req) } => {
                                    match req {
                                        BattleShip::battleship_response::Msg::ShotResult(ShotResult {
                                            seq,
                                            hit,
                                            sunk,
                                            won
                                        }) => {
                                            // shot result received
                                            if let Some([row, col]) = battle.pening_shot.take() {
                                                battle.my_shots[row][col] = true;
                                                if hit {
                                                    battle.their_board[row][col] = true;
                                                }
                                                if won {
                                                    // i just won
                                                    todo!();
                                                }
                                            }
                                            battle.shot_seq = seq;
                                            battle.phase = BattlePhase::WaitingForOpponent;
                                        }

                                        _ => {}
                                    }
                                }
                                _ => {}
                            }
                        }
                        
                    }

                    SwarmEvent::Behaviour(MyBehaviourEvent::Kademlia(
                        libp2p::kad::Event::RoutingUpdated { peer, addresses, .. }
                    )) => {

                        let record_key = libp2p::kad::RecordKey::new(&peer.to_bytes());
                        swarm.behaviour_mut().kademlia.get_record(record_key);

                        for addr in addresses.iter() {
                            let _ = swarm.dial(
                                libp2p::swarm::dial_opts::DialOpts::peer_id(peer)
                                    .addresses(addresses.iter().cloned().collect())
                                    .build()
                            );
                            break;
                        }
                        swarm.behaviour_mut().gossipsub.add_explicit_peer(&peer);

                    }

                    SwarmEvent::ConnectionEstablished { peer_id, .. } => {
                        state.total_connections += 1;
                        swarm.behaviour_mut().gossipsub.add_explicit_peer(&peer_id);
                        swarm.behaviour_mut().kademlia.get_closest_peers(state.my_peer_id);
                    }

                    

                    //      DEBUG!!!

                    SwarmEvent::NewListenAddr { address, .. } => {
                        // display this in your TUI messages list as a debug message
                        state.messages.push(DisplayMessage {
                            nickname: "SYSTEM".to_string(),
                            peer_id: "local".to_string(),
                            content: format!("Listening on: {}", address),
                            timestamp: 0,
                        });
                    }

                    SwarmEvent::Behaviour(MyBehaviourEvent::Kademlia(e)) => {
                        state.messages.push(DisplayMessage {
                            nickname: "KAD".to_string(),
                            peer_id: "local".to_string(),
                            content: format!("{:?}", e),
                            timestamp: 0
                        });
                    }

                    

                    _ => {},
                }
            }

        }



        
        
    }

    

    disable_raw_mode()?;
    terminal.clear()?;
    Ok(())
}

fn safe_exit(swarm: &mut libp2p::Swarm<MyBehaviour>, state: &mut AppState) {

    // unsubscribe from all topics
    for topic in state.recent_topics.clone() {
        let topic_string = format!("peerboard/v1/{topic}").to_string();
        let topic_hash = gossipsub::IdentTopic::new(topic_string.clone());
        swarm.behaviour_mut().gossipsub.unsubscribe(&topic_hash);
    }

    // unregister from rendezvous
    swarm.behaviour_mut().rendezvous.unregister(
        libp2p::rendezvous::Namespace::new("peerboard/challenge/seeking".to_string()).unwrap(),
        BOOTSTRAP_PEER_ID.parse().unwrap(),
    );

    state.exit = true;// exit the loop
}

fn is_place_possible(state: &BuildState) -> bool {

    if state.current_ship == Ship::None {
        return false;
    }

    fn collision(board: [[bool;10];10], here: [i32;2], direction: &QuadDirection, len: usize) -> bool {
        match direction {
            QuadDirection::UP => {
                for i in 0..len {
                    let row = here[1] - i as i32;
                    let col = here[0];
                    if board[row as usize][col as usize] {
                        return true;
                    }
                }
                false
            }
            QuadDirection::DOWN => {
                for i in 0..len {
                    let row = here[1] + i as i32;
                    let col = here[0];
                    if board[row as usize][col as usize] {
                        return true;
                    }
                }
                false
            }
            QuadDirection::RIGHT => {
                for i in 0..len {
                    let row = here[1];
                    let col = here[0] + i as i32;
                    if board[row as usize][col as usize] {
                        return true;
                    }
                }
                false
            }
            QuadDirection::LEFT => {
                for i in 0..len {
                    let row = here[1];
                    let col = here[0] - i as i32;
                    if board[row as usize][col as usize] {
                        return true;
                    }
                }
                false
            }
        }
    }

    fn out_of_bounds(here: [i32;2], direction: &QuadDirection, len: usize) -> bool {
        match direction {
            QuadDirection::UP => {
                here[1] - (len as i32 - 1) < 0
            }
            QuadDirection::RIGHT => {
                here[0] + (len as i32 - 1) > 9
            }
            QuadDirection::DOWN => {
                here[1] + (len as i32 - 1) > 9
            }
            QuadDirection::LEFT => {
                here[0] - (len as i32 - 1) < 0
            }
        }
    }

    return !(out_of_bounds(state.cursor, &state.placing_cursor, state.current_ship.length() as usize)
            || collision(state.my_board, state.cursor, &state.placing_cursor, state.current_ship.length() as usize));

}

fn draw_ui(frame: &mut Frame, state: &mut AppState) {

    let outer_chunks = Layout::horizontal([
        Constraint::Percentage(30),
        Constraint::Percentage(70),
    ]).split(frame.area());// the whole area

    let topic_chunks = Layout::vertical([
        Constraint::Min(0),
        //Constraint::Max(80),
        Constraint::Length(3),
    ]).split(outer_chunks[0]);
    
    let msg_chunks = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(3),
    ]).split(outer_chunks[1]);// subsection withing the seconds chunk of the outer chunk

    // input buffer stuff
    
    let (msg_buffer, topic_buffer) = if state.selected_area == 1 {
        (format!("{}█", state.msg_buffer), state.topic_buffer.clone())
    } else {
        (state.msg_buffer.clone(), format!("{}█", state.topic_buffer))
    };

        // msg chunks
    // calculate the height avaliable vs the height needed to render messages
    let avavliable_height = msg_chunks[0].height as usize;

    let mut rows_used = 0;
    let mut visible: Vec<&DisplayMessage> = Vec::new();

    for msgs in state.messages.iter().rev() {// reverse order, was sorted as ACCENDING in sql
        let msg_height = 2; // 1 for name + peer_id, 1 for content
        if rows_used + msg_height > avavliable_height - 2 {
            break;
        }
        rows_used += msg_height;
        visible.push(msgs);
    }

     // message area
    let items: Vec<ListItem> = visible.iter().map(|msg| {

        let header = Line::from(format!(
            "# # {} ({}) # #",
            msg.nickname,
            &msg.peer_id,
        ));
        let body = Line::from(msg.content.clone());
        ListItem::new(vec![header, body])
    }).collect();

    let list = List::new(items)
        .block(Block::bordered()
        .title(format!("{} | #peers: {} | total_connections: {}",
            state.current_topic.clone(), state.peer_counts.get(&state.current_topic).unwrap_or(&0), state.total_connections))
            .border_set(border::THICK))
        .direction(ListDirection::BottomToTop);
    

    // input box
    let input = Paragraph::new(msg_buffer.as_str())
        .block(Block::bordered().title("Input"));
    

        // topic chunks
    
    let topic_list = List::new(state.recent_topics.clone())
        .block(Block::bordered().title("Recently Visited").border_set(border::THICK));
    

    let topic_input = Paragraph::new(topic_buffer.as_str())
        .block(Block::bordered().title("Add New Topic"));

    
    frame.render_widget(list, msg_chunks[0]);
    frame.render_widget(input, msg_chunks[1]);
    frame.render_widget(topic_list, topic_chunks[0]);
    frame.render_widget(topic_input, topic_chunks[1]);



}


fn draw_ren(frame: &mut Frame, state: &mut AppState) {
    if let GameState::RENDEZVOUS(rend) = &mut state.game_state {

        let peers_list = List::new(rend.seeking_peers.clone().iter()
            .map(|peer| {
                peer.to_string()
            }))
            .block(Block::bordered().title("seeking peers".to_string()))
            .highlight_symbol("> ");

        
        let mut list_state = ListState::default();
        list_state.select(rend.selected);
        
        
        frame.render_stateful_widget(peers_list, frame.area(), &mut list_state);

        //frame.render_widget(peers_list, frame.area());
    }
}

fn draw_proposition(frame: &mut Frame, state: &mut AppState) {
    // we recvied a battle request
    // need to accept or decline
    if let GameState::ACCEPT(acpt) = &mut state.game_state {

        let peer = acpt.peer.as_ref().unwrap();

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Percentage(50),
                Constraint::Percentage(50)
            ])
            .split(frame.area());

        // prompt
        let message = Paragraph::new(format!("{} ({}) Has challenged you!",
                peer.nickname,
                peer.peer_id))
            .block(Block::default().borders(Borders::ALL).title("Accept or Decline?"));

        frame.render_widget(message, chunks[0]);

        // buttons
        let (acc_style, dec_style) = match acpt.selected {
            true => {
                (Style::default().fg(Color::Green), Style::default())
            }
            false => {
                (Style::default(),Style::default().fg(Color::Red))
            }
        };

        let acc_btn = Paragraph::new("Accept").block(
            Block::default()
            .borders(Borders::ALL)
            .style(acc_style)
        );
        let dec_btn = Paragraph::new("Decline").block(
            Block::default()
            .borders(Borders::ALL)
            .style(dec_style)
        );

        let btn_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(50),
                Constraint::Percentage(50),
            ])
            .split(chunks[1]);

        frame.render_widget(acc_btn, btn_chunks[0]);
        frame.render_widget(dec_btn, btn_chunks[1]);
        
        

    }

}

fn draw_build(frame: &mut Frame, state: &mut AppState) {
    if let GameState::BUILD(build) = &state.game_state {
        let board_str = draw_board(&build);

        let board = Paragraph::new(board_str)
            .block(Block::bordered().title("Place your ships"));

        frame.render_widget(board, frame.area());
    }
    
}


fn draw_battle(frame: &mut Frame, state: &mut AppState) {
    if let GameState::BATTLE(battle) = &state.game_state {
        let (my_board_str, their_board_str) = draw_board_battle(&battle);

        let my_board = Paragraph::new(my_board_str)
            .block(Block::bordered().title("Your board"));
        let their_board = Paragraph::new(their_board_str)
            .block(Block::bordered().title("Their board"));

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Percentage(50),
                Constraint::Percentage(50)
            ])
            .split(frame.area());

        frame.render_widget(their_board, chunks[0]);
        frame.render_widget(my_board, chunks[1]);

    }
}

fn draw_board(state: &BuildState) -> String {
    let board_str: String = state.my_board.iter().enumerate().map(|(r, row)| {
        let row_str: String = row.iter().enumerate().map(|(c, cell)| {
            if state.is_placing {
                match &state.placing_cursor {
                    QuadDirection::UP => {
                        if (state.cursor[1] != 0) {// yuck and i hate it!
                            if r == state.cursor[1] as usize - 1 && c == state.cursor[0] as usize {
                                return "[|]".to_string();
                            }
                        }
                    }
                    QuadDirection::RIGHT => {
                        if c == state.cursor[0] as usize + 1 && r == state.cursor[1] as usize {
                            return "[-]".to_string();
                        }
                    }
                    QuadDirection::DOWN => {
                        if r == state.cursor[1] as usize + 1 && c == state.cursor[0] as usize {
                            return "[|]".to_string();
                        }
                    }
                    QuadDirection::LEFT => {
                        if (state.cursor[0] != 0) {
                            if c == state.cursor[0] as usize - 1 && r == state.cursor[1] as usize {
                                return "[-]".to_string();
                            }
                        }
                        
                    }
                    

                }
            }
            if r == state.cursor[1] as usize && c == state.cursor[0] as usize {
                "[+]".to_string()
            }
            else if (*cell) {
                "[X]".to_string()
            } else {
                "[ ]".to_string()
            }
        }).collect();
        row_str + "\n"
    }).collect();

    return board_str;
    
}

fn draw_board_battle(state: &BattleState) -> (String, String) {
    let my_board_str: String = state.my_board.iter().enumerate().map(|(r, row)| {
        let row_str: String = row.iter().enumerate().map(|(c, cell)| {
            if state.their_shots[r][c] {
                "[O]".to_string()
            }
            else if (*cell) {
                "[X]".to_string()
            } else {
                "[ ]".to_string()
            }
        }).collect();
        row_str + "\n"
    }).collect();

    let their_board_str: String = state.my_shots.iter().enumerate().map(|(r, row)| {
        let row_str: String = row.iter().enumerate().map(|(c, cell)| {
            if r == state.cursor[1] as usize && c == state.cursor[0] as usize {
                "[+]".to_string()
            }
            else if state.their_board[r][c] && *cell {
                "[X]".to_string()
            }
            else if *cell {
                "[O]".to_string()
            }
            else {
                "[ ]".to_string()
            }
        }).collect();
        row_str + "\n"
    }).collect();

    return (my_board_str, their_board_str);
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {

    // cli input
    
    let cli = Cli::parse();

    // sql init

    let conn = Connection::open(cli.sqldb).expect("couldnt open table");
    
    conn.execute(
        "CREATE TABLE IF NOT EXISTS msgs (
            peer_id     TEXT NOT NULL,
            topic       TEXT NOT NULL,
            content     TEXT,
            timestamp   INTEGER NOT NULL,
            message_id  TEXT NOT NULL,
            nickname    TEXT
        )",
        (),
    ).expect("failed to create table");
    
    // TUI stuff

    // key stuff

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
        let bytes = fs::read(cli.ident_key.unwrap()).expect("error reading key file");
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
        .with_quic()
        .with_behaviour(| key| {

            // set up kademlia
            let peer_id = key.public().to_peer_id();

            let store = MemoryStore::new(peer_id);
            let mut kad_config = KadConfig::new(StreamProtocol::new("/peerboard/kad/1.0.0"));
            kad_config.set_query_timeout(Duration::from_secs(10));
            kad_config.set_parallelism((NonZero::new(3).unwrap()));

            let mut kademlia = KadBehaviour::with_config(key.public().to_peer_id(), store, kad_config);
            kademlia.set_mode(Some(Mode::Server));
            

            // set up mdns
            let mdns = mdns::tokio::Behaviour::new(
                mdns::Config::default(), key.public().to_peer_id()
            ).expect("mdns init failed");

            // set up gossipsub
            // msg id auth
            let msg_id_fn = |msg: &gossipsub::Message| {
                // use the uuid from the message. if uuid doesnt exist: kill self.
                if let Ok(decode) = PeerBoardMessage::decode(msg.data.as_slice()) {
                    gossipsub::MessageId::from(decode.message_id)
                } else {
                    gossipsub::MessageId::from(Uuid::new_v4().to_string())
                }
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
                challenge: request_response::cbor::Behaviour::new(
                    [(StreamProtocol::new("/peerboard/challenge/1.0.0"), ProtocolSupport::Full)],
                    request_response::Config::default()
                ),
                battleship: request_response::cbor::Behaviour::new(
                    [(StreamProtocol::new("/peerboard/battleship/1.0.0"), ProtocolSupport::Full)],
                    request_response::Config::default(),
                ),
                rendezvous: libp2p::rendezvous::client::Behaviour::new(key.clone()),
                kademlia,
                gossipsub,
                mdns,
            }
        })?
        .with_swarm_config(|cfg| cfg.with_idle_connection_timeout(Duration::from_secs(30)))
        .build();


    // set up my nodes id stuffs v
    let listen_port: String = if cli.port.is_some() {
        cli.port.unwrap()
    } else {
        "0".to_string()
    };

    let multiaddr: Multiaddr = format!("/ip4/0.0.0.0/tcp/{listen_port}").parse()?;
    let multiaddr_quic: Multiaddr = format!("/ip4/0.0.0.0/udp/{listen_port}/quic-v1").parse()?;

    let _ = swarm.listen_on(multiaddr.clone());
    let _ = swarm.listen_on(multiaddr_quic.clone());

    let mut tcp_port = None;
    let mut udp_port = None;

    loop {// get the correct port bindings
        match swarm.select_next_some().await {
            SwarmEvent::NewListenAddr { address, .. } => {
                for proto in address.iter() {
                    match proto {
                        libp2p::multiaddr::Protocol::Tcp(p) => tcp_port = Some(p),
                        libp2p::multiaddr::Protocol::Udp(p) => udp_port = Some(p),
                        _ => {}
                    }
                }
                if tcp_port.is_some() && udp_port.is_some() {
                    println!("tcp: {:?}, udp: {:?}", tcp_port, udp_port);
                    break;
                }
            }
            _ => {}
        }
    }
    

    if let Some(ip) = cli.public_ip {
        if let Some(p) = tcp_port {
            let ext: Multiaddr = format!("/ip4/{ip}/tcp/{p}").parse()?;
            swarm.add_external_address(ext.clone());
            swarm.behaviour_mut().kademlia.add_address(&local_peer_id, ext);
        }
        if let Some(p) = udp_port {
            let ext_quic: Multiaddr = format!("/ip4/{ip}/udp/{p}/quic-v1").parse()?;
            swarm.add_external_address(ext_quic.clone());
            swarm.behaviour_mut().kademlia.add_address(&local_peer_id, ext_quic);
        }

        let external_addrs: Vec<Multiaddr> = swarm.external_addresses().cloned().collect();
        if !external_addrs.is_empty() {
            // create a record of my address
            let addr_bytes = external_addrs.iter()
                .flat_map(|addr| addr.to_vec())
                .collect::<Vec<u8>>();

            let record_key = libp2p::kad::RecordKey::new(&local_peer_id.to_bytes());
            let record = libp2p::kad::Record::new(record_key, addr_bytes);

            if let Err(e) = swarm.behaviour_mut().kademlia.put_record(record, libp2p::kad::Quorum::One) {
                println!("failed to publish address record: {}", e);
            } else {
                println!("published address record!");
            }

        }

    }
    

    
    //swarm.add_external_address(multiaddr.clone());
    //swarm.add_external_address(multiaddr_quic.clone());
    
    

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
        swarm.behaviour_mut().kademlia.bootstrap();
        
        swarm.dial(bootstrap_multiaddr)?;
        // self lookup!!! v

        swarm.behaviour_mut().kademlia.get_closest_peers(local_peer_id);
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

    // unregister from the rendezvous place on startup
    swarm.behaviour_mut().rendezvous.unregister(
        libp2p::rendezvous::Namespace::new("peerboard/challenge/seeking".to_string()).unwrap(),
        BOOTSTRAP_PEER_ID.parse().unwrap(),
    );



    // the main thing!!! v

    if (!cli.debug) {
        let mut app = AppState{
            current_topic: topic_string.clone(),
            subscribed_topics: vec![topic_string.clone()],
            msg_buffer: String::new(),
            topic_buffer: String::new(),
            messages: Vec::new(),
            recent_topics: Vec::new(),
            selected_area: 0,
            peer_counts: HashMap::new(),
            game_state: GameState::MESSAGE,
            exit: false,
            total_connections: 0,
            self_lookup_done: false,
            my_peer_id: local_peer_id,
        };

        enable_raw_mode()?;
        let mut terminal = Terminal::new(CrosstermBackend::new(stdout()))?;
        terminal.clear()?;

        let result = run_tui(&mut terminal, &mut swarm, &mut app, &conn).await;

        disable_raw_mode()?;
        terminal.clear()?;

        result
    } else {

        let mut other_peer_id: Option<PeerId> = None;

        let mut stdin: io::Lines<BufReader<io::Stdin>> = io::BufReader::new(io::stdin()).lines();

        loop {
            select! {

                // simple msg match
                Ok(Some(line)) = stdin.next_line() => {
                    // construct a message based on the input

                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_secs() as i64;

                    let msg = PeerBoardMessage {
                        peer_id: local_peer_id.to_string(),
                        topic: topic_string.clone(),
                        content: line,
                        timestamp: now,
                        message_id: Uuid::new_v4().to_string(),
                        nickname: "alex".to_string(),
                    };

                    // check the message for validity
                    if (check_msg(&msg)) {
                        let mut buf = Vec::new();
                        msg.encode(&mut buf).unwrap();
                        swarm.behaviour_mut().gossipsub.publish(topic.clone(), buf)?;
                        // add it to the db
                        conn.execute(
                            "INSERT INTO msgs (peer_id, topic, content, timestamp, message_id, nickname)
                            VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                        (msg.peer_id, msg.topic, msg.content, msg.timestamp, msg.message_id, msg.nickname),
                        ).expect("couldnt add msg to the db");
                    }
                    else {
                        println!("bad message. didnt publish");
                    }

                    
                }

                

                // swam match
                event = swarm.select_next_some() => match event {

                    SwarmEvent::Dialing { peer_id, connection_id } => {
                        println!("Dialing {peer_id:?}");
                    }

                    SwarmEvent::OutgoingConnectionError { peer_id, error, .. } => {
                        println!("FAILED to dial {peer_id:?}: {error}");
                    }

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
                        message,
                        ..
                        })) => {
                            match PeerBoardMessage::decode(message.data.as_slice()) {
                                Ok(msg) => {
                                    // check the message for validity
                                    if (check_msg(&msg)) {
                                        // chgeck if it exists within the db v

                                        let msg_exists: bool = conn
                                            .query_row(
                                                "SELECT COUNT(*) FROM msgs WHERE message_id = ?1",
                                                [&msg.message_id],
                                                |row| row.get::<_, i64>(0),
                                            )
                                            .unwrap_or(0) > 0;

                                        if msg_exists {
                                            //duplicate message
                                            println!("duplicate message recieved");
                                        }
                                        else {
                                            // valid good non duplicate message
                                            println!("[{}] {}: {}", msg.topic, msg.nickname, msg.content);
                                            // add it to the db
                                            conn.execute(
                                                "INSERT INTO msgs (peer_id, topic, content, timestamp, message_id, nickname)
                                                VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                                            (msg.peer_id, msg.topic, msg.content, msg.timestamp, msg.message_id, msg.nickname),
                                            ).expect("couldnt add msg to the db");
                                        }

                                        
                                        

                                    }
                                    else {
                                        println!("received a malformed message");
                                    }
                                    
                                },
                                Err(e) => println!("failed to decode msg: {e}"),
                            }
                        }

                    _ => println!("unhandled: {:?}", event),
                }

                
                
            }
        }

        Ok(())

    }

    
}
