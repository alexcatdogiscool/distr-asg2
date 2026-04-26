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
use regex::Regex;


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

    #[arg(long)]
    nick: Option<String>,

}

#[derive(NetworkBehaviour)]
struct MyBehaviour {
    challenge: request_response::cbor::Behaviour<Vec<u8>, Vec<u8>>,
    battleship: request_response::cbor::Behaviour<Vec<u8>, Vec<u8>>,
    rendezvous: libp2p::rendezvous::client::Behaviour,
    kademlia: KadBehaviour<MemoryStore>,
    gossipsub: gossipsub::Behaviour,
    mdns: mdns::tokio::Behaviour,
    identify: identify::Behaviour,
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

    if msg.content.as_bytes().len() > 4096 {
        return false;
    }
    if !msg.topic.starts_with("peerboard/v1/") {
        return false;
    }
    if msg.timestamp - now > 300 {
        return false;
    }
    if msg.nickname.as_bytes().len() > 32 {
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
    FINISHED(FinishedState),
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum GameResult {
    WON,
    LOST,
    RESIGN,
}

struct FinishedState {
    result: GameResult,
    time: std::time::Instant,
}

struct AcceptDecline {
    selected: bool,
    peer: Option<SimplePeer>,
    rend_state: RendezvousState,
    channel: Option<request_response::ResponseChannel<Vec<u8>>>,
}

#[derive(Clone, Copy)]
struct filledBoard {
    board: [[Ship;10];10],
}

#[derive(Clone, Copy)]
struct guessBoard {
    board: [[bool;10];10],
}

impl filledBoard {
    pub fn new() -> Self {
        filledBoard { board: [[Ship::None;10];10] }
    }
}

impl guessBoard {
    pub fn new() -> Self {
        guessBoard { board: [[false;10];10] }
    }
}



struct BuildState {
    opponent_peer_id: PeerId,
    opponent_nickname: String,
    my_board: filledBoard,
    current_ship: Ship,
    cursor: [i32;2],
    is_placing: bool,
    placing_cursor: QuadDirection,
    board_ready: bool,
    am_challenger: bool,
    opponent_ready: bool,
    board_ready_sent: bool
}

impl BuildState {
    fn place(&mut self) {
        // we know that placing is possible because we checked before running this (hopefully)
        match self.placing_cursor {
            QuadDirection::UP => {
                for r in (self.cursor[1] - (self.current_ship.length() as i32 - 1))..=self.cursor[1] {
                    self.my_board.board[r as usize][self.cursor[0] as usize] = self.current_ship;
                }
            }
            QuadDirection::RIGHT => {
                for c in self.cursor[0]..self.cursor[0] + self.current_ship.length() as i32 {
                    self.my_board.board[self.cursor[1] as usize][c as usize] = self.current_ship;
                }
            }
            QuadDirection::DOWN => {
                for r in self.cursor[1]..self.cursor[1] + self.current_ship.length() as i32 {
                    self.my_board.board[r as usize][self.cursor[0] as usize] = self.current_ship;
                }
            }
            QuadDirection::LEFT => {
                for c in (self.cursor[0] - (self.current_ship.length() as i32 - 1))..=self.cursor[0] {
                    self.my_board.board[self.cursor[1] as usize][c as usize] = self.current_ship;
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

#[derive(PartialEq, Clone, Copy)]
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

    fn ship_to_string(&self) -> String {
        match self {
            Ship::CARRIER => "[C]".to_string(),
            Ship::BATTLESHIP => "[B]".to_string(),
            Ship::CRUISER => "[c]".to_string(),
            Ship::SUBMARINE => "[S]".to_string(),
            Ship::DESTROYER => "[D]".to_string(),
            Ship::None => "[ ]".to_string()
        }
    }
}

fn was_ship_sunk(board: [[Ship;10];10], ship: Ship) -> bool {

    for r in board.iter() {
        for c in r.iter() {
            if *c == ship {
                return false;
            }
        }
    }
    true


}


#[derive(Clone, Copy, PartialEq, Eq)]
enum HitState {
    Hit,
    Miss,
    None,
}

struct BattleState {
    opponent_peer_id: PeerId,
    opponent_nickname: String,
    my_board: filledBoard,
    my_shots: [[HitState;10];10],
    their_shots: [[HitState;10];10],
    my_turn: bool,
    shot_seq: u32,
    phase: BattlePhase,
    cursor: [i32;2],
    am_challenger: bool,
    pening_shot: Option<[usize;2]>,
    ships_sunk: u8,
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
    WaitingForBoardAck { opponent_ready: bool },
    OpponentBoardReady { is_ready: bool },
    WaitingForOpponent,
    MyTurn,
    GameOver { i_won: GameResult },
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
    udp_port: Option<u16>,
    tcp_port: Option<u16>,
    nickname: String,

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

    let mut bootstrap_int = tokio::time::interval(Duration::from_secs(35));


    while !state.exit {

        
        tokio::select! {

            _ = bootstrap_int.tick() => {
                let peer_count: usize = swarm
                    .behaviour_mut()
                    .kademlia
                    .kbuckets()
                    .map(|bucket| bucket.num_entries())
                    .sum();

                if peer_count < 3 {
                    let _ = swarm.behaviour_mut().kademlia.bootstrap();
                }
                // make peer discovery faster
                let _ = swarm.behaviour_mut().kademlia.bootstrap();
                swarm.behaviour_mut().kademlia.get_closest_peers(state.my_peer_id);
            }

            _ = draw_interval.tick() => {
                // draw the tui
                terminal.draw(|frame| {

                    match &mut state.game_state {
                        GameState::MESSAGE => {
                            draw_ui(frame, state);
                        }
                        GameState::RENDEZVOUS(_rend) => {
                            draw_ren(frame, state);
                        }
                        GameState::BATTLE(_battle) => {
                            draw_battle(frame, state);
                        }
                        GameState::ACCEPT(_acpt) => {
                            draw_proposition(frame, state);
                        }
                        GameState::BUILD(_build) => {
                            draw_build(frame, state);
                        }
                        GameState::FINISHED(_finished) => {
                            draw_finished(frame, state);
                            
                        }
                    };
                    
                })?;

                match &mut state.game_state {
                    GameState::FINISHED(finished) => {
                        let len = std::time::Instant::now() - finished.time;
                        if len.as_secs() > 3 {
                            // go back to messaging!
                            state.game_state = GameState::MESSAGE;
                        }
                    }

                    GameState::BATTLE(battle) => {
                        match battle.phase {
                            BattlePhase::GameOver{i_won} => {
                                state.game_state = GameState::FINISHED(FinishedState {
                                    result: i_won,
                                    time: std::time::Instant::now(),
                                });
                            }
                            _ => {}
                        }
                    }

                    _ => {}
                }

                while event::poll(Duration::from_millis(0))? {
                    if let Event::Key(key) = event::read()? {
                        // handle all the key presses

                        match &mut state.game_state {
                            // finished game
                            
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
                                                nickname: state.nickname.clone(),
                                            };

                                            // check the message for validity
                                            if check_msg(&msg) {
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
                                            else if Regex::new("[a-z0-9]+").unwrap().is_match(&state.topic_buffer) {// make sure topic name is valid
                                                
                                                // unsubscribe from previous topic
                                                let old_topic_str = format!("{}", state.current_topic);
                                                let old_topic = gossipsub::IdentTopic::new(old_topic_str.clone());
                                                swarm.behaviour_mut().gossipsub.unsubscribe(&old_topic);

                                                // subscribe to new topic
                                                let topic_str = format!("peerboard/v1/{}", state.topic_buffer.clone());
                                                let new_topic = gossipsub::IdentTopic::new(topic_str.clone());
                                                let _ = swarm.behaviour_mut().gossipsub.subscribe(&new_topic);
                                                if !(state.recent_topics.contains(&topic_str)) {
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

                                match battle.phase {

                                    BattlePhase::OpponentBoardReady{is_ready: ready} => {
                                        if ready {
                                            battle.phase = if battle.my_turn {
                                                BattlePhase::MyTurn
                                            } else {
                                                BattlePhase::WaitingForOpponent
                                            }
                                        }
                                        // else, wait for them to be ready!
                                    }
                                    BattlePhase::WaitingForBoardAck{ opponent_ready: _ready } => {}
                                    BattlePhase::GameOver{ i_won: _won } => {
                                        // go to finished state
                                        
                                        
                                        // this is handled outside of the keyboard event handler
                                    }

                                    BattlePhase::MyTurn | BattlePhase::WaitingForOpponent => {
                                        match key.code {// play the game
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
                                                if battle.phase == BattlePhase::MyTurn && 
                                                    battle.my_shots[battle.cursor[1] as usize][battle.cursor[0] as usize] == HitState::None {//fire a shot

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
                                                    let _ = request.encode(&mut buf);
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

                                    _ => {},
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

                                            let propose = ChallengePropose{ nickname: state.nickname.clone() };
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

                                                // unregister
                                                swarm.behaviour_mut().rendezvous.unregister(
                                                    libp2p::rendezvous::Namespace::new("peerboard/challenge/seeking".to_string()).unwrap(),
                                                    BOOTSTRAP_PEER_ID.parse().unwrap(),
                                                );
                                                
                                                state.game_state = GameState::BUILD(BuildState {
                                                    opponent_peer_id: acpt.peer.as_ref().unwrap().peer_id,
                                                    opponent_nickname: acpt.peer.as_ref().unwrap().nickname.clone(),// hell
                                                    my_board: filledBoard::new(),
                                                    current_ship: Ship::CARRIER,
                                                    cursor: [0;2],
                                                    is_placing: false,
                                                    placing_cursor: QuadDirection::UP,
                                                    board_ready: false,
                                                    am_challenger: false,// i am accepting, i am not the challenger
                                                    opponent_ready: false,
                                                    board_ready_sent: false
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

                                if build.board_ready && !build.board_ready_sent {
                                    // request BoardReady
                                    build.board_ready_sent = true;
                                    let request = BattleshipRequest { msg: Some(battleship_request::Msg::BoardReady(BoardReady {})) };// ???
                                    let mut buf = vec![];
                                    request.encode(&mut buf)?;
                                    swarm.behaviour_mut().battleship.send_request(&build.opponent_peer_id, buf);

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


                    
                    
                    // gossipsub listen
                    SwarmEvent::Behaviour(MyBehaviourEvent::Gossipsub(gossipsub::Event::Message {
                        message,
                        ..
                        })) => {
                            match PeerBoardMessage::decode(message.data.as_slice()) {
                                Ok(msg) => {
                                    // check the message for validity
                                    if check_msg(&msg) {
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

                    SwarmEvent::Behaviour(MyBehaviourEvent::Gossipsub(gossipsub::Event::Subscribed { topic, .. })) => {
                        *state.peer_counts.entry(topic.to_string()).or_insert(0) += 1;
                    }

                    SwarmEvent::Behaviour(MyBehaviourEvent::Gossipsub(gossipsub::Event::Unsubscribed { topic, .. })) => {
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
                        else {
                            // not in rendezvous state
                            // always decline
                            let decline = ChallengeResponse { accepted: false };
                            let mut buf = vec![];
                            decline.encode(&mut buf).unwrap();
                            let _ = swarm.behaviour_mut().challenge.send_response(channel, buf);
                        }
                    },

                    // incoming response
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
                            if let GameState::RENDEZVOUS(_rend) = &mut state.game_state {

                                state.game_state = GameState::BUILD(BuildState {
                                    opponent_peer_id: peer,
                                    opponent_nickname: "idk".to_string(),
                                    my_board: filledBoard::new(),
                                    current_ship: Ship::CARRIER,
                                    cursor: [0,0],
                                    is_placing: false,
                                    placing_cursor: QuadDirection::UP,
                                    board_ready: false,
                                    am_challenger: true,// i challenged them
                                    opponent_ready: false,
                                    board_ready_sent: false
                                });
                            }
                        }// else, do nothing
                    },

                    SwarmEvent::Behaviour(MyBehaviourEvent::Rendezvous(
                        libp2p::rendezvous::client::Event::Discovered { registrations, .. }
                    )) => {
                        if let GameState::RENDEZVOUS(rend) = &mut state.game_state {
                            for regs in registrations {
                                let peer_id = regs.record.peer_id();
                                if peer_id == *swarm.local_peer_id() {
                                    continue;
                                }
                                rend.seeking_peers.push(peer_id);// add peers seeking peers to the list (super cool patern matching!!)
                                
                                for addr in regs.record.addresses() {
                                    let _ = swarm.dial(addr.clone());
                                    
                                }
                                swarm.behaviour_mut().gossipsub.add_explicit_peer(&peer_id);
                                
                            }
                        }
                        
                    }

                        // game logic stuff
                    // incoming request
                    SwarmEvent::Behaviour(MyBehaviourEvent::Battleship(
                        request_response::Event::Message {
                            message: request_response::Message::Request { request, channel, .. },
                            ..
                        }
                    )) => {
                        let msg = BattleshipRequest::decode(request.as_slice()).unwrap();

                        match msg {
                            BattleshipRequest {msg: Some(msg)} => {
                                match msg {
                                    BattleShip::battleship_request::Msg::BoardReady(_BoardReady) => {
                                        // opponent told me "my board is ready"
                                        // send borad ack always
                                        let response = BattleshipResponse { msg: Some(battleship_response::Msg::BoardAck(BoardAck {  })) };
                                        let mut buf = vec![];
                                        response.encode(&mut buf)?;
                                        let _ = swarm.behaviour_mut().battleship.send_response(channel, buf);

                                        if let GameState::BUILD(build) = &mut state.game_state {
                                            // flag them as ready
                                            build.opponent_ready = true;

                                            
                                            
                                        }
                                        else if let GameState::BATTLE(battle) = &mut state.game_state {
                                            //battle.phase = BattlePhase::OpponentBoardReady { is_ready: true }
                                            battle.phase = if battle.am_challenger {
                                                BattlePhase::MyTurn
                                            } else {
                                                BattlePhase::WaitingForOpponent
                                            }
                                        }
                                    }

                                    // recv a shot
                                    BattleShip::battleship_request::Msg::Shot(shot) => {
                                        if let GameState::BATTLE(battle) = &mut state.game_state {

                                            // check if shot is valid
                                            let valid_shot = !(shot.row > 9 || shot.col > 9|| battle.phase == BattlePhase::MyTurn);

                                            if valid_shot {
                                                battle.phase = BattlePhase::MyTurn;
                                                //battle.shot_seq = shot.seq + 1;
                                                
                                                battle.their_shots[shot.row as usize][shot.col as usize] = HitState::Miss;
                                                // checkif the shot was a hit
                                                if battle.my_board.board[shot.row as usize][shot.col as usize] != Ship::None {
                                                    
                                                    // it hit
                                                    battle.their_shots[shot.row as usize][shot.col as usize] = HitState::Hit;

                                                    // what did they hit?
                                                    let shipType = battle.my_board.board[shot.row as usize][shot.col as usize].clone();
                                                    // set this cell to false so it cant be hit again
                                                    battle.my_board.board[shot.row as usize][shot.col as usize] = Ship::None;

                                                    // did i just lose?
                                                    let mut am_alive: bool = false;
                                                    for r in battle.my_board.board.iter() {
                                                        for c in r.iter() {
                                                            if *c != Ship::None {
                                                                am_alive = true;
                                                                break;
                                                            }
                                                        }
                                                        if am_alive {
                                                            break;
                                                        }
                                                    }

                                                    // was this ship sunk?
                                                    let sunk = was_ship_sunk(battle.my_board.board, shipType);
                                                        // send them back the sunk msg
                                                    

                                                    // send hit response
                                                    let response = BattleshipResponse {msg: Some(battleship_response::Msg::ShotResult(ShotResult{
                                                        seq: battle.shot_seq,
                                                        hit: true,
                                                        sunk: sunk,
                                                        won: !am_alive,
                                                    }))};
                                                    let mut buf = vec![];
                                                    response.encode(&mut buf)?;
                                                    let _ = swarm.behaviour_mut().battleship.send_response(channel, buf);

                                                    // if i lost
                                                    // send resign and stuff
                                                    if !am_alive {
                                                        let mesg = BattleshipRequest {msg: Some(battleship_request::Msg::Resign(Resign{}))};
                                                        let mut buf = vec![];
                                                        mesg.encode(&mut buf)?;
                                                        swarm.behaviour_mut().battleship.send_request(&battle.opponent_peer_id, buf);
                                                        battle.phase = BattlePhase::GameOver { i_won: GameResult::LOST };
                                                    }
                                                    
                                                    
                                                    


                                                } else {
                                                    // send hit response
                                                    let response = BattleshipResponse {msg: Some(battleship_response::Msg::ShotResult(ShotResult{
                                                        seq: battle.shot_seq,
                                                        hit: false,
                                                        sunk: false,
                                                        won: false
                                                    }))};
                                                    let mut buf = vec![];
                                                    response.encode(&mut buf)?;
                                                    let _ = swarm.behaviour_mut().battleship.send_response(channel, buf);

                                                }
                                            } else {// shot was invalid
                                                let response = BattleshipResponse {msg: Some(battleship_response::Msg::ShotResult(ShotResult{
                                                    seq: battle.shot_seq,
                                                    hit: false,
                                                    sunk: false,
                                                    won: false
                                                }))};
                                                let mut buf = vec![];
                                                response.encode(&mut buf)?;
                                                let _ = swarm.behaviour_mut().battleship.send_response(channel, buf);

                                                // resign

                                                let req = BattleshipRequest {msg: Some(battleship_request::Msg::Resign(Resign{}))};
                                                let mut buf = vec![];
                                                req.encode(&mut buf)?;
                                                swarm.behaviour_mut().battleship.send_request(&battle.opponent_peer_id, buf);
                                            }
                                        }
                                    }

                                    BattleShip::battleship_request::Msg::Resign(_res) => {
                                        // send resignAck
                                        let response = BattleshipResponse {msg: Some(battleship_response::Msg::ResignAck(ResignAck{}))};
                                        let mut buf = vec![];
                                        response.encode(&mut buf)?;
                                        let _ = swarm.behaviour_mut().battleship.send_response(channel, buf);
                                        // end the game
                                        // check if am alive
                                        let mut am_alive: bool = false;

                                        if let GameState::BATTLE(battle) = &mut state.game_state {
                                            for r in battle.my_board.board.iter() {
                                                for c in r.iter() {
                                                    if *c != Ship::None {
                                                        am_alive = true;
                                                        break;
                                                    }
                                                }
                                                if am_alive {
                                                    break;
                                                }
                                            }
                                        }

                                        if let GameState::BATTLE(battle) = &mut state.game_state {
                                            battle.phase = BattlePhase::GameOver{
                                                i_won: if am_alive {
                                                    GameResult::WON
                                                } else {
                                                    GameResult::LOST
                                                }
                                            };
                                        }
                                        

                                    }

                                    

                                }
                                
                            }

                            

                            _ => {}
                        }

                    }

                    SwarmEvent::Behaviour(MyBehaviourEvent::Battleship(
                        request_response::Event::OutboundFailure {
                            ..
                        }
                    )) => {
                        //didnt recieve a response within 30 seconds
                        // resign

                        if let GameState::BATTLE(battle) = &mut state.game_state {
                            let response = BattleshipRequest { msg: Some(battleship_request::Msg::Resign(Resign {  })) };
                            let mut buf = vec![];
                            response.encode(&mut buf)?;
                            swarm.behaviour_mut().battleship.send_request(&battle.opponent_peer_id, buf);
                            // end the game (resigned)
                            battle.phase = BattlePhase::GameOver{ i_won: GameResult::RESIGN };
                            
                            }
                        

                        
                    }

                    // incoming response
                    SwarmEvent::Behaviour(MyBehaviourEvent::Battleship(
                        request_response::Event::Message {
                            message: request_response::Message::Response {
                                response,
                                ..},
                            ..
                        }
                    )) => {
                        let msg = BattleshipResponse::decode(response.as_slice()).unwrap();

                        if let GameState::BATTLE(battle) = &mut state.game_state {
                            match msg {
                                BattleshipResponse { msg: Some(req) } => {
                                    match req {
                                        // recv a shot response
                                        BattleShip::battleship_response::Msg::ShotResult(ShotResult {
                                            seq,
                                            hit,
                                            sunk,
                                            won
                                        }) => {
                                            // shot result received
                                            if let Some([row, col]) = battle.pening_shot.take() {
                                                battle.my_shots[row][col] = HitState::Miss;
                                                if hit {
                                                    battle.my_shots[row][col] = HitState::Hit;
                                                }
                                                if sunk {
                                                    // display a message
                                                    battle.ships_sunk += 1;
                                                }
                                                if won {
                                                    // i just won
                                                    battle.phase = BattlePhase::GameOver { i_won: GameResult::WON };
                                                    // send a resign
                                                    let mesg = BattleshipRequest {msg: Some(battleship_request::Msg::Resign(Resign{}))};
                                                    let mut buf = vec![];
                                                    mesg.encode(&mut buf);
                                                    swarm.behaviour_mut().battleship.send_request(&battle.opponent_peer_id, buf);

                                                } else if !won {
                                                    battle.phase = BattlePhase::WaitingForOpponent;
                                                }
                                            }
                                            //battle.shot_seq = seq;
                                            
                                        }

                                        // recived resignAck
                                        BattleShip::battleship_response::Msg::ResignAck(_ack) => {
                                            // recieved a resignAck
                                            let i_won: GameResult = match battle.phase {
                                                BattlePhase::GameOver{ i_won: r } => {
                                                    r.clone()
                                                }
                                                _ => {
                                                    GameResult::RESIGN
                                                }
                                            };

                                            battle.phase = BattlePhase::GameOver{
                                                i_won: i_won
                                            }
                                        }
                                        _ => {}
                                    }
                                }
                                _ => {}
                            }
                        }
                        if let GameState::BUILD(build) = &mut state.game_state {
                            match msg {
                                BattleshipResponse { msg: Some(req) } => {
                                    match req {
                                        BattleShip::battleship_response::Msg::BoardAck(_) => {
                                            // in build state and ooponent acknowlaged out buildReady.
                                            // move to battle state and wait for oppoenent

                                            let phase: BattlePhase = if build.opponent_ready {
                                                if build.am_challenger {
                                                    BattlePhase::MyTurn
                                                } else {
                                                    BattlePhase::WaitingForOpponent
                                                }
                                            } else {
                                                BattlePhase::OpponentBoardReady { is_ready: false }
                                            };
                                            state.game_state = GameState::BATTLE(BattleState {
                                                opponent_peer_id: build.opponent_peer_id,
                                                opponent_nickname: build.opponent_nickname.clone(),
                                                my_board: build.my_board.clone(),
                                                my_shots: [[HitState::None;10];10],
                                                their_shots: [[HitState::None;10];10],
                                                my_turn: build.am_challenger,
                                                shot_seq: 1,
                                                phase: phase,
                                                cursor: build.cursor,
                                                am_challenger: build.am_challenger,
                                                pening_shot: None,
                                                ships_sunk: 0,
                                            });


                                        }

                                        _ => {}
                                    }
                                    
                                }

                                

                                

                                _ => {}
                            }
                        }
                        
                    }

                    SwarmEvent::ExternalAddrConfirmed { address } => {
                        
                        // make sure kademlia knows this is us
                        swarm.behaviour_mut().kademlia.add_address(&state.my_peer_id, address);
                    }

                    SwarmEvent::NewExternalAddrCandidate { address } => {
                        
                        // make sure kademlia knows this is us
                        swarm.add_external_address(address.clone());
                        swarm.behaviour_mut().kademlia.add_address(&state.my_peer_id, address);
                    }

                    

                    

                    SwarmEvent::Behaviour(MyBehaviourEvent::Identify(
                        libp2p::identify::Event::Received { peer_id, info, .. }
                    )) => {
                        for addr in info.listen_addrs.clone() {
                            swarm.behaviour_mut().kademlia.add_address(&peer_id, addr);
                        }
                        swarm.behaviour_mut().gossipsub.add_explicit_peer(&peer_id);

                        // Extract just the IP from observed_addr
                        let observed_ip = info.observed_addr.iter().find_map(|proto| {
                            if let libp2p::multiaddr::Protocol::Ip4(ip) = proto {
                                Some(ip)
                            } else {
                                None
                            }
                        });

                        if let Some(ip) = observed_ip {
                            // Combine observed public IP with our actual listen ports
                            if let Some(p) = &state.tcp_port {
                                let addr: Multiaddr = format!("/ip4/{}/tcp/{}", ip, p).parse().unwrap();
                                swarm.add_external_address(addr.clone());
                                swarm.behaviour_mut().kademlia.add_address(&state.my_peer_id, addr);
                            }
                            if let Some(p) = &state.udp_port {
                                let addr: Multiaddr = format!("/ip4/{}/udp/{}/quic-v1", ip, p).parse().unwrap();
                                swarm.add_external_address(addr.clone());
                                swarm.behaviour_mut().kademlia.add_address(&state.my_peer_id, addr);
                            }
                        }
                    }

                    SwarmEvent::Behaviour(MyBehaviourEvent::Kademlia(kad_event)) => {
                        match kad_event {
                            libp2p::kad::Event::RoutingUpdated { peer, addresses, .. } => {

                                

                                for addr in addresses.iter() {
                                    let _ = swarm.dial(
                                        libp2p::swarm::dial_opts::DialOpts::peer_id(peer)
                                            .addresses(addresses.iter().cloned().collect())
                                            .build()
                                    );
                                    let _ = swarm.dial(peer);
                                    swarm.behaviour_mut().kademlia.add_address(&peer, addr.clone());
                                }
                                swarm.behaviour_mut().gossipsub.add_explicit_peer(&peer);
                            }

                            libp2p::kad::Event::OutboundQueryProgressed {result, ..} => {
                                match result {
                                    libp2p::kad::QueryResult::GetClosestPeers(Ok(ok)) => {
                                        
                                        for peer in ok.peers {
                                            
                                            for addr in peer.addrs.clone() {
                                                swarm.behaviour_mut().kademlia.add_address(&peer.peer_id, addr.clone());
                                                
                                            }
                                            let _ = swarm.dial(
                                                libp2p::swarm::dial_opts::DialOpts::peer_id(peer.peer_id)
                                                    .addresses(peer.addrs)
                                                    .build()
                                            );
                                            swarm.behaviour_mut().gossipsub.add_explicit_peer(&peer.peer_id);
                                        }
                                    }

                                    libp2p::kad::QueryResult::Bootstrap(Ok(result)) => {
                                        if result.num_remaining == 0 {
                                            swarm.behaviour_mut().kademlia.get_closest_peers(state.my_peer_id);
                                        }
                                    }

                                    _ => {}
                                }
                            }



                            _ => {
                                
                            }
                        }
                    }

                    SwarmEvent::NewListenAddr { address, .. } => {
                        
                        
                        // tell kademlia about this listen address too
                        swarm.add_external_address(address.clone());
                        swarm.behaviour_mut().kademlia.add_address(&state.my_peer_id, address);
                    }


                    SwarmEvent::ConnectionEstablished { peer_id, .. } => {

                        let ext_addrs: Vec<Multiaddr> = swarm.external_addresses().cloned().collect();
                        

                        for addr in swarm.external_addresses().cloned().collect::<Vec<_>>() {
                            swarm.behaviour_mut().kademlia.add_address(&state.my_peer_id, addr);
                        }

                        // Sync all of them into kademlia as YOUR address
                        for addr in ext_addrs {
                            swarm.behaviour_mut().kademlia.add_address(&state.my_peer_id, addr);
                        }
                        
                        state.total_connections += 1;
                        swarm.behaviour_mut().gossipsub.add_explicit_peer(&peer_id);

                        if peer_id.to_string() == BOOTSTRAP_PEER_ID {

                            
                            swarm.behaviour_mut().kademlia.get_closest_peers(state.my_peer_id);
                            let _ = swarm.behaviour_mut().kademlia.bootstrap();
                            
                        }
                        
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

    fn collision(board: [[Ship;10];10], here: [i32;2], direction: &QuadDirection, len: usize) -> bool {
        match direction {
            QuadDirection::UP => {
                for i in 0..len {
                    let row = here[1] - i as i32;
                    let col = here[0];
                    if board[row as usize][col as usize] != Ship::None {
                        return true;
                    }
                }
                false
            }
            QuadDirection::DOWN => {
                for i in 0..len {
                    let row = here[1] + i as i32;
                    let col = here[0];
                    if board[row as usize][col as usize] != Ship::None {
                        return true;
                    }
                }
                false
            }
            QuadDirection::RIGHT => {
                for i in 0..len {
                    let row = here[1];
                    let col = here[0] + i as i32;
                    if board[row as usize][col as usize] != Ship::None {
                        return true;
                    }
                }
                false
            }
            QuadDirection::LEFT => {
                for i in 0..len {
                    let row = here[1];
                    let col = here[0] - i as i32;
                    if board[row as usize][col as usize] != Ship::None {
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
            || collision(state.my_board.board, state.cursor, &state.placing_cursor, state.current_ship.length() as usize));

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

        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(33),
                Constraint::Percentage(33),
                Constraint::Percentage(34),
            ])
            .split(frame.area());

        // board stuff
        let board_str = draw_board(&build);

        let board = Paragraph::new(board_str)
            .block(Block::new().title("Place your ships"));
        
        // peices to place
        let mut remaining_peices = String::new();
        let mut current: Ship = build.current_ship;
        while current != Ship::None {
            for _ in 0..current.length() {
                remaining_peices.push_str(&current.ship_to_string());
            }
            remaining_peices.push('\n');
            current = current.next();
        }

        let remaining = Paragraph::new(remaining_peices)
            .block(Block::new().title("remaining ships to place"));

        frame.render_widget(board, chunks[1]);
        frame.render_widget(remaining, chunks[2]);
        
    }
    
}


fn draw_battle(frame: &mut Frame, state: &mut AppState) {
    if let GameState::BATTLE(battle) = &state.game_state {

        let outer_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(33),
                Constraint::Percentage(33),
                Constraint::Percentage(34),
            ]).split(frame.area());

        
        // game boards
        let (my_board_str, their_board_str) = draw_board_battle(&battle);

        let my_board = Paragraph::new(my_board_str)
            .block(Block::bordered().title("Your board"));
        let their_board = Paragraph::new(their_board_str)
            .block(Block::bordered().title("Their board"));

        let battle_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Percentage(50),
                Constraint::Percentage(50)
            ])
            .split(outer_chunks[1]);

        // stats!!
        // turn number, ships sunk
        
        let mut sunk_stat: String = format!("{}/5\n", battle.ships_sunk);
        let turn: String = match battle.phase {
            BattlePhase::MyTurn => "Your Turn".to_string(),
            BattlePhase::WaitingForOpponent => "Opponents Turn".to_string(),
            
            _ => {
                "how did you get here...".to_string()
            }
        };

        sunk_stat.push_str(&turn);


        let stats = Paragraph::new(sunk_stat)
                .block(Block::new().title(format!("STATS | Turn Number: {}", battle.shot_seq)));

        

        



        frame.render_widget(their_board, battle_chunks[0]);
        frame.render_widget(my_board, battle_chunks[1]);
        frame.render_widget(stats, outer_chunks[2]);

    }
}

fn draw_board(state: &BuildState) -> String {
    let board_str: String = state.my_board.board.iter().enumerate().map(|(r, row)| {
        let row_str: String = row.iter().enumerate().map(|(c, cell)| {
            if state.is_placing {
                match &state.placing_cursor {
                    QuadDirection::UP => {
                        if state.cursor[1] != 0 {// yuck and i hate it!
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
                        if state.cursor[0] != 0 {
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
            else {
                cell.ship_to_string()
            }
        }).collect();
        row_str + "\n"
    }).collect();

    return board_str;
    
}

fn draw_board_battle(state: &BattleState) -> (String, String) {
    let my_board_str: String = state.my_board.board.iter().enumerate().map(|(r, row)| {
        let row_str: String = row.iter().enumerate().map(|(c, _cell)| {
            match state.their_shots[r][c] {
                HitState::Hit => "[X]".to_string(),
                HitState::Miss => "[O]".to_string(),
                HitState::None => state.my_board.board[r][c].ship_to_string(),
            }
        }).collect();
        row_str + "\n"
    }).collect();

    let their_board_str: String = state.my_shots.iter().enumerate().map(|(r, row)| {
        let row_str: String = row.iter().enumerate().map(|(c, _cell)| {
            if r == state.cursor[1] as usize && c == state.cursor[0] as usize {
                "[+]".to_string()
            }
            else {
                match state.my_shots[r][c] {
                    HitState::Hit => "[X]".to_string(),
                    HitState::Miss => "[O]".to_string(),
                    HitState::None => "[ ]".to_string(),
                }
            }
        }).collect();
        row_str + "\n"
    }).collect();

    return (my_board_str, their_board_str);
}

fn draw_finished(frame: &mut Frame, state: &mut AppState) {
    if let GameState::FINISHED(finished) = &mut state.game_state {
        let msg = match finished.result {
            GameResult::WON => "YOU WON!".to_string(),
            GameResult::LOST => "YOU LOST :(".to_string(),
            GameResult::RESIGN => "YOU RESIGNED".to_string(),
        };

        let outer_chunks = Layout::default()
            .constraints([
                Constraint::Percentage(33),
                Constraint::Percentage(33),
                Constraint::Percentage(34),
            ]).split(frame.area());
        
        let inner_chunks = Layout::default()
            .constraints([
                Constraint::Percentage(33),
                Constraint::Percentage(33),
                Constraint::Percentage(34),
            ]).split(outer_chunks[1]);
        
        let display = Paragraph::new(msg)
            .block(Block::bordered().title("GAME OVER"));

        frame.render_widget(display, inner_chunks[1]);

    }
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
            kad_config.set_parallelism(NonZero::new(3).unwrap());

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

            // identify
            let identify = libp2p::identify::Behaviour::new(
                libp2p::identify::Config::new(
                    "/peerboard/identify/1.0.0".to_string(),
                    key.public(),
                )
            );


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
                    request_response::Config::default().with_request_timeout(Duration::from_secs(30))
                ),
                battleship: request_response::cbor::Behaviour::new(
                    [(StreamProtocol::new("/peerboard/battleship/1.0.0"), ProtocolSupport::Full)],
                    request_response::Config::default().with_request_timeout(Duration::from_secs(30)),
                ),
                rendezvous: libp2p::rendezvous::client::Behaviour::new(key.clone()),
                kademlia,
                gossipsub,
                mdns,
                identify

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

    swarm.listen_on(multiaddr.clone())?;
    swarm.listen_on(multiaddr_quic.clone())?;

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
        
        swarm.dial(bootstrap_multiaddr)?;
        // self lookup!!! v

        //swarm.behaviour_mut().kademlia.get_closest_peers(local_peer_id);
        //swarm.behaviour_mut().kademlia.get_closest_peers(bootstrap_peer_id);
        
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
        //swarm.behaviour_mut().kademlia.bootstrap()?;
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

    if !cli.debug {
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
            udp_port: udp_port,
            tcp_port: tcp_port,
            nickname: cli.nick.unwrap_or("Anonymous".to_string())
        };

        enable_raw_mode()?;
        let mut terminal = Terminal::new(CrosstermBackend::new(stdout()))?;
        terminal.clear()?;

        let result = run_tui(&mut terminal, &mut swarm, &mut app, &conn).await;

        disable_raw_mode()?;
        terminal.clear()?;

        result
    } else {


        Ok(())

    }
    

    
}
