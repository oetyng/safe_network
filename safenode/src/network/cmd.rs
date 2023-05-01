// Copyright 2023 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under The General Public License (GPL), version 3.
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. Please review the Licences for the specific language governing
// permissions and limitations relating to use of the SAFE Network Software.

use super::{error::Error, MsgResponder, NetworkEvent, SwarmDriver};

use crate::{
    network::error::Result,
    protocol::messages::{QueryResponse, Request, Response},
};

use libp2p::{
    kad::{Record, RecordKey},
    multiaddr::Protocol,
    Multiaddr, PeerId,
};
use std::collections::{hash_map, HashSet};
use tokio::sync::oneshot;
use tracing::warn;
use xor_name::XorName;

/// Commands to send to the Swarm
#[derive(Debug)]
pub enum SwarmCmd {
    StartListening {
        addr: Multiaddr,
        sender: oneshot::Sender<Result<()>>,
    },
    Dial {
        peer_id: PeerId,
        peer_addr: Multiaddr,
        sender: oneshot::Sender<Result<()>>,
    },
    QueryForClosestPeers {
        xor_name: XorName,
        sender: oneshot::Sender<HashSet<PeerId>>,
    },
    GetClosestLocalPeers {
        xor_name: XorName,
        sender: oneshot::Sender<HashSet<PeerId>>,
    },
    SendRequest {
        req: Request,
        peer: PeerId,
        sender: oneshot::Sender<Result<Response>>,
    },
    SendResponse {
        resp: Response,
        channel: MsgResponder,
    },
    GetSwarmLocalState(oneshot::Sender<SwarmLocalState>),
    /// Put data to the Kad network as record
    PutProvidedDataAsRecord {
        record: Record,
    },
    /// Get data from the kademlia store
    GetData {
        key: RecordKey,
        sender: oneshot::Sender<QueryResponse>,
    },
}

/// Snapshot of information kept in the Swarm's local state
#[derive(Debug, Clone)]
pub struct SwarmLocalState {
    /// List of currently connected peers
    pub connected_peers: Vec<PeerId>,
    /// List of aaddresses the node is currently listening on
    pub listeners: Vec<Multiaddr>,
}

impl SwarmDriver {
    pub(crate) async fn handle_cmd(&mut self, cmd: SwarmCmd) -> Result<(), Error> {
        match cmd {
            SwarmCmd::GetData { key, sender } => {
                let query_id = self.swarm.behaviour_mut().kademlia.get_record(key);
                let _ = self.pending_query.insert(query_id, sender);
            }
            SwarmCmd::PutProvidedDataAsRecord { record } => {
                // TODO: when do we remove records. Do we need to?
                let _ = self
                    .swarm
                    .behaviour_mut()
                    .kademlia
                    .put_record(record, libp2p::kad::Quorum::All)?;
            }
            SwarmCmd::StartListening { addr, sender } => {
                let _ = match self.swarm.listen_on(addr) {
                    Ok(_) => sender.send(Ok(())),
                    Err(e) => sender.send(Err(e.into())),
                };
            }
            SwarmCmd::Dial {
                peer_id,
                peer_addr,
                sender,
            } => {
                if let hash_map::Entry::Vacant(e) = self.pending_dial.entry(peer_id) {
                    let _routing_update = self
                        .swarm
                        .behaviour_mut()
                        .kademlia
                        .add_address(&peer_id, peer_addr.clone());
                    match self
                        .swarm
                        .dial(peer_addr.with(Protocol::P2p(peer_id.into())))
                    {
                        Ok(()) => {
                            let _ = e.insert(sender);
                        }
                        Err(e) => {
                            let _ = sender.send(Err(e.into()));
                        }
                    }
                } else {
                    warn!("Already dialing peer.");
                }
            }
            SwarmCmd::QueryForClosestPeers { xor_name, sender } => {
                let key = xor_name.0.to_vec();
                let query_id = self.swarm.behaviour_mut().kademlia.get_closest_peers(key);
                let _ = self
                    .pending_get_closest_peers
                    .insert(query_id, (sender, Default::default()));
            }
            SwarmCmd::GetClosestLocalPeers { xor_name, sender } => {
                let key = xor_name.0.to_vec();
                let key = libp2p::kad::KBucketKey::new(key);
                let closest_peer: HashSet<PeerId> = self
                    .swarm
                    .behaviour_mut()
                    .kademlia
                    .get_closest_local_peers(&key)
                    .map(|k| *k.preimage())
                    .collect();
                let _ = sender.send(closest_peer);
            }
            SwarmCmd::SendRequest { req, peer, sender } => {
                // If `self` is the recipient, forward the request directly to our upper layer to
                // be handled.
                // `self` then handles the request and sends a response back again to itself.
                if peer == *self.swarm.local_peer_id() {
                    trace!("Sending request to self");
                    self.event_sender
                        .send(NetworkEvent::RequestReceived {
                            req,
                            channel: MsgResponder::FromSelf(sender),
                        })
                        .await?;
                } else {
                    trace!("Sending request to peer {peer:?}");
                    let request_id = self
                        .swarm
                        .behaviour_mut()
                        .request_response
                        .send_request(&peer, req);
                    let _ = self.pending_requests.insert(request_id, sender);
                }
            }
            SwarmCmd::SendResponse { resp, channel } => match channel {
                // If the response is for `self`, send it directly through the oneshot channel.
                MsgResponder::FromSelf(channel) => {
                    trace!("Sending response to self");
                    channel
                        .send(Ok(resp))
                        .map_err(|_| Error::InternalMsgChannelDropped)?;
                }
                MsgResponder::FromPeer(channel) => {
                    self.swarm
                        .behaviour_mut()
                        .request_response
                        .send_response(channel, resp)
                        .map_err(Error::OutgoingResponseDropped)?;
                }
            },
            SwarmCmd::GetSwarmLocalState(sender) => {
                let current_state = SwarmLocalState {
                    connected_peers: self.swarm.connected_peers().cloned().collect(),
                    listeners: self.swarm.listeners().cloned().collect(),
                };

                sender
                    .send(current_state)
                    .map_err(|_| Error::InternalMsgChannelDropped)?;
            }
        }
        Ok(())
    }
}
