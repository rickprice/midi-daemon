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
        self.send_to_addr(*dest, address, args)
    }

    /// Send to an ad-hoc address not in the named targets map.
    /// Used for subscriber replies and notifications.
    pub fn send_to_addr(&self, dest: SocketAddr, address: String, args: Vec<OscType>) -> Result<()> {
        let packet = rosc::OscPacket::Message(rosc::OscMessage { addr: address, args });
        let encoded = rosc::encoder::encode(&packet)
            .map_err(|e| anyhow::anyhow!("OSC encode error: {:?}", e))?;
        self.socket.send_to(&encoded, dest)?;
        Ok(())
    }
}

pub struct OscReceiver {
    running: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl OscReceiver {
    /// Spawn a UDP listener on `port`. The callback receives the sender's
    /// address so routes can reply to dynamic subscriber addresses.
    pub fn spawn<F>(port: u16, callback: F) -> Result<Self>
    where
        F: Fn(SocketAddr, String, Vec<OscType>) + Send + 'static,
    {
        let socket = UdpSocket::bind(format!("0.0.0.0:{}", port))
            .with_context(|| format!("Failed to bind OSC receive port {}", port))?;
        socket.set_read_timeout(Some(Duration::from_millis(100)))?;

        let running = Arc::new(AtomicBool::new(true));
        let running_clone = Arc::clone(&running);

        let thread = std::thread::spawn(move || {
            let mut buf = [0u8; 65536];
            while running_clone.load(Ordering::Relaxed) {
                match socket.recv_from(&mut buf) {
                    Ok((n, from)) => match rosc::decoder::decode_udp(&buf[..n]) {
                        Ok((_, packet)) => dispatch_packet(packet, &callback, from, 0),
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

        Ok(OscReceiver { running, thread: Some(thread) })
    }
}

impl Drop for OscReceiver {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        // Join the thread so the UDP socket is released before we return.
        // With the 100 ms read timeout this waits at most ~100 ms, ensuring a
        // hot-reload that re-binds the same port does not hit EADDRINUSE.
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

const MAX_BUNDLE_DEPTH: u8 = 16;

fn dispatch_packet<F: Fn(SocketAddr, String, Vec<OscType>)>(
    packet: rosc::OscPacket,
    callback: &F,
    from: SocketAddr,
    depth: u8,
) {
    match packet {
        rosc::OscPacket::Message(msg) => callback(from, msg.addr, msg.args),
        rosc::OscPacket::Bundle(bundle) => {
            if depth >= MAX_BUNDLE_DEPTH {
                warn!("OSC bundle nesting exceeds depth limit ({}); ignoring", MAX_BUNDLE_DEPTH);
                return;
            }
            for item in bundle.content {
                dispatch_packet(item, callback, from, depth + 1);
            }
        }
    }
}
