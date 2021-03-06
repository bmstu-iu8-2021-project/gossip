use std::io::Result;
use std::net::{SocketAddr, TcpStream};
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use crate::{config, speach, whisper};

pub struct Node {
    pub name: String,
    pub uuid: u32,
    pub stream: Option<TcpStream>,
    pub address: Option<SocketAddr>,
    pub iv: Vec<u8>,
}

impl Clone for Node {
    fn clone(&self) -> Self {
        Self {
            name: self.name.clone(),
            uuid: self.uuid.clone(),
            stream: match &self.stream {
                Some(stream) => {
                    if let Ok(copy) = stream.try_clone() {
                        Some(copy)
                    } else {
                        None
                    }
                }
                None => None,
            },
            address: self.address.clone(),
            iv: match &self.stream {
                Some(_) => self.iv.clone(),
                None => vec![0; 12],
            },
        }
    }
}
impl PartialEq for Node {
    fn eq(&self, other: &Node) -> bool {
        self.uuid == other.uuid
    }
}
impl Eq for Node {}
impl Node {
    pub fn to_string(&self) -> String {
        let node = json::object! {
            name: self.name.clone(),
            uuid: self.uuid,
            address: match self.address {
                Some(addr) => addr.to_string(),
                None => "None".to_string()
            },
        };
        json::stringify(node)
    }
    pub fn from_str(json_node: &str) -> json::Result<Node> {
        let parse_try = json::parse(json_node);
        match parse_try {
            Err(parse_error) => Err(parse_error),
            Ok(mut json_tree) => Ok(Node {
                name: {
                    let parse = json_tree["name"].take_string();
                    if parse.is_some() {
                        parse.unwrap()
                    } else {
                        return Err(json::Error::UnexpectedEndOfJson);
                    }
                },
                uuid: {
                    if let Some(id) = json_tree["uuid"].as_number() {
                        match std::convert::TryFrom::try_from(id) {
                            Ok(num) => num,
                            _ => 0,
                        }
                    } else {
                        return Err(json::Error::UnexpectedEndOfJson);
                    }
                },
                stream: None,
                address: {
                    let mut result = None;
                    if let Some(string) = json_tree["address"].take_string() {
                        if let Ok(res) = SocketAddr::from_str(&string) {
                            if res.is_ipv4() {
                                result = Some(res);
                            }
                        }
                    }
                    result
                },
                iv: vec![0; 12],
            }),
        }
    }
    pub fn with_address(name: String, uuid: u32, address: SocketAddr) -> Node {
        Node {
            name,
            uuid,
            stream: None,
            address: Some(address),
            iv: vec![0; 12],
        }
    }
    pub fn new(name: String, uuid: u32, stream: Option<TcpStream>) -> Node {
        Node {
            name,
            uuid,
            address: {
                let mut result = None;
                if let Some(ref stream) = stream {
                    if let Ok(addr) = stream.peer_addr() {
                        if addr.is_ipv4() {
                            result = Some(addr);
                        }
                    }
                }
                result
            },
            stream,
            iv: vec![0; 12],
        }
    }
}

fn negotiate_encryption(ctx: Arc<Mutex<config::State>>, stream: &mut TcpStream) -> Result<()> {
    let requests = speach::receive_greeting(stream);
    let mut encryption_request: Option<whisper::Message> = None;
    for i in requests {
        if i.msgtype == whisper::MessageType::EncryptionRequest {
            encryption_request = Some(i);
            break;
        }
    }
    if let Some(encryption_request) = encryption_request {
        let public_key = encryption_request.contents.as_bytes();
        let pkey_temp = openssl::pkey::PKey::public_key_from_pem(public_key).unwrap();
        let temp_encrypter = openssl::encrypt::Encrypter::new(&pkey_temp).unwrap();
        if speach::authenticate(&encryption_request.sender, stream) {
            // TODO: think about what to do when this fails
            speach::send_encryption_data(
                stream,
                &ctx.lock().unwrap().enc_key.clone(),
                &temp_encrypter,
            )?;
            // ugly
            speach::send_encryption_data(
                stream,
                &ctx.lock().unwrap().myself.iv.clone(),
                &temp_encrypter,
            )?;
        } else {
            // TODO: report invalid auth data to peer
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Auth failed",
            ));
        }
    }
    Ok(())
}

fn get_listener_addr(message: &whisper::Message, stream: &TcpStream) -> Option<SocketAddr> {
    let mut listen_addr = None;
    let stream_addr = stream.peer_addr().unwrap();
    if message.sender.address.is_some() {
        listen_addr = message.sender.address;
        listen_addr.as_mut().unwrap().set_ip(stream_addr.ip());
    }
    listen_addr
}

// different in that people connect to us directly here instead of us receiving gossip
pub fn receive_newcomer(ctx: Arc<Mutex<config::State>>, mut stream: TcpStream) -> Result<Node> {
    let message = speach::receive_greeting(&mut stream)?;
    log::info!(
        "New connection from {}\nWith greeting: {}\nAnd a node: {}",
        stream.peer_addr().unwrap(),
        message.to_string(),
        message.sender.to_string()
    );
    speach::send_data(
        &mut stream,
        ctx.lock().unwrap().announcement.to_string().as_bytes(),
    )?;
    //if !message.contents.contains("gossipless") {
    negotiate_encryption(ctx.clone(), &mut stream)?;
    //}
    // sender doesn't know it's address, so we tell everyone where from we got the
    // message
    let mut orig_sender = message.sender.clone();
    orig_sender.address = get_listener_addr(&message, &stream);
    ctx.lock().unwrap().network_info.push(orig_sender);
    // don't share the newcomer with the network
    // instead let the newcomer connect to everyone
    //ctx.lock().unwrap().gossiper_tx.send(message)
    let mut new_node = Node::new(
        message.sender.name.clone(),
        message.sender.uuid,
        Some(stream),
    );
    new_node.iv = ctx.lock().unwrap().myself.iv.clone();
    Ok(new_node)
}

pub fn request_missed(ctx: &mut config::State) -> Result<()> {
    let db = sled::open(match ctx.config.lock() {
        Ok(cfg) => cfg.stored_messages_filename.clone(),
        _ => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "db poisoned",
            ))
        }
    })
    // maybe don't crash here?
    .expect("Unable to open messages db");
    let last_msg_timestamp = match db.last().expect("unable to access db") {
        Some(pair) => {
            //i128::from_ne_bytes(&pair.0.to_vec()),
            let mut timestamp = [0u8; 16];
            timestamp.copy_from_slice(pair.0.as_ref());
            i128::from_ne_bytes(timestamp)
        }
        None => 0i128,
    };
    let mut request = whisper::Message::new(
        whisper::MessageType::MissedMessagesRequest,
        &ctx.myself,
        &last_msg_timestamp.to_string(),
        vec![ctx.myself.uuid],
        0,
        &vec![0u8; ctx.cipher.iv_len().unwrap_or_default()],
        std::time::SystemTime::now(),
    );
    openssl::rand::rand_bytes(&mut request.next_iv);
    // TODO: maybe choose random
    let encrypted = request
        .encrypt(
            &ctx.cipher,
            &ctx.enc_key,
            match ctx.connections.first() {
                Some(con) => &con.iv,
                None => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::AddrNotAvailable,
                        "no connections established",
                    ))
                }
            },
        )
        .unwrap();
    ctx.connections.first_mut().unwrap().iv = request.next_iv.clone();
    speach::send_data(
        ctx.connections
            .first_mut()
            .unwrap()
            .stream
            .as_mut()
            .unwrap(),
        &encrypted,
    )?;
    // this will not return anything now, but server will catch up later just fine
    // will be fixed when this becomes client-side
    Ok(())
}
