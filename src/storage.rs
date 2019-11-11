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

// inspired by https://github.com/LucioFranco/kv/blob/master/src/storage.rs

use crate::KV;
use protobuf::Message;
use raft::eraftpb::{ConfState, HardState};
use raft::eraftpb::{Entry, Snapshot};
use raft::storage::{MemStorage, RaftState, Storage as ReadStorage};
use raft::{Error as RaftError, Result as RaftResult, StorageError};
use regex::Regex;
use std::borrow::Borrow;
/// The missing storage trait from raft-rs ...
pub trait WriteStorage: ReadStorage {
    fn append(&self, entries: &[Entry]) -> RaftResult<()>;
    fn apply_snapshot(&mut self, snapshot: Snapshot) -> RaftResult<()>;
    fn set_conf_state(&mut self, cs: ConfState) -> RaftResult<()>;
    fn set_hard_state(&mut self, commit: u64, term: u64) -> RaftResult<()>;
}

/*
pub trait Storable: ?Sized {
    fn encode(self) -> Bytes;
    fn decode(bytes: Bytes) -> Option<Self>;
}
*/
#[derive(Default)]
pub struct URMemStorage {
    backend: MemStorage,
}

#[allow(dead_code)]
impl URMemStorage {
    pub fn new_with_conf_state(_id: u64, state: ConfState) -> Self {
        Self {
            backend: MemStorage::new_with_conf_state(state),
        }
    }
    pub fn new(_id: u64) -> Self {
        Self {
            backend: MemStorage::new(),
        }
    }
}

impl WriteStorage for URMemStorage {
    fn apply_snapshot(&mut self, snapshot: Snapshot) -> RaftResult<()> {
        self.backend.wl().apply_snapshot(snapshot)
    }

    fn append(&self, entries: &[Entry]) -> RaftResult<()> {
        self.backend.wl().append(entries)
    }

    fn set_conf_state(&mut self, cs: ConfState) -> RaftResult<()> {
        self.backend.wl().set_conf_state(cs);
        Ok(())
    }

    fn set_hard_state(&mut self, commit: u64, term: u64) -> RaftResult<()> {
        let mut s = self.backend.wl();
        s.mut_hard_state().commit = commit;
        s.mut_hard_state().term = term;
        Ok(())
    }
}

impl ReadStorage for URMemStorage {
    fn first_index(&self) -> RaftResult<u64> {
        self.backend.first_index()
    }

    fn last_index(&self) -> RaftResult<u64> {
        self.backend.last_index()
    }

    fn term(&self, idx: u64) -> RaftResult<u64> {
        self.backend.term(idx)
    }

    fn initial_state(&self) -> RaftResult<RaftState> {
        self.backend.initial_state()
    }
    fn entries(
        &self,
        low: u64,
        high: u64,
        max_size: impl Into<Option<u64>>,
    ) -> RaftResult<Vec<Entry>> {
        self.backend.entries(low, high, max_size)
    }
    fn snapshot(&self, request_index: u64) -> RaftResult<Snapshot> {
        self.backend.snapshot(request_index)
    }
}

use rocksdb::{Direction, IteratorMode, WriteBatch, DB};

const CONF_STATE: &'static [u8; 16] = b"\0\0\0\0\0\0\0ConfState";
const HARD_STATE: &'static [u8; 16] = b"\0\0\0\0\0\0\0HardState";

//#[derive(Default)]
pub struct URRocksStorage {
    backend: DB,
    conf_state: Option<ConfState>,
}

impl URRocksStorage {
    pub fn new_with_conf_state(id: u64, state: ConfState) -> Self {
        let mut db = Self::new(id);

        db.set_conf_state(state).unwrap();
        db.set_hard_state(1, 1).unwrap();
        db
    }
    pub fn new(id: u64) -> Self {
        let backend = DB::open_default(&format!("raft-rocks-{}", id)).unwrap();
        Self {
            backend,
            conf_state: None,
        }
    }

    fn get_hard_state(&self) -> HardState {
        let mut hs = HardState::new();
        if let Ok(Some(data)) = self.backend.get(&HARD_STATE) {
            hs.merge_from_bytes(&data).unwrap();
        };
        hs
    }
    fn get_conf_state(&self) -> ConfState {
        let mut cs = ConfState::new();
        if let Ok(Some(data)) = self.backend.get(&CONF_STATE) {
            cs.merge_from_bytes(&data).unwrap();
        };
        cs
    }
    fn clear_log(&self) {
        self.clear_log_to(u64::max_value());
    }

    fn clear_log_to(&self, before: u64) {
        let before = make_log_key(before);
        self.backend
            .iterator(IteratorMode::From(&LOW_INDEX, Direction::Forward))
            .take_while(|(k, _)| {
                let k: &[u8] = k.borrow();
                k <= &HIGH_INDEX[..]
            })
            .take_while(|(k, _)| {
                let k: &[u8] = k.borrow();
                k <= &before[..]
            })
            .for_each(|(k, _)| self.backend.delete(&k).unwrap());
    }

    fn clear_data(&self) {
        self.backend
            .iterator(IteratorMode::From(&LOW_DATA, Direction::Forward))
            .take_while(|(k, _)| {
                let k: &[u8] = k.borrow();
                // NOTE: we got to use < here since we don't have a fixed "last key" for data as it's not fixed size
                // HIGH_DATA is technically LOW of the next segment
                k < &HIGH_DATA[..]
            })
            .for_each(|(k, _)| self.backend.delete(&k).unwrap());
    }

    pub fn get(&self, key: &str) -> Option<String> {
        let key = make_data_key(key);
        self.backend
            .get(key)
            .unwrap()
            .map(|v| String::from_utf8_lossy(&v).to_string())
    }
    pub fn put(&self, key: &str, value: String) {
        let key = make_data_key(key);
        self.backend.put(key, value).unwrap();
    }

    pub fn data_snapshot(&self) -> Vec<u8> {
        self.backend
            .iterator(IteratorMode::From(&LOW_DATA, Direction::Forward))
            .take_while(|(k, _)| {
                let k: &[u8] = k.borrow();
                k < &HIGH_DATA[..]
            })
            .map(|(k, v)| {
                serde_json::to_string(&KV {
                    key: std::str::from_utf8(&k[8..]).unwrap().into(),
                    value: std::str::from_utf8(&v).unwrap().into(),
                })
                .unwrap()
            })
            .collect::<Vec<String>>()
            .join("\n")
            .into_bytes()
    }

    pub fn apply_data_snapshot(&self, data: Vec<u8>) {
        self.clear_data();

        for kv in data.split(|c| *c == b'\n') {
            if let Ok(kv) = serde_json::from_slice::<KV>(&kv) {
                self.put(&kv.key, kv.value);
            }
        }
    }
}

impl WriteStorage for URRocksStorage {
    fn apply_snapshot(&mut self, mut snapshot: Snapshot) -> RaftResult<()> {
        let mut meta = snapshot.take_metadata();
        self.apply_data_snapshot(snapshot.take_data());
        let term = meta.term;
        let index = meta.index;

        let first_index = self.first_index().unwrap();
        // Make sure the snapshot is not prior to our first log
        if first_index > index {
            return Err(RaftError::Store(StorageError::SnapshotOutOfDate));
        }

        self.set_hard_state(index, term)?;
        self.set_conf_state(meta.take_conf_state())?;
        // From Mem node do we only want to clear up to index?
        self.clear_log();
        Ok(())
    }

    fn append(&self, entries: &[Entry]) -> RaftResult<()> {
        if entries.is_empty() {
            return Ok(());
        }
        let mut batch = WriteBatch::default();
        for entry in entries {
            let key = make_log_key(entry.index);
            let data = entry.write_to_bytes()?;
            batch.put(&key, &data).unwrap();
        }
        self.backend.write(batch).unwrap();
        self.backend.flush().unwrap();

        Ok(())
    }

    fn set_conf_state(&mut self, cs: ConfState) -> RaftResult<()> {
        self.conf_state = Some(cs.clone());

        let data = cs.write_to_bytes()?;
        self.backend.put(&CONF_STATE, &data).unwrap();
        self.backend.flush().unwrap();
        Ok(())
    }

    fn set_hard_state(&mut self, commit: u64, term: u64) -> RaftResult<()> {
        let mut hs = HardState::new();
        hs.commit = commit;
        hs.term = term;
        let data = hs.write_to_bytes()?;
        self.backend.put(&HARD_STATE, &data).unwrap();
        self.clear_log_to(commit);
        self.backend.flush().unwrap();
        Ok(())
    }
}

impl ReadStorage for URRocksStorage {
    fn initial_state(&self) -> RaftResult<RaftState> {
        let hard_state = self.get_hard_state();
        if hard_state == HardState::default() {
            return Ok(RaftState::new(hard_state, ConfState::default()));
        };
        let conf_state = self.get_conf_state();
        Ok(RaftState::new(hard_state, conf_state))
    }

    fn entries(
        &self,
        low: u64,
        high: u64,
        max_size: impl Into<Option<u64>>,
    ) -> RaftResult<Vec<Entry>> {
        use std::cmp::max;
        let first_index = self.first_index().unwrap();
        if low < first_index {
            return Err(RaftError::Store(StorageError::Compacted));
        }
        let last_index = self.last_index().unwrap() + 1;
        if high > last_index {
            panic!("index out of bound (last: {}, high: {})", last_index, high);
        }

        let low_key = make_log_key(low);
        let iter = self
            .backend
            .iterator(IteratorMode::From(&low_key, Direction::Forward))
            .take_while(|(k, _)| {
                let k: &[u8] = k.borrow();
                k <= &HIGH_INDEX[..]
            })
            .map(|(_, v)| {
                let mut e = Entry::new();
                e.merge_from_bytes(&v).unwrap();
                e
            })
            .take_while(|e| e.index < high);

        if let Some(max_size) = max_size.into() {
            //FIXME use max_size as size not count
            Ok(iter.take(max(max_size, 1) as usize).collect())
        } else {
            Ok(iter.collect())
        }
    }

    fn term(&self, idx: u64) -> RaftResult<u64> {
        let first_index = self.first_index().unwrap();

        let hs = self.get_hard_state();

        if idx == hs.commit {
            return Ok(hs.term);
        }

        if idx < first_index {
            return Err(RaftError::Store(StorageError::Compacted));
        }

        let key = make_log_key(idx);
        self.backend
            .get(&key)
            .unwrap()
            .map(|v| {
                let mut e = Entry::new();
                e.merge_from_bytes(&v).unwrap();
                e.term
            })
            .ok_or(RaftError::Store(StorageError::Unavailable))
    }

    fn first_index(&self) -> RaftResult<u64> {
        let first = self
            .backend
            .iterator(IteratorMode::From(&LOW_INDEX, Direction::Forward))
            .take_while(|(k, _)| {
                let k: &[u8] = k.borrow();
                k <= &HIGH_INDEX[..]
            })
            .next()
            .map(|(_, v)| {
                let mut e = Entry::new();
                e.merge_from_bytes(&v).unwrap();
                e.index
            })
            .unwrap_or_else(|| self.get_hard_state().commit + 1);
        Ok(first)
    }

    fn last_index(&self) -> RaftResult<u64> {
        let last = self
            .backend
            .iterator(IteratorMode::From(&HIGH_INDEX, Direction::Reverse))
            .take_while(|(k, _)| {
                let k: &[u8] = k.borrow();
                k >= &LOW_INDEX[..]
            })
            .next()
            .map(|(_k, v)| {
                let mut e = Entry::new();
                e.merge_from_bytes(&v).unwrap();
                e.index
            })
            .unwrap_or_else(|| self.get_hard_state().commit);
        Ok(last)
    }

    fn snapshot(&self, request_index: u64) -> RaftResult<Snapshot> {
        let mut snapshot = Snapshot::default();
        let hs = self.get_hard_state();
        // Use the latest applied_idx to construct the snapshot.
        let applied_idx = hs.commit;
        let term = hs.term;
        snapshot.set_data(self.data_snapshot());
        let meta = snapshot.mut_metadata();
        meta.index = applied_idx;
        meta.term = term;

        meta.set_conf_state(self.get_conf_state().clone());
        // https://github.com/tikv/raft-rs/blob/3f5171a9f833679cb40437ca47031eb0e9f4aa3e/src/storage.rs#L494
        if meta.index < request_index {
            meta.index = request_index;
        }
        Ok(snapshot)
    }
}

const CONF_PREFIX: u8 = 0;
const RAFT_PREFIX: u8 = 1;
const DATA_PREFIX: u8 = 2;
// https://github.com/LucioFranco/kv/blob/417dbb7f969bd311e1e9ed91ab9980a1cae25f56/src/storage.rs#L152
const HIGH_INDEX: [u8; 16] = [
    RAFT_PREFIX,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    255,
    255,
    255,
    255,
    255,
    255,
    255,
    255,
];
const LOW_INDEX: [u8; 16] = [RAFT_PREFIX, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];

const LOW_DATA: [u8; 8] = [DATA_PREFIX, 0, 0, 0, 0, 0, 0, 0];
const HIGH_DATA: [u8; 8] = [DATA_PREFIX + 1, 0, 0, 0, 0, 0, 0, 0];
fn make_log_key(idx: u64) -> [u8; 16] {
    use bytes::BufMut;
    use std::io::Cursor;
    let mut key = [0; 16];

    {
        let mut key = Cursor::new(&mut key[..]);
        key.put_u64_le(RAFT_PREFIX as u64);
        key.put_u64_le(idx);
    }

    key
}

fn make_data_key(key_s: &str) -> Vec<u8> {
    use bytes::{BufMut, BytesMut};
    use std::io::{Cursor, Write};
    let mut key = vec![0; 8 + key_s.len()];

    {
        let mut key = Cursor::new(&mut key[..]);
        key.put_u64_le(DATA_PREFIX as u64);
        key.write_all(key_s.as_bytes());
    }

    key
}
