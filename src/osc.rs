use std::collections::HashMap;
use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use rosc::OscType;
use tracing::{debug, warn};

#[derive(Default)]
pub struct OscDecl {
    pub receive_port: Option<u16>,
    pub send_targets: HashMap<String, SocketAddr>,
}

pub struct OscSender {
    socket: UdpSocket,
    pub targets: HashMap<String, SocketAddr>,
}

impl OscSender {
    pub fn new(targets: HashMap<String, SocketAddr>) -> Result<Self> {
        let socket =
            UdpSocket::bind("0.0.0.0:0").context("Failed to create OSC send socket")?;
        Ok(Self { socket, targets })
    }

    pub fn send(&self, target: &str, address: String, args: Vec<OscType>) -> Result<()> {
        let dest = self
            .targets
            .get(target)
            .ok_or_else(|| anyhow::anyhow!("Unknown OSC target: '{}'", target))?;
        let packet = rosc::OscPacket::Message(rosc::OscMessage { addr: address, args });
        let encoded = rosc::encoder::encode(&packet)
            .map_err(|e| anyhow::anyhow!("OSC encode error: {:?}", e))?;
        self.socket.send_to(&encoded, dest)?;
        Ok(())
    }
}

pub struct OscReceiver {
    running: Arc<AtomicBool>,
    _thread: std::thread::JoinHandle<()>,
}

impl OscReceiver {
    pub fn spawn<F>(port: u16, callback: F) -> Result<Self>
    where
        F: Fn(String, Vec<OscType>) + Send + 'static,
    {
        let socket = UdpSocket::bind(format!("0.0.0.0:{}", port))
            .with_context(|| format!("Failed to bind OSC receive port {}", port))?;
        socket.set_read_timeout(Some(Duration::from_millis(100)))?;

        let running = Arc::new(AtomicBool::new(true));
        let running_clone = Arc::clone(&running);

        let thread = std::thread::spawn(move || {
            let mut buf = [0u8; 65536];
            while running_clone.load(Ordering::Relaxed) {
                match socket.recv(&mut buf) {
                    Ok(n) => match rosc::decoder::decode_udp(&buf[..n]) {
                        Ok((_, packet)) => dispatch_packet(packet, &callback),
                        Err(e) => debug!("OSC decode error: {:?}", e),
                    },
                    Err(e)
                        if e.kind() == io::ErrorKind::WouldBlock
                            || e.kind() == io::ErrorKind::TimedOut =>
                    {
                        continue;
                    }
                    Err(e) => {
                        warn!("OSC receive error: {}", e);
                        break;
                    }
                }
            }
        });

        Ok(OscReceiver { running, _thread: thread })
    }
}

impl Drop for OscReceiver {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
    }
}

fn dispatch_packet<F: Fn(String, Vec<OscType>)>(packet: rosc::OscPacket, callback: &F) {
    match packet {
        rosc::OscPacket::Message(msg) => callback(msg.addr, msg.args),
        rosc::OscPacket::Bundle(bundle) => {
            for item in bundle.content {
                dispatch_packet(item, callback);
            }
        }
    }
}
