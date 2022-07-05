// Copyright (c) Aptos
// SPDX-License-Identifier: Apache-2.0

use crate::network_interface::ConsensusMsg;
use crate::quorum_store::{
    types::{Batch, Data, PersistedValue},
    utils::RoundExpirations,
};
use crate::{
    network::NetworkSender,
    quorum_store::{batch_requester::BatchRequester, batch_store::BatchStoreCommand},
};
use anyhow::bail;
use aptos_crypto::HashValue;
use aptos_logger::debug;
use aptos_types::PeerId;
use consensus_types::{
    common::Round,
    proof_of_store::{LogicalTime, ProofOfStore},
};
use dashmap::DashMap;
use executor_types::Error;
use once_cell::sync::OnceCell;
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex,
    },
    time::Duration,
};
use tokio::{
    sync::{
        mpsc::{Receiver, Sender},
        oneshot,
    },
    time,
};

// Make configuration parameter (passed to QuorumStore).
/// Maximum number of rounds in the future from local prospective to hold batches for.
const MAX_BATCH_EXPIRY_ROUND_GAP: Round = 20;

#[derive(Debug)]
pub(crate) enum BatchReaderCommand {
    GetBatchForPeer(HashValue, PeerId),
    GetBatchForSelf(ProofOfStore, oneshot::Sender<Result<Data, Error>>),
    BatchResponse(HashValue, Data),
}

#[derive(PartialEq)]
enum StorageKind {
    MemoryOnly,
    PersistedOnly,
    MemoryAndPersisted,
}

struct QuotaManager {
    memory_balance: usize,
    db_balance: usize,
    memory_quota: usize,
    db_quota: usize,
}

impl QuotaManager {
    fn new(max_db_balance: usize, max_memory_balance: usize) -> Self {
        assert!(max_db_balance >= max_memory_balance);
        Self {
            memory_balance: 0,
            db_balance: 0,
            memory_quota: max_memory_balance,
            db_quota: max_db_balance,
        }
    }

    pub(crate) fn update_quota(&mut self, num_bytes: usize) -> anyhow::Result<StorageKind> {
        if self.memory_balance + num_bytes <= self.memory_quota {
            self.memory_balance = self.memory_balance + num_bytes;
            self.db_balance = self.db_balance + num_bytes;
            Ok(StorageKind::MemoryAndPersisted)
        } else if self.db_balance + num_bytes <= self.db_quota {
            self.db_balance = self.db_balance + num_bytes;
            Ok(StorageKind::PersistedOnly)
        } else {
            bail!("Storage quota exceeded ");
        }
    }

    pub(crate) fn free_quota(&mut self, num_bytes: usize, store_kind: StorageKind) {
        match store_kind {
            StorageKind::MemoryOnly => {
                // MemoryOnly is used to cache data requested by execution compute for
                // the later use by execution commit. As it is already part of the blocks
                // agreed by consensus, the quote is not charged to anyone.
                unreachable!("MemoryOnly quota charged to a peer (must be free)");
            }
            StorageKind::PersistedOnly => {
                self.db_balance = self.db_balance - num_bytes;
            }
            StorageKind::MemoryAndPersisted => {
                self.memory_balance = self.memory_balance - num_bytes;
                self.db_balance = self.db_balance - num_bytes;
            }
        }
    }
}

/// Provides in memory representation of stored batches (strong cache), and allows
/// efficient concurrent readers.
pub struct BatchReader {
    epoch: OnceCell<u64>,
    my_peer_id: PeerId,
    last_committed_round: AtomicU64,
    db_cache: DashMap<HashValue, (PersistedValue, StorageKind)>,
    peer_quota: DashMap<PeerId, QuotaManager>,
    expirations: Mutex<RoundExpirations<HashValue>>,
    batch_store_tx: Sender<BatchStoreCommand>,
    self_tx: Sender<BatchReaderCommand>,
    max_execution_round_lag: Round,
    memory_quota: usize,
    db_quota: usize,
}

impl BatchReader {
    pub(crate) fn new(
        epoch: u64,
        last_committed_round: Round,
        db_content: HashMap<HashValue, PersistedValue>,
        my_peer_id: PeerId,
        batch_store_tx: Sender<BatchStoreCommand>,
        self_tx: Sender<BatchReaderCommand>,
        max_execution_round_lag: Round,
        memory_quota: usize,
        db_quota: usize,
    ) -> (Self, Vec<HashValue>) {
        let self_ob = Self {
            epoch: OnceCell::with_value(epoch),
            my_peer_id,
            last_committed_round: AtomicU64::new(last_committed_round),
            db_cache: DashMap::new(),
            peer_quota: DashMap::new(),
            expirations: Mutex::new(RoundExpirations::new()),
            batch_store_tx,
            self_tx,
            max_execution_round_lag,
            memory_quota,
            db_quota,
        };

        let mut expired_keys = Vec::new();
        for (digest, value) in db_content {
            let expiration = value.expiration;
            assert!(epoch >= expiration.epoch());
            if epoch > expiration.epoch()
                || last_committed_round > expiration.round() + MAX_BATCH_EXPIRY_ROUND_GAP
            {
                expired_keys.push(digest);
            } else {
                self_ob
                    .update_cache(digest, value)
                    .expect("Storage limit exceeded upon BatchReader construction");
            }
        }

        (self_ob, expired_keys)
    }

    fn epoch(&self) -> u64 {
        *self.epoch.get().unwrap()
    }

    // Return an error if storage quota is exceeded.
    fn update_cache(&self, digest: HashValue, mut value: PersistedValue) -> anyhow::Result<()> {
        let author = value.author;
        let mut quota_manager = self
            .peer_quota
            .entry(author)
            .or_insert(QuotaManager::new(self.db_quota, self.memory_quota));
        let store_kind = quota_manager.update_quota(value.num_bytes)?;
        if store_kind == StorageKind::PersistedOnly {
            value.remove_payload();
        }

        let expiration_round = value.expiration.round();
        if let Some((prev_value, prev_kind)) = self.db_cache.insert(digest, (value, store_kind)) {
            self.free_quota(prev_value, prev_kind);
        }
        self.expirations
            .lock()
            .unwrap()
            .add_item(digest, expiration_round);
        Ok(())
    }

    pub(crate) fn save(&self, digest: HashValue, value: PersistedValue) -> anyhow::Result<bool> {
        if value.expiration.epoch() == self.epoch()
            && value.expiration.round() > self.last_committed_round()
            && value.expiration.round() <= self.last_committed_round() + MAX_BATCH_EXPIRY_ROUND_GAP
        {
            if let Some(entry) = self.db_cache.get(&digest) {
                if entry.0.expiration.round() >= value.expiration.round() {
                    debug!("QS: already have the digest with higher expiration");
                    return Ok(false);
                }
            }
            self.update_cache(digest, value)?;
        } else {
            bail!(
                "Wrong expiration {:?}, last committed round = {}",
                value.expiration,
                self.last_committed_round()
            );
        }
        Ok(true)
    }

    fn clear_expired_payload(&self, certified_time: LogicalTime) -> Vec<HashValue> {
        assert_eq!(
            certified_time.epoch(),
            self.epoch(),
            "Execution epoch inconsistent with BatchReader"
        );

        let expired_round = if certified_time.round() >= self.max_execution_round_lag {
            certified_time.round() - self.max_execution_round_lag
        } else {
            0
        };

        let expired_digests = self.expirations.lock().unwrap().expire(expired_round);
        expired_digests
            .into_iter()
            .filter_map(|h| {
                let cache_expiration_round = self
                    .db_cache
                    .get(&h)
                    .expect("Expired entry not in cache")
                    .0 // PersistedValue
                    .expiration
                    .round();
                if cache_expiration_round <= expired_round {
                    let (_, (persisted_value, store_kind)) = self
                        .db_cache
                        .remove(&h)
                        .expect("Expired entry not in cache");
                    self.free_quota(persisted_value, store_kind);
                    Some(h)
                } else {
                    None
                    // Otherwise, expiration got extended, ignore.
                }
            })
            .collect()
    }

    fn free_quota(&self, persisted_value: PersistedValue, store_kind: StorageKind) {
        if store_kind != StorageKind::MemoryOnly {
            let mut quota_manager = self
                .peer_quota
                .get_mut(&persisted_value.author)
                .expect("No QuotaManager for batch author");
            assert_eq!(
                persisted_value.maybe_payload.is_some(),
                store_kind == StorageKind::MemoryAndPersisted,
                "BatchReader payload and storage kind mismatch"
            );
            quota_manager.free_quota(persisted_value.num_bytes, store_kind);
        }
    }

    // TODO: maybe check the epoch to stop communicating on epoch change.
    // TODO: make sure state-sync also sends the message.
    // TODO: make sure message is sent execution re-starts (will also clean)
    pub async fn update_certified_round(&self, certified_time: LogicalTime) {
        debug!("QS: batch reader updating time {:?}", certified_time);
        let prev_round = self
            .last_committed_round
            .fetch_max(certified_time.round(), Ordering::SeqCst);
        assert!(
            prev_round < certified_time.round(),
            "Non-increasing executed rounds reported to BatchStore"
        );
        let expired_keys = self.clear_expired_payload(certified_time);
        self.batch_store_tx
            .send(BatchStoreCommand::Clean(expired_keys))
            .await
            .expect("Failed to send to BatchStore");
    }

    fn last_committed_round(&self) -> Round {
        self.last_committed_round.load(Ordering::Relaxed)
    }

    // TODO: maybe check the epoch to stop communicating on epoch change.
    // TODO: use timeouts and return an error if cannot get tha batch.
    pub async fn get_batch(&self, proof: ProofOfStore) -> oneshot::Receiver<Result<Data, Error>> {
        let (tx, rx) = oneshot::channel();

        if let Some(entry) = self.db_cache.get(&proof.digest()) {
            let value = &entry.0;
            let store_kind = &entry.1;

            if *store_kind == StorageKind::PersistedOnly {
                assert!(
                    value.maybe_payload.is_none(),
                    "BatchReader payload and storage kind mismatch"
                );
                self.batch_store_tx
                    .send(BatchStoreCommand::BatchRequest(
                        proof.digest().clone(),
                        self.my_peer_id,
                        Some(tx),
                    ))
                    .await
                    .expect("Failed to send to BatchStore");
            } else {
                // Available in memory.
                tx.send(Ok(value
                    .maybe_payload
                    .clone()
                    .expect("BatchReader payload and storage kind mismatch")))
                    .expect("Receiver of requested batch is not available");
            }
        } else {
            self.self_tx
                .send(BatchReaderCommand::GetBatchForSelf(proof, tx))
                .await
                .expect("Batch Reader Receiver is not available");
        }
        rx
    }

    pub(crate) async fn start(
        &self,
        mut batch_reader_rx: Receiver<BatchReaderCommand>,
        network_sender: NetworkSender,
        request_num_peers: usize,
        request_timeout_ms: usize,
    ) {
        let mut batch_requester = BatchRequester::new(
            self.epoch(),
            self.my_peer_id,
            request_num_peers,
            request_timeout_ms,
            network_sender.clone(),
        );

        let mut interval = time::interval(Duration::from_millis((request_timeout_ms / 2) as u64));

        loop {
            // TODO: shutdown?
            tokio::select! {
                biased;

                _ = interval.tick() => {
                    batch_requester.handle_timeouts().await;
                },

                Some(cmd) = batch_reader_rx.recv() => {
                    match cmd {
            BatchReaderCommand::GetBatchForPeer(digest, peer_id) => {
                if let Some(entry) = self.db_cache.get(&digest) {
                    let value = &entry.0;
            let store_kind = &entry.1;

                if *store_kind == StorageKind::PersistedOnly {
                    assert!(value.maybe_payload.is_none(),
                                            "BatchReader payload and storage kind mismatch");
                    self.batch_store_tx
                    .send(BatchStoreCommand::BatchRequest(digest, peer_id, None))
                    .await
                    .expect("Failed to send to BatchStore");
                } else {
                    let batch = Batch::new(
                    self.epoch(),
                    self.my_peer_id,
                    digest,
                    Some(value.maybe_payload.clone().expect("BatchReader payload and storage kind mismatch")),
                    );
                    let msg = ConsensusMsg::BatchMsg(Box::new(batch));
                    network_sender.send(msg, vec![peer_id]).await;
                } // TODO: consider returning Nack
                }
            }
                        BatchReaderCommand::GetBatchForSelf(proof, ret_tx) => {
                            batch_requester
                                .add_request(proof.digest().clone(), proof.shuffled_signers(), ret_tx)
                                .await;
                        }
                        BatchReaderCommand::BatchResponse(digest, payload) => {
                            batch_requester.serve_request(digest, payload);
                        }
                    }
                }
            }
        }
    }
}
