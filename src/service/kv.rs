// Copyright 2018-2019, Wayfair GmbH
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

use super::*;
use crate::{pubsub, storage, ServiceId};
use serde::{Deserialize, Serialize};

pub const KV_SERVICE: ServiceId = ServiceId(0);

#[derive(Serialize, Deserialize, Debug, Clone)]
pub(crate) enum PSEvent {
    Put {
        scope: u16,
        key: String,
        new: String,
        old: Option<String>,
    },
    Cas {
        scope: u16,
        key: String,
        new: String,
        old: String,
    },
    CasConflict {
        scope: u16,
        key: String,
        new: String,
        conflict: Option<String>,
    },
    Delete {
        scope: u16,
        key: String,
        old: Option<String>,
    },
}

pub struct Service {
    scope: u16,
}

impl Service {
    pub fn new(scope: u16) -> Self {
        Self { scope }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Event {
    Get {
        key: Vec<u8>,
    },
    Put {
        key: Vec<u8>,
        value: Vec<u8>,
    },
    Cas {
        key: Vec<u8>,
        check_value: Vec<u8>,
        store_value: Vec<u8>,
    },
    Delete {
        key: Vec<u8>,
    },
}

impl Event {
    pub fn get(key: Vec<u8>) -> Vec<u8> {
        serde_json::to_vec(&Event::Get { key }).unwrap()
    }
    pub fn put(key: Vec<u8>, value: Vec<u8>) -> Vec<u8> {
        serde_json::to_vec(&Event::Put { key, value }).unwrap()
    }
    pub fn cas(key: Vec<u8>, check_value: Vec<u8>, store_value: Vec<u8>) -> Vec<u8> {
        serde_json::to_vec(&Event::Cas {
            key,
            check_value,
            store_value,
        })
        .unwrap()
    }
    pub fn delete(key: Vec<u8>) -> Vec<u8> {
        serde_json::to_vec(&Event::Delete { key }).unwrap()
    }
}

impl<Storage> super::Service<Storage> for Service
where
    Storage: storage::Storage,
{
    fn execute(
        &mut self,
        storage: &Storage,
        pubsub: &pubsub::Channel,
        event: Vec<u8>,
    ) -> Result<Option<Vec<u8>>, Error> {
        match serde_json::from_slice(&event) {
            Ok(Event::Get { key }) => Ok(storage
                .get(self.scope, &key)
                .and_then(|v| String::from_utf8(v).ok())
                .and_then(|s| serde_json::to_vec(&serde_json::Value::String(s)).ok())),
            Ok(Event::Put { key, value }) => {
                let old = storage
                    .get(self.scope, &key)
                    .and_then(|value| String::from_utf8(value).ok());
                storage.put(self.scope, &key, &value);
                let msg = serde_json::to_value(&PSEvent::Put {
                    scope: self.scope,
                    key: String::from_utf8(key).unwrap_or_default(),
                    new: String::from_utf8(value).unwrap_or_default(),
                    old: old.clone(),
                })
                .unwrap();
                pubsub
                    .send(pubsub::Msg::Msg {
                        channel: "kv".into(),
                        msg: msg,
                    })
                    .unwrap();
                Ok(old.and_then(|s| serde_json::to_vec(&serde_json::Value::String(s)).ok()))
            }
            Ok(Event::Cas {
                key,
                check_value,
                store_value,
            }) => {
                if let Some(conflict) = storage.cas(self.scope, &key, &check_value, &store_value) {
                    let conflict = String::from_utf8(conflict).ok();
                    let msg = serde_json::to_value(&PSEvent::CasConflict {
                        scope: self.scope,
                        key: String::from_utf8(key).unwrap_or_default(),
                        new: String::from_utf8(store_value).unwrap_or_default(),
                        conflict: conflict.clone(),
                    })
                    .unwrap();
                    pubsub
                        .send(pubsub::Msg::Msg {
                            channel: "kv".into(),
                            msg: msg,
                        })
                        .unwrap();
                    Ok(conflict
                        .and_then(|s| serde_json::to_vec(&serde_json::Value::String(s)).ok()))
                } else {
                    let old = String::from_utf8(check_value).ok();
                    let new = String::from_utf8(store_value).ok();
                    let msg = serde_json::to_value(&PSEvent::Cas {
                        scope: self.scope,
                        key: String::from_utf8(key).unwrap_or_default(),
                        new: new.clone().unwrap_or_default(),
                        old: old.unwrap(),
                    })
                    .unwrap();
                    pubsub
                        .send(pubsub::Msg::Msg {
                            channel: "kv".into(),
                            msg: msg,
                        })
                        .unwrap();
                    Ok(new.and_then(|s| serde_json::to_vec(&serde_json::Value::String(s)).ok()))
                }
            }
            Ok(Event::Delete { key }) => Ok(storage
                .delete(self.scope, &key)
                .and_then(|v| String::from_utf8(v).ok())
                .and_then(|s| serde_json::to_vec(&serde_json::Value::String(s)).ok())),
            _ => Err(Error::UnknownEvent),
        }
    }
    fn is_local(&self, event: &[u8]) -> Result<bool, Error> {
        match serde_json::from_slice(&event) {
            Ok(Event::Get { .. }) => Ok(true),
            Ok(Event::Put { .. }) => Ok(false),
            Ok(Event::Cas { .. }) => Ok(false),
            Ok(Event::Delete { .. }) => Ok(false),
            _ => Err(Error::UnknownEvent),
        }
    }
}