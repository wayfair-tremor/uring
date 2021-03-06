// Copyright 2018-2020, Wayfair GmbH
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

pub mod placement;
use super::*;
use crate::{pubsub, storage, ServiceId};
use async_std::sync::Mutex;
use async_trait::async_trait;
use byteorder::{BigEndian, ReadBytesExt};
use bytes::BufMut;
use futures::SinkExt;
use serde_derive::{Deserialize, Serialize};
use std::io::Cursor;
use std::marker::PhantomData;
use uring_common::{MRingNodes, Relocations};
use ws_proto::PSMRing;
pub const ID: ServiceId = ServiceId(1);
use raft::RawNode;

pub struct Service<Placement>
where
    Placement: placement::Placement,
{
    marker: PhantomData<Placement>,
}

impl<Placement> Service<Placement>
where
    Placement: placement::Placement,
{
    pub fn new() -> Self {
        Self {
            marker: PhantomData::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Event {
    GetSize,
    SetSize { size: u64 },
    GetNodes,
    AddNode { node: String },
    RemoveNode { node: String },
}

impl Event {
    pub fn get_size() -> Vec<u8> {
        serde_json::to_vec(&Event::GetSize).unwrap()
    }
    pub fn set_size(size: u64) -> Vec<u8> {
        serde_json::to_vec(&Event::SetSize { size }).unwrap()
    }
    pub fn get_nodes() -> Vec<u8> {
        serde_json::to_vec(&Event::GetNodes).unwrap()
    }
    pub fn add_node(node: String) -> Vec<u8> {
        serde_json::to_vec(&Event::AddNode { node }).unwrap()
    }
    pub fn remove_node(node: String) -> Vec<u8> {
        serde_json::to_vec(&Event::RemoveNode { node }).unwrap()
    }
}

pub const RING_SIZE: &[u8; 9] = b"ring-size";
pub const NODES: &[u8; 10] = b"ring-nodes";

impl<Placement> Service<Placement>
where
    Placement: placement::Placement,
{
    async fn size<Storage>(&self, storage: &Storage) -> Option<u64>
    where
        Storage: storage::Storage,
    {
        storage
            .get(mring::ID.0 as u16, RING_SIZE)
            .await
            .and_then(|v| {
                let mut rdr = Cursor::new(v);
                rdr.read_u64::<BigEndian>().ok()
            })
    }

    async fn nodes<Storage>(&self, storage: &Storage) -> Option<MRingNodes>
    where
        Storage: storage::Storage,
    {
        storage
            .get(mring::ID.0 as u16, NODES)
            .await
            .and_then(|v| serde_json::from_slice(&v).ok())
    }
}

#[async_trait]
impl<Storage, Placement> super::Service<Storage> for Service<Placement>
where
    Storage: storage::Storage + Send + Sync + 'static,
    Placement: placement::Placement + Send + Sync,
{
    async fn execute(
        &mut self,
        node: &Mutex<RawNode<Storage>>,
        pubsub: &mut pubsub::Channel,
        event: Vec<u8>,
    ) -> Result<(u16, Vec<u8>), Error> {
        let raft_node = node.try_lock().unwrap();
        let storage = raft_node.store();
        match serde_json::from_slice(&event) {
            Ok(Event::GetSize) => {
                if let Some(size) = self.size(storage).await {
                    Ok((
                        200u16,
                        serde_json::to_vec(&serde_json::Value::from(size)).unwrap(),
                    ))
                } else {
                    Ok((
                        404u16,
                        serde_json::to_vec(&serde_json::Value::Null).unwrap(),
                    ))
                }
            }
            Ok(Event::SetSize { size }) => {
                if let Some(size) = self.size(storage).await {
                    return Ok((
                        409,
                        serde_json::to_vec(&serde_json::Value::from(size)).unwrap(),
                    ));
                }

                let mut data: Vec<u8> = vec![0; 8];
                data.put_u64(size);
                storage.put(mring::ID.0 as u16, RING_SIZE, &data).await;

                pubsub
                    .send(pubsub::Msg::new(
                        "mring",
                        PSMRing::SetSize {
                            size,
                            strategy: Placement::name(),
                        },
                    ))
                    .await
                    .unwrap();

                Ok((
                    200,
                    serde_json::to_vec(&serde_json::Value::from(size)).unwrap(),
                ))
            }
            Ok(Event::GetNodes) => Ok((200, storage.get(mring::ID.0 as u16, NODES).await.unwrap())),
            Ok(Event::AddNode { node }) => {
                let size = if let Some(size) = self.size(&*storage).await {
                    size
                } else {
                    return Ok((412, serde_json::to_vec(&"mring size not set").unwrap()));
                };
                let next = if let Some(current) = self.nodes(&*storage).await {
                    let (next, relocations) = Placement::add_node(size, current, node.clone());
                    pubsub
                        .send(pubsub::Msg::new(
                            "mring",
                            PSMRing::NodeAdded {
                                node,
                                strategy: Placement::name(),
                                next: next.clone(),
                                relocations,
                            },
                        ))
                        .await
                        .unwrap();
                    next
                } else {
                    let next = Placement::new(size, node.clone());

                    pubsub
                        .send(pubsub::Msg::new(
                            "mring",
                            PSMRing::NodeAdded {
                                node,
                                strategy: Placement::name(),
                                next: next.clone(),
                                relocations: Relocations::new(),
                            },
                        ))
                        .await
                        .unwrap();
                    next
                };
                let next = serde_json::to_vec(&next).unwrap();
                storage.put(mring::ID.0 as u16, NODES, &next).await;
                Ok((200, next))
            }
            Ok(Event::RemoveNode { node }) => {
                let size = if let Some(size) = self.size(&*storage).await {
                    size
                } else {
                    return Ok((412, serde_json::to_vec(&"mring size not set").unwrap()));
                };

                if let Some(current) = self.nodes(&*storage).await {
                    let (next, relocations) = Placement::remove_node(size, current, node.clone());
                    pubsub
                        .send(pubsub::Msg::new(
                            "mring",
                            PSMRing::NodeRemoved {
                                node,
                                strategy: Placement::name(),
                                next: next.clone(),
                                relocations,
                            },
                        ))
                        .await
                        .unwrap();
                    let next = serde_json::to_vec(&next).unwrap();
                    storage.put(mring::ID.0 as u16, NODES, &next).await;
                    Ok((200, next))
                } else {
                    return Ok((
                        412,
                        serde_json::to_vec(&"mring does not exist yet").unwrap(),
                    ));
                }
            }
            Err(_) => Err(Error::UnknownEvent),
        }
    }
    fn is_local(&self, event: &[u8]) -> Result<bool, Error> {
        match serde_json::from_slice(&event) {
            Ok(Event::GetSize) => Ok(true),
            Ok(Event::GetNodes) => Ok(true),
            Ok(Event::SetSize { .. }) => Ok(false),
            Ok(Event::AddNode { .. }) => Ok(false),
            Ok(Event::RemoveNode { .. }) => Ok(false),
            Err(_) => Err(Error::UnknownEvent),
        }
    }
}
