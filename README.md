


# How to build

cargo run -- --help gives:

Usage: asg2 [OPTIONS] --sqldb <SQLDB>

Options:
      --ident-key <IDENT_KEY>  
      --peer <PEER>            
      --port <PORT>            
      --sqldb <SQLDB>          
  -d, --debug                  
      --public-ip <PUBLIC_IP>  
      --nick <NICK>            
  -h, --help                   Print help

only a databade name is required and does not need to exist, it will be created in that case with the name given by you.
--ident-key is the key for the persistant identity of a node.
If not given in the CLI, one will be generated with name "ident-keypair.key"
--peer is optional and sets an initial peer to connect to. the app with connect to the bootstrap node in any case.
--port sets the internal port. this is optional
--debug is depreciated
--public ip is optional, i lets the program know what your public IP is for routing, this is learnt so is unnecasary
--nick is the human readable nickname for your node, if not set, it will default to Anonymous


to build and run initailly just run:
cargo run -- --sqldb msg_db

this will create the database and the keypair, for future runs, run:
cargo run -- --sqldb msg_db --idenk-key ident-keypair.key


# Controls

## Messaging

after running the app, you will be in the main messaging state.
press the up and down arrow keys to chnage the cursor beteen the left and right buffer.
You will automatically subscribe to "/general".
pressing enter on the right will post a message.
when in the right buffer, you can enter a new topic name like "rust".
It will automatically prepend the "peerboard/...." under the hood, and you will be unsubscribed from the current topic.

## Rendezvous

To rendezvous with peers, type "/rendezvous" in the left buffer. MUST start with "/"
This will take you to the rendezvod screen.
You can press up and down to navigate the list of peers.
pressing Escape will send you back to the messaging state.

### Send a request
Pressing Enter will send a battle request to the selected peer, in which case you must wait for a response.

### Recv a response
If you are registerd, anyone can send you a request, in which case, a big ACCEPT / DECLINE message will promt you.
If you press decline, you go back to rendezvous.
If you accept then...

# Battleship

## Build
Each game starts in the build phase, where you must place your peices.
the right shows you the peices remaining.
Arrow keys navigate the cursor around the board.
Pressing enter will select that cell, then, arrow keys define the direction of placement.
If you want to change the cell, press backspace.
If you are happing with the position and direction, press enter again and that peice will be consumed.

## Battle
Once all preices are placed, you will move to the battle phase.
If you were the first to finish building your board, you will not be able to move the cursor until your opponent is ready.
If it is your turn (as indicated on the right), move the cursor and press enter to fire.
If you shot was a MISS, an "O" will be placed where you shot, if you hit, and "X" will be placed
These same symbols will be placed on your board representing shots fired by your opponent.

If a shot you fire sinks one of your opponents ship, the SHIPS SUNK counter will increase.

## Winning or Losing
If a shot you fire wins you the game, a winning screen will be displayed for 3 seconds, then you will be sent back to the message state.
If a shot fired by your opponent makes you lose, a losing screen will be displayed for 3 seconds and you will be sent back to the messaging state.

From the messaging state, you can chose to play agian by typing "/rendezvous" again.

