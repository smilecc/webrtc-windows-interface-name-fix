#[cfg(test)]
mod allocation_test;

pub mod allocation_manager;
pub mod channel_bind;
pub mod five_tuple;
pub mod permission;

use crate::errors::*;
use crate::proto::{channum::*, *};
use channel_bind::*;
use five_tuple::*;
use permission::*;

use util::{Conn, Error};

use tokio::sync::{mpsc, Mutex};
use tokio::time::{Duration, Instant};

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{atomic::AtomicBool, atomic::Ordering, Arc};

// Allocation is tied to a FiveTuple and relays traffic
// use create_allocation and get_allocation to operate
pub struct Allocation {
    relay_addr: SocketAddr,
    protocol: Protocol,
    //TODO: TurnSocket: Box<dyn Conn>,
    //TODO: RelaySocket: Box<dyn Conn>,
    five_tuple: FiveTuple,
    permissions: Arc<Mutex<HashMap<String, Permission>>>,
    channel_bindings: Arc<Mutex<HashMap<ChannelNumber, ChannelBind>>>,
    pub(crate) allocations: Option<Arc<Mutex<HashMap<String, Allocation>>>>,
    reset_tx: Option<mpsc::Sender<Duration>>,
    timer_expired: Arc<AtomicBool>,
    closed: bool, // Option<mpsc::Receiver<()>>,
}

fn addr2ipfingerprint(addr: &SocketAddr) -> String {
    addr.ip().to_string()
}

impl Allocation {
    // creates a new instance of NewAllocation.
    pub fn new(_turn_socket: impl Conn, five_tuple: FiveTuple) -> Self {
        Allocation {
            relay_addr: SocketAddr::new(Ipv4Addr::new(0, 0, 0, 0).into(), 0),
            protocol: PROTO_UDP,
            //TODO: TurnSocket:  turnSocket,
            five_tuple,
            permissions: Arc::new(Mutex::new(HashMap::new())),
            channel_bindings: Arc::new(Mutex::new(HashMap::new())),
            allocations: None,
            reset_tx: None,
            timer_expired: Arc::new(AtomicBool::new(false)),
            closed: false,
        }
    }

    // has_permission gets the Permission from the allocation
    pub async fn has_permission(&self, addr: &SocketAddr) -> bool {
        let permissions = self.permissions.lock().await;
        permissions.get(&addr2ipfingerprint(addr)).is_some()
    }

    // add_permission adds a new permission to the allocation
    pub async fn add_permission(&self, mut p: Permission) {
        let fingerprint = addr2ipfingerprint(&p.addr);

        {
            let permissions = self.permissions.lock().await;
            if let Some(existed_permission) = permissions.get(&fingerprint) {
                existed_permission.refresh(PERMISSION_TIMEOUT).await;
                return;
            }
        }

        p.permissions = Some(Arc::clone(&self.permissions));
        p.start(PERMISSION_TIMEOUT).await;

        {
            let mut permissions = self.permissions.lock().await;
            permissions.insert(fingerprint, p);
        }
    }

    // remove_permission removes the net.Addr's fingerprint from the allocation's permissions
    pub async fn remove_permission(&self, addr: &SocketAddr) -> bool {
        let mut permissions = self.permissions.lock().await;
        permissions.remove(&addr2ipfingerprint(addr)).is_some()
    }

    // add_channel_bind adds a new ChannelBind to the allocation, it also updates the
    // permissions needed for this ChannelBind
    pub async fn add_channel_bind(
        &self,
        mut c: ChannelBind,
        lifetime: Duration,
    ) -> Result<(), Error> {
        {
            if let Some(addr) = self.get_channel_addr(&c.number).await {
                if addr != c.peer {
                    return Err(ERR_SAME_CHANNEL_DIFFERENT_PEER.to_owned());
                }
            }

            if let Some(number) = self.get_channel_number(&c.peer).await {
                if number != c.number {
                    return Err(ERR_SAME_CHANNEL_DIFFERENT_PEER.to_owned());
                }
            }
        }

        {
            let channel_bindings = self.channel_bindings.lock().await;
            if let Some(cb) = channel_bindings.get(&c.number) {
                cb.refresh(lifetime).await;

                // Channel binds also refresh permissions.
                self.add_permission(Permission::new(cb.peer)).await;

                return Ok(());
            }
        }

        let peer = c.peer;

        // Add or refresh this channel.
        c.channel_bindings = Some(Arc::clone(&self.channel_bindings));
        c.start(lifetime).await;

        {
            let mut channel_bindings = self.channel_bindings.lock().await;
            channel_bindings.insert(c.number, c);
        }

        // Channel binds also refresh permissions.
        self.add_permission(Permission::new(peer)).await;

        Ok(())
    }

    // remove_channel_bind removes the ChannelBind from this allocation by id
    pub async fn remove_channel_bind(&self, number: ChannelNumber) -> bool {
        let mut channel_bindings = self.channel_bindings.lock().await;
        channel_bindings.remove(&number).is_some()
    }

    // get_channel_addr gets the ChannelBind's addr
    pub async fn get_channel_addr(&self, number: &ChannelNumber) -> Option<SocketAddr> {
        let channel_bindings = self.channel_bindings.lock().await;
        if let Some(cb) = channel_bindings.get(number) {
            Some(cb.peer)
        } else {
            None
        }
    }

    // GetChannelByAddr gets the ChannelBind's number from this allocation by net.Addr
    pub async fn get_channel_number(&self, addr: &SocketAddr) -> Option<ChannelNumber> {
        let channel_bindings = self.channel_bindings.lock().await;
        for cb in channel_bindings.values() {
            if cb.peer == *addr {
                return Some(cb.number);
            }
        }
        None
    }

    // Close closes the allocation
    pub async fn close(&mut self) -> Result<(), Error> {
        if self.closed {
            return Ok(());
        }

        self.closed = true;
        self.stop();

        {
            let mut permissions = self.permissions.lock().await;
            for p in permissions.values_mut() {
                p.stop();
            }
        }

        {
            let mut channel_bindings = self.channel_bindings.lock().await;
            for c in channel_bindings.values_mut() {
                c.stop();
            }
        }

        Ok(())
    }

    pub async fn start(&mut self, lifetime: Duration) {
        let (reset_tx, mut reset_rx) = mpsc::channel(1);
        self.reset_tx = Some(reset_tx);

        let allocations = self.allocations.clone();
        let five_tuple = self.five_tuple.clone();
        let timer_expired = Arc::clone(&self.timer_expired);

        tokio::spawn(async move {
            let timer = tokio::time::sleep(lifetime);
            tokio::pin!(timer);
            let mut done = false;

            while !done {
                tokio::select! {
                    _ = &mut timer => {
                        if let Some(allocs) = &allocations{
                            let mut a = allocs.lock().await;
                            a.remove(&five_tuple.fingerprint());
                        }
                        done = true;
                    },
                    result = reset_rx.recv() => {
                        if let Some(d) = result {
                            timer.as_mut().reset(Instant::now() + d);
                        } else {
                            done = true;
                        }
                    },
                }
            }

            timer_expired.store(true, Ordering::SeqCst);
        });
    }

    pub fn stop(&mut self) -> bool {
        let expired = self.reset_tx.is_none() || self.timer_expired.load(Ordering::SeqCst);
        self.reset_tx.take();
        expired
    }

    // Refresh updates the allocations lifetime
    pub async fn refresh(&self, lifetime: Duration) {
        if let Some(tx) = &self.reset_tx {
            let _ = tx.send(lifetime).await;
        }
    }
}
/*
//  https://tools.ietf.org/html/rfc5766#section-10.3
//  When the server receives a UDP datagram at a currently allocated
//  relayed transport address, the server looks up the allocation
//  associated with the relayed transport address.  The server then
//  checks to see whether the set of permissions for the allocation allow
//  the relaying of the UDP datagram as described in Section 8.
//
//  If relaying is permitted, then the server checks if there is a
//  channel bound to the peer that sent the UDP datagram (see
//  Section 11).  If a channel is bound, then processing proceeds as
//  described in Section 11.7.
//
//  If relaying is permitted but no channel is bound to the peer, then
//  the server forms and sends a Data indication.  The Data indication
//  MUST contain both an XOR-PEER-ADDRESS and a DATA attribute.  The DATA
//  attribute is set to the value of the 'data octets' field from the
//  datagram, and the XOR-PEER-ADDRESS attribute is set to the source
//  transport address of the received UDP datagram.  The Data indication
//  is then sent on the 5-tuple associated with the allocation.

const rtpMTU = 1500

func (a *Allocation) packetHandler(m *Manager) {
    buffer := make([]byte, rtpMTU)

    for {
        n, srcAddr, err := a.RelaySocket.ReadFrom(buffer)
        if err != nil {
            m.delete_allocation(a.five_tuple)
            return
        }

        a.log.Debugf("relay socket %s received %d bytes from %s",
            a.RelaySocket.LocalAddr().String(),
            n,
            srcAddr.String())

        if channel := a.GetChannelByAddr(srcAddr); channel != nil {
            channelData := &proto.ChannelData{
                Data:   buffer[:n],
                number: channel.number,
            }
            channelData.Encode()

            if _, err = a.TurnSocket.WriteTo(channelData.Raw, a.five_tuple.src_addr); err != nil {
                a.log.Errorf("Failed to send ChannelData from allocation %v %v", srcAddr, err)
            }
        } else if p := a.get_permission(srcAddr); p != nil {
            udpAddr := srcAddr.(*net.UDPAddr)
            peerAddressAttr := proto.PeerAddress{IP: udpAddr.IP, Port: udpAddr.Port}
            dataAttr := proto.Data(buffer[:n])

            msg, err := stun.Build(stun.TransactionID, stun.NewType(stun.MethodData, stun.ClassIndication), peerAddressAttr, dataAttr)
            if err != nil {
                a.log.Errorf("Failed to send DataIndication from allocation %v %v", srcAddr, err)
            }
            a.log.Debugf("relaying message from %s to client at %s",
                srcAddr.String(),
                a.five_tuple.src_addr.String())
            if _, err = a.TurnSocket.WriteTo(msg.Raw, a.five_tuple.src_addr); err != nil {
                a.log.Errorf("Failed to send DataIndication from allocation %v %v", srcAddr, err)
            }
        } else {
            a.log.Infof("No Permission or Channel exists for %v on allocation %v", srcAddr, a.relay_addr.String())
        }
    }
}
*/
