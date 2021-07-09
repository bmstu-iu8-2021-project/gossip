use rand::Rng;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::mpsc;
use std::thread;

pub mod neighborhood;
pub mod whisper;
pub mod speach;

fn spawn_listener() -> (mpsc::Receiver<TcpStream>, SocketAddr) {
    let port: u16 = rand::thread_rng().gen_range(7000..50000);
    let mut local_address = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), port);
    let mut listener = TcpListener::bind(local_address);
    while listener.is_err() {
        local_address.set_port(rand::thread_rng().gen_range(7000..50000));
        listener = TcpListener::bind(local_address);
    }
    let listener = listener.unwrap();
    let (listener_tx, listener_rx) = mpsc::channel();
    let _listener_thread = thread::spawn(move || {
        for i in listener.incoming() {
            if let Ok(new_connection) = i {
                listener_tx
                    .send(new_connection)
                    .expect("Unable to send new connection to server!");
            }
        }
    });
    println!("Listening at {}", local_address);
    (listener_rx, local_address)
}

pub fn spawn_server(
    client_name: String,
    init_nodes: Vec<String>,
) -> (mpsc::Sender<String>, mpsc::Receiver<String>) {
    let (listener_rx, local_address) = spawn_listener();
    let uuid: u32 = rand::thread_rng().gen();
    println!("I am {}", uuid);
    let myself = neighborhood::Node::new(&client_name, uuid, &local_address);
    let announcement = crate::whisper::Message::new(
        crate::whisper::MessageType::NewMember,
        &myself,
        &json::stringify(json::object! {
            aquaintance: [ uuid ],
        }),
        crate::whisper::Encryption::None,
    );
    let mut connections = speach::initial_connections(init_nodes, &announcement);
    let (tx, client_rx) = mpsc::channel();
    let (client_tx, rx) = mpsc::channel();
    let _server_thread = thread::spawn(move || loop {
        // mailbox
        let mut newcomer_mailbox: Vec<crate::whisper::Message> = Vec::new();
        let mut mailbox = Vec::<crate::whisper::Message>::new();
        for i in connections.iter_mut() {
            if let Some(stream) = i.1.as_mut() {
                let connection_messages = speach::receive_messages(stream);
                mailbox.extend(connection_messages);
            }
        }
        for i in mailbox.iter_mut() {
            match i.msgtype {
                crate::whisper::MessageType::Text => {
                    tx.send(i.format())
                        .expect("Unable to send message to client!");
                }
                crate::whisper::MessageType::NewMember => {
                    let mut message_contents = json::parse(i.contents.as_str()).unwrap();
                    message_contents["aquaintance"].push(myself.uuid).unwrap();
                    newcomer_mailbox.push(crate::whisper::Message::new(
                        i.msgtype,
                        &i.sender,
                        &json::stringify(message_contents),
                        i.encryption,
                    ));
                }
            }
        }

        // greet the spoken and tell him not to worry introducing me
        for i in newcomer_mailbox.iter() {
            let newcomer = i.sender.clone();
            let mut announcement = announcement.clone();
            let mut contents = json::parse(announcement.contents.as_str()).unwrap();
            contents.insert("gossipless", true);
            announcement.contents = json::stringify(contents);
            println!("Was told to connect to {}", newcomer.address);
            let mut greeted = false;
            for i in connections.iter() {
                if i.0.uuid == newcomer.uuid { greeted = true; break; }
            }
            if greeted { return; }
            if let Ok(node) = speach::init_connection(&newcomer.address, &announcement) {
                connections.push(node);
            } else {
                connections.push((newcomer, None));
            }
        }
        // don't propagate the gossip (for now)
        newcomer_mailbox.clear();
        // mailman
        while let Ok(msg_text) = rx.try_recv() {
            let msg = crate::whisper::Message::new(
                crate::whisper::MessageType::Text,
                &myself,
                &msg_text,
                crate::whisper::Encryption::None,
            );
            for i in connections.iter_mut() {
                if let Some(stream) = i.1.as_mut() {
                    speach::send_message(stream, &msg);
                }
            }
        }
        // direct connections
        // create gossip
        let mut newcomer_mailbox: Vec<crate::whisper::Message> = Vec::new();
        while let Ok(mut new_connection) = listener_rx.try_recv() {
            if let Ok(mut message) = speach::receive_greeting(&mut new_connection) {
                println!(
                    "New connection from {}",
                    new_connection.peer_addr().unwrap()
                );
                new_connection.set_nonblocking(true).unwrap();
                speach::send_message(&mut new_connection, &announcement);
                // sender doesn't know it's adress, so we tell everyone where from we got the
                // message
                message.sender.address.set_ip(new_connection.peer_addr().unwrap().ip());
                let mut message_contents = json::parse(message.contents.as_str()).unwrap();
                if !message_contents.has_key("gossipless") {
                    message_contents["aquaintance"].push(uuid).unwrap();
                    newcomer_mailbox.push(crate::whisper::Message::new(
                        message.msgtype,
                        &message.sender,
                        &json::stringify(message_contents),
                        message.encryption,
                    ));
                }
                connections.push((message.sender.clone(), Some(new_connection)));
            }
        }
        // spread the gossip (for now to everyone)
        for i in newcomer_mailbox.iter() {
            let message_contents = json::parse(i.contents.clone().as_str()).unwrap();
            for j in connections.iter_mut() {
                println!(
                    "Greeting from {} is aquainted with {}",
                    i.sender.name, message_contents["aquaintance"]
                );

                if !message_contents["aquaintance"].contains(j.0.uuid) && j.1.is_some() {
                    if let Some(stream) = j.1.as_mut() {
                    println!(
                        "Sending greeting to uuid {}, address {}",
                        j.0.uuid,
                        stream.peer_addr().unwrap()
                    );
                    speach::send_message(stream, i);
                    }
                }
            }
        }
        thread::sleep(std::time::Duration::from_millis(200));
    });
    (client_tx, client_rx)
}
