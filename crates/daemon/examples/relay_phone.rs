//! Plays a paired phone against a live relay: connects to
//! `<relay>/connect?host=<id>`, pairs with a `#pair=` link, completes the
//! Noise handshake and prints the daemon's first frame (docs/relay-protocol.md).
//!
//! cargo run -p lynshen-daemon --example relay_phone -- <ws-relay-base> <pair-link>
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use lynshen_daemon::noise;
use serde_json::json;
use tungstenite::Message;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (relay, link) = (&args[1], &args[2]);
    let pair = link.split_once("#pair=").expect("pair link").1;
    let parts: Vec<&str> = pair.split('.').collect();
    let (host_id, host_key, code) = (
        parts[0],
        URL_SAFE_NO_PAD.decode(parts[1]).unwrap(),
        parts[2],
    );
    let (mut socket, _) =
        tungstenite::connect(format!("{relay}/connect?host={host_id}")).expect("relay connect");
    let (phone_key, _) = noise::generate_keypair().unwrap();
    let mut initiator = noise::initiator(&phone_key, &host_key).unwrap();
    let mut buffer = vec![0u8; noise::MAX_MESSAGE];
    let hello = json!({ "name": "relay-test-phone", "pair": code }).to_string();
    let written = initiator
        .write_message(hello.as_bytes(), &mut buffer)
        .unwrap();
    socket
        .send(Message::binary(buffer[..written].to_vec()))
        .unwrap();
    let reply = match socket.read().expect("handshake reply") {
        Message::Binary(bytes) => bytes,
        other => panic!("unexpected {other:?}"),
    };
    let read = initiator.read_message(&reply, &mut buffer).unwrap();
    println!("handshake: {}", String::from_utf8_lossy(&buffer[..read]));
    let mut transport = noise::Transport::new(initiator).unwrap();
    loop {
        let message = match socket.read().expect("first frame") {
            Message::Binary(bytes) => bytes,
            other => panic!("unexpected {other:?}"),
        };
        if let Some(frame) = transport.open(&message).expect("decrypt") {
            let text = String::from_utf8_lossy(&frame);
            println!(
                "first frame: {}",
                text.chars().take(160).collect::<String>()
            );
            break;
        }
    }
}
