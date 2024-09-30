//! State sync is trying to fetch the 'full state' (which can be multiple GB).
//! It happens after HeaderSync and before BlockSync (but only if the node sees that it is 'too much behind').
//! See <https://near.github.io/nearcore/architecture/how/sync.html> for more detailed information.
//! Such state can be downloaded only at special heights (currently - at the beginning of the current and previous
//! epochs).
//!
//! You can do the state sync for each shard independently.
//! It starts by fetching a 'header' - that contains basic information about the state (for example its size, how
//! many parts it consists of, hash of the root etc).
//! Then it tries downloading the rest of the data in 'parts' (usually the part is around 1MB in size).
//!
//! Optionally, it is possible to configure a cloud storage location where state headers and parts
//! are available. The behavior is currently as follows:
//!     - State Headers: If external storage is configured, we will get the headers there.
//!     Otherwise, we will send requests to random peers for the state headers.
//!     - State Parts: In the network crate we track which nodes in the network can serve parts for
//!     which shards. If no external storage is configured, parts are obtained from peers
//!     accordingly. If external storage is configure, we attempt first to obtain parts from the
//!     peers in the network before falling back to the external storage.
//!
//! This is an intermediate approach in the process of eliminating external storage entirely.

use crate::metrics;
use crate::sync::external::{
    create_bucket_readonly, external_storage_location, ExternalConnection,
};
use borsh::BorshDeserialize;
use futures::{future, FutureExt};
use near_async::futures::{FutureSpawner, FutureSpawnerExt};
use near_async::messaging::{CanSend, SendAsync};
use near_async::time::{Clock, Duration, Utc};
use near_chain::chain::{ApplyStatePartsRequest, LoadMemtrieRequest};
use near_chain::near_chain_primitives;
use near_chain::types::RuntimeAdapter;
use near_chain::Chain;
use near_chain_configs::{ExternalStorageConfig, ExternalStorageLocation, SyncConfig};
use near_client_primitives::types::{
    format_shard_sync_phase, DownloadStatus, ShardSyncDownload, ShardSyncStatus,
};
use near_epoch_manager::EpochManagerAdapter;
use near_network::types::{
    HighestHeightPeerInfo, NetworkRequests, NetworkResponses, PeerManagerAdapter,
    PeerManagerMessageRequest, StateSyncEvent,
};
use near_primitives::hash::CryptoHash;
use near_primitives::network::PeerId;
use near_primitives::shard_layout::ShardUId;
use near_primitives::state_part::PartId;
use near_primitives::state_sync::{
    ShardStateSyncResponse, ShardStateSyncResponseHeader, StatePartKey,
};
use near_primitives::types::{AccountId, EpochHeight, EpochId, ShardId, StateRoot};
use near_store::DBCol;
use rand::seq::SliceRandom;
use rand::thread_rng;
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use tokio::sync::{Semaphore, TryAcquireError};
use tracing::info;

use super::external::StateFileType;

/// Maximum number of state parts to request per peer on each round when node is trying to download the state.
pub const MAX_STATE_PART_REQUEST: u64 = 16;
/// Number of state parts already requested stored as pending.
/// This number should not exceed MAX_STATE_PART_REQUEST times (number of peers in the network).
pub const MAX_PENDING_PART: u64 = MAX_STATE_PART_REQUEST * 10000;
/// Time limit per state dump iteration.
/// A node must check external storage for parts to dump again once time is up.
pub const STATE_DUMP_ITERATION_TIME_LIMIT_SECS: u64 = 300;

pub enum StateSyncResult {
    /// State sync still in progress. No action needed by the caller.
    InProgress,
    /// The state for all shards was downloaded.
    Completed,
}

pub enum StateSyncFileDownloadResult {
    StateHeader { header_length: u64, header: ShardStateSyncResponseHeader },
    StatePart { part_length: u64 },
}

/// Signals that a state part was downloaded and saved to RocksDB.
/// Or failed to do so.
pub struct StateSyncGetFileResult {
    sync_hash: CryptoHash,
    shard_id: ShardId,
    part_id: Option<PartId>,
    result: Result<StateSyncFileDownloadResult, String>,
}

struct StateSyncExternal {
    /// Chain ID.
    chain_id: String,
    /// This semaphore imposes a restriction on the maximum number of simultaneous downloads
    semaphore: Arc<tokio::sync::Semaphore>,
    /// A node with external storage configured first tries to obtain state parts from peers.
    /// For each part, it will make this many attempts before getting it from external storage.
    peer_attempts_threshold: u64,
    /// Connection to the external storage.
    external: ExternalConnection,
}

/// Helper to track state sync.
pub struct StateSync {
    clock: Clock,

    /// External storage, if configured.
    external: Option<StateSyncExternal>,

    /// Is used for communication with the peers.
    network_adapter: PeerManagerAdapter,

    /// Timeout (set in config - by default to 60 seconds) is used to figure out how long we should wait
    /// for the answer from the other node before giving up.
    timeout: Duration,

    /// Maps shard_id to result of applying downloaded state.
    state_parts_apply_results: HashMap<ShardId, Result<(), near_chain_primitives::error::Error>>,

    /// Maps shard_id to result of loading in-memory trie.
    load_memtrie_results: HashMap<ShardUId, Result<(), near_chain_primitives::error::Error>>,

    /// Maps shard_id to result of splitting state for resharding.
    resharding_state_roots:
        HashMap<ShardId, Result<HashMap<ShardUId, StateRoot>, near_chain::Error>>,

    /// Message queue to process the received state parts.
    state_parts_mpsc_tx: Sender<StateSyncGetFileResult>,
    state_parts_mpsc_rx: Receiver<StateSyncGetFileResult>,
}

impl StateSync {
    pub fn new(
        clock: Clock,
        network_adapter: PeerManagerAdapter,
        timeout: Duration,
        chain_id: &str,
        sync_config: &SyncConfig,
        catchup: bool,
    ) -> Self {
        let external = match sync_config {
            SyncConfig::Peers => None,
            SyncConfig::ExternalStorage(ExternalStorageConfig {
                location,
                num_concurrent_requests,
                num_concurrent_requests_during_catchup,
                external_storage_fallback_threshold,
            }) => {
                let external = match location {
                    ExternalStorageLocation::S3 { bucket, region, .. } => {
                        let bucket = create_bucket_readonly(
                            &bucket,
                            &region,
                            timeout.max(Duration::ZERO).unsigned_abs(),
                        );
                        if let Err(err) = bucket {
                            panic!("Failed to create an S3 bucket: {}", err);
                        }
                        ExternalConnection::S3 { bucket: Arc::new(bucket.unwrap()) }
                    }
                    ExternalStorageLocation::Filesystem { root_dir } => {
                        ExternalConnection::Filesystem { root_dir: root_dir.clone() }
                    }
                    ExternalStorageLocation::GCS { bucket, .. } => ExternalConnection::GCS {
                        gcs_client: Arc::new(cloud_storage::Client::default()),
                        reqwest_client: Arc::new(reqwest::Client::default()),
                        bucket: bucket.clone(),
                    },
                };
                let num_permits = if catchup {
                    *num_concurrent_requests_during_catchup
                } else {
                    *num_concurrent_requests
                } as usize;
                Some(StateSyncExternal {
                    chain_id: chain_id.to_string(),
                    semaphore: Arc::new(tokio::sync::Semaphore::new(num_permits)),
                    peer_attempts_threshold: *external_storage_fallback_threshold,
                    external,
                })
            }
        };
        let (tx, rx) = channel::<StateSyncGetFileResult>();
        StateSync {
            clock,
            external,
            network_adapter,
            timeout,
            state_parts_apply_results: HashMap::new(),
            load_memtrie_results: HashMap::new(),
            resharding_state_roots: HashMap::new(),
            state_parts_mpsc_rx: rx,
            state_parts_mpsc_tx: tx,
        }
    }

    // The return value indicates whether state sync is
    // finished, in which case the client will transition to block sync
    fn sync_shards_status(
        &mut self,
        me: &Option<AccountId>,
        sync_hash: CryptoHash,
        sync_status: &mut HashMap<u64, ShardSyncDownload>,
        chain: &mut Chain,
        epoch_manager: &dyn EpochManagerAdapter,
        highest_height_peers: &[HighestHeightPeerInfo],
        tracking_shards: Vec<ShardId>,
        now: Utc,
        state_parts_task_scheduler: &near_async::messaging::Sender<ApplyStatePartsRequest>,
        load_memtrie_scheduler: &near_async::messaging::Sender<LoadMemtrieRequest>,
        state_parts_future_spawner: &dyn FutureSpawner,
        use_colour: bool,
        runtime_adapter: Arc<dyn RuntimeAdapter>,
    ) -> Result<bool, near_chain::Error> {
        let mut all_done = true;

        let prev_hash = *chain.get_block_header(&sync_hash)?.prev_hash();
        let prev_epoch_id = *chain.get_block_header(&prev_hash)?.epoch_id();
        let epoch_id = *chain.get_block_header(&sync_hash)?.epoch_id();
        let prev_shard_layout = epoch_manager.get_shard_layout(&prev_epoch_id)?;
        let shard_layout = epoch_manager.get_shard_layout(&epoch_id)?;
        if prev_shard_layout != shard_layout {
            // This error message is used in tests to ensure node exists for the
            // correct reason. When changing it please also update the tests.
            panic!("cannot sync to the first epoch after sharding upgrade. Please wait for the next epoch or find peers that are more up to date");
        }
        let need_to_reshard = epoch_manager.will_shard_layout_change(&prev_hash)?;

        for shard_id in tracking_shards {
            let version = prev_shard_layout.version();
            let shard_uid = ShardUId { version, shard_id: shard_id as u32 };
            let mut download_timeout = false;
            let mut run_shard_state_download = false;
            let shard_sync_download = sync_status.entry(shard_id).or_insert_with(|| {
                run_shard_state_download = true;
                ShardSyncDownload::new_download_state_header(now)
            });

            let mut shard_sync_done = false;
            match &shard_sync_download.status {
                ShardSyncStatus::StateDownloadHeader => {
                    (download_timeout, run_shard_state_download) = self
                        .sync_shards_download_header_status(
                            shard_id,
                            shard_sync_download,
                            sync_hash,
                            chain,
                            now,
                        )?;
                }
                ShardSyncStatus::StateDownloadParts => {
                    let res =
                        self.sync_shards_download_parts_status(shard_id, shard_sync_download, now);
                    download_timeout = res.0;
                    run_shard_state_download = res.1;
                }
                ShardSyncStatus::StateApplyScheduling => {
                    self.sync_shards_apply_scheduling_status(
                        shard_id,
                        shard_sync_download,
                        sync_hash,
                        chain,
                        now,
                        state_parts_task_scheduler,
                    )?;
                }
                ShardSyncStatus::StateApplyInProgress => {
                    self.sync_shards_apply_status(
                        shard_id,
                        shard_sync_download,
                        sync_hash,
                        chain,
                        load_memtrie_scheduler,
                    )?;
                }
                ShardSyncStatus::StateApplyFinalizing => {
                    shard_sync_done = self.sync_shards_apply_finalizing_status(
                        shard_uid,
                        chain,
                        sync_hash,
                        now,
                        need_to_reshard,
                        shard_sync_download,
                    )?;
                }
                ShardSyncStatus::ReshardingScheduling => {
                    panic!("Resharding V2 scheduling is no longer supported")
                }
                ShardSyncStatus::ReshardingApplying => {
                    panic!("Resharding V2 scheduling is no longer supported")
                }
                ShardSyncStatus::StateSyncDone => {
                    shard_sync_done = true;
                }
            }
            let stage = if shard_sync_done {
                // Update the state sync stage metric, because maybe we'll not
                // enter this function again.
                ShardSyncStatus::StateSyncDone.repr()
            } else {
                shard_sync_download.status.repr()
            };
            metrics::STATE_SYNC_STAGE.with_label_values(&[&shard_id.to_string()]).set(stage as i64);
            all_done &= shard_sync_done;

            if download_timeout {
                tracing::warn!(
                    target: "sync",
                    %shard_id,
                    timeout_sec = self.timeout.whole_seconds(),
                    "State sync didn't download the state, sending StateRequest again");
                tracing::debug!(
                    target: "sync",
                    %shard_id,
                    %sync_hash,
                    ?me,
                    phase = format_shard_sync_phase(shard_sync_download, use_colour),
                    "State sync status");
            }

            // Execute syncing for shard `shard_id`
            if run_shard_state_download {
                self.request_shard(
                    shard_id,
                    chain,
                    sync_hash,
                    shard_sync_download,
                    highest_height_peers,
                    runtime_adapter.clone(),
                    state_parts_future_spawner,
                )?;
            }
        }

        Ok(all_done)
    }

    /// Checks the message queue for new downloaded parts and writes them.
    fn process_downloaded_parts(
        &mut self,
        chain: &mut Chain,
        sync_hash: CryptoHash,
        shard_sync: &mut HashMap<u64, ShardSyncDownload>,
    ) {
        for StateSyncGetFileResult { sync_hash: msg_sync_hash, shard_id, part_id, result } in
            self.state_parts_mpsc_rx.try_iter()
        {
            if msg_sync_hash != sync_hash {
                tracing::debug!(target: "sync",
                    ?shard_id,
                    ?sync_hash,
                    ?msg_sync_hash,
                    "Received message for other epoch.",
                );
                continue;
            }
            if let Some(shard_sync_download) = shard_sync.get_mut(&shard_id) {
                let file_type = shard_sync_download.status.to_string();
                let (download_result, download) = match result {
                    Err(err) => (Err(err), None),
                    // Store the header
                    Ok(StateSyncFileDownloadResult::StateHeader { header_length, header }) => {
                        info!(target: "sync", ?header_length, ?part_id, "processing state header");
                        if shard_sync_download.status != ShardSyncStatus::StateDownloadHeader {
                            continue;
                        }
                        let download = shard_sync_download.get_header_download_mut();
                        if download.as_ref().and_then(|d| Some(d.done)).unwrap_or(true) {
                            continue;
                        }
                        let result = chain
                            .set_state_header(shard_id, sync_hash, header)
                            .map_err(|err| format!("State sync set_state_header error: {err:?}"))
                            .map(|_| header_length);
                        (result, download)
                    }
                    // Part was stored on the tx side.
                    Ok(StateSyncFileDownloadResult::StatePart { part_length }) => {
                        info!(target: "sync", ?part_length, ?part_id, ?shard_id, "processing state part");
                        if shard_sync_download.status != ShardSyncStatus::StateDownloadParts {
                            continue;
                        }
                        (
                            Ok(part_length),
                            part_id.and_then(|part_id| {
                                shard_sync_download.downloads.get_mut(part_id.idx as usize)
                            }),
                        )
                    }
                };

                process_download_response(
                    shard_id,
                    sync_hash,
                    download,
                    file_type,
                    download_result,
                );
            }
        }
    }

    // Called by the client actor, when it finished applying all the downloaded parts.
    pub fn set_apply_result(
        &mut self,
        shard_id: ShardId,
        apply_result: Result<(), near_chain::Error>,
    ) {
        self.state_parts_apply_results.insert(shard_id, apply_result);
    }

    // Called by the client actor, when it finished resharding.
    pub fn set_resharding_result(
        &mut self,
        shard_id: ShardId,
        result: Result<HashMap<ShardUId, StateRoot>, near_chain::Error>,
    ) {
        self.resharding_state_roots.insert(shard_id, result);
    }

    // Called by the client actor, when it finished loading memtrie.
    pub fn set_load_memtrie_result(
        &mut self,
        shard_uid: ShardUId,
        result: Result<(), near_chain::Error>,
    ) {
        self.load_memtrie_results.insert(shard_uid, result);
    }

    /// Find the hash of the first block on the same epoch (and chain) of block with hash `sync_hash`.
    pub fn get_epoch_start_sync_hash(
        chain: &Chain,
        sync_hash: &CryptoHash,
    ) -> Result<CryptoHash, near_chain::Error> {
        let mut header = chain.get_block_header(sync_hash)?;
        let mut epoch_id = *header.epoch_id();
        let mut hash = *header.hash();
        let mut prev_hash = *header.prev_hash();
        loop {
            if prev_hash == CryptoHash::default() {
                return Ok(hash);
            }
            header = chain.get_block_header(&prev_hash)?;
            if &epoch_id != header.epoch_id() {
                return Ok(hash);
            }
            epoch_id = *header.epoch_id();
            hash = *header.hash();
            prev_hash = *header.prev_hash();
        }
    }

    /// Returns new ShardSyncDownload if successful, otherwise returns given shard_sync_download
    fn request_shard(
        &mut self,
        shard_id: ShardId,
        chain: &Chain,
        sync_hash: CryptoHash,
        shard_sync_download: &mut ShardSyncDownload,
        highest_height_peers: &[HighestHeightPeerInfo],
        runtime_adapter: Arc<dyn RuntimeAdapter>,
        state_parts_future_spawner: &dyn FutureSpawner,
    ) -> Result<(), near_chain::Error> {
        // Downloading strategy starts here
        match shard_sync_download.status {
            ShardSyncStatus::StateDownloadHeader => {
                // If no external storage is configured, we have to request headers from our peers
                let possible_targets = match self.external {
                    Some(_) => vec![],
                    None => {
                        if highest_height_peers.is_empty() {
                            tracing::debug!(target: "sync", "Can't request a state header: No possible targets");
                            return Ok(());
                        }
                        highest_height_peers.iter().map(|peer| peer.peer_info.id.clone()).collect()
                    }
                };

                self.request_shard_header(
                    chain,
                    shard_id,
                    sync_hash,
                    &possible_targets,
                    shard_sync_download,
                    state_parts_future_spawner,
                );
            }
            ShardSyncStatus::StateDownloadParts => {
                self.request_shard_parts(
                    shard_id,
                    sync_hash,
                    shard_sync_download,
                    chain,
                    runtime_adapter,
                    state_parts_future_spawner,
                );
            }
            _ => {}
        }

        Ok(())
    }

    /// Makes a StateRequestHeader header to one of the peers or downloads the header from external storage.
    fn request_shard_header(
        &mut self,
        chain: &Chain,
        shard_id: ShardId,
        sync_hash: CryptoHash,
        possible_targets: &[PeerId],
        new_shard_sync_download: &mut ShardSyncDownload,
        state_parts_future_spawner: &dyn FutureSpawner,
    ) {
        let header_download = new_shard_sync_download.get_header_download_mut().unwrap();
        if let Some(StateSyncExternal { chain_id, external, .. }) = &self.external {
            // TODO(saketh): Eventually we aim to deprecate the external storage and rely only on
            // peers in the network for getting state headers.
            let sync_block_header = chain.get_block_header(&sync_hash).unwrap();
            let epoch_id = sync_block_header.epoch_id();
            let epoch_info = chain.epoch_manager.get_epoch_info(epoch_id).unwrap();
            let epoch_height = epoch_info.epoch_height();
            request_header_from_external_storage(
                header_download,
                shard_id,
                sync_hash,
                epoch_id,
                epoch_height,
                &chain_id.clone(),
                external.clone(),
                state_parts_future_spawner,
                self.state_parts_mpsc_tx.clone(),
            );
        } else {
            // TODO(saketh): We need to rework the way we get headers from peers entirely.
            // Currently it is assumed that one of the direct peers of the node is able to generate
            // the shard header.
            let peer_id = possible_targets.choose(&mut thread_rng()).cloned().unwrap();
            tracing::debug!(target: "sync", ?peer_id, shard_id, ?sync_hash, ?possible_targets, "request_shard_header");
            assert!(header_download.run_me.load(Ordering::SeqCst));
            header_download.run_me.store(false, Ordering::SeqCst);
            header_download.state_requests_count += 1;
            header_download.last_target = Some(peer_id.clone());
            let run_me = header_download.run_me.clone();
            near_performance_metrics::actix::spawn(
                std::any::type_name::<Self>(),
                self.network_adapter
                    .send_async(PeerManagerMessageRequest::NetworkRequests(
                        NetworkRequests::StateRequestHeader { shard_id, sync_hash, peer_id },
                    ))
                    .then(move |result| {
                        if let Ok(NetworkResponses::RouteNotFound) =
                            result.map(|f| f.as_network_response())
                        {
                            // Send a StateRequestHeader on the next iteration
                            run_me.store(true, Ordering::SeqCst);
                        }
                        future::ready(())
                    }),
            );
        }
    }

    /// Makes requests to download state parts for the given epoch of the given shard.
    fn request_shard_parts(
        &mut self,
        shard_id: ShardId,
        sync_hash: CryptoHash,
        new_shard_sync_download: &mut ShardSyncDownload,
        chain: &Chain,
        runtime_adapter: Arc<dyn RuntimeAdapter>,
        state_parts_future_spawner: &dyn FutureSpawner,
    ) {
        // Iterate over all parts that needs to be requested (i.e. download.run_me is true).
        // Parts are ordered such that its index match its part_id.
        let mut peer_requests_sent = 0;
        let mut state_root_and_part_count: Option<(CryptoHash, u64)> = None;
        for (part_id, download) in parts_to_fetch(new_shard_sync_download) {
            if self
                .external
                .as_ref()
                .is_some_and(|ext| download.state_requests_count >= ext.peer_attempts_threshold)
            {
                // TODO(saketh): After we have sufficient confidence that requesting state parts
                // from peers is working well, we will eliminate the external storage entirely.
                let StateSyncExternal { chain_id, semaphore, external, .. } =
                    self.external.as_ref().unwrap();
                if semaphore.available_permits() == 0 {
                    continue;
                }

                let sync_block_header = chain.get_block_header(&sync_hash).unwrap();
                let epoch_id = sync_block_header.epoch_id();
                let epoch_info = chain.epoch_manager.get_epoch_info(epoch_id).unwrap();
                let epoch_height = epoch_info.epoch_height();

                let (state_root, state_num_parts) =
                    state_root_and_part_count.get_or_insert_with(|| {
                        let shard_state_header =
                            chain.get_state_header(shard_id, sync_hash).unwrap();
                        (
                            shard_state_header.chunk_prev_state_root(),
                            shard_state_header.num_state_parts(),
                        )
                    });

                request_part_from_external_storage(
                    part_id,
                    download,
                    shard_id,
                    sync_hash,
                    epoch_id,
                    epoch_height,
                    *state_num_parts,
                    &chain_id.clone(),
                    *state_root,
                    semaphore.clone(),
                    external.clone(),
                    runtime_adapter.clone(),
                    state_parts_future_spawner,
                    self.state_parts_mpsc_tx.clone(),
                );
            } else {
                if peer_requests_sent >= MAX_STATE_PART_REQUEST {
                    continue;
                }

                // The request sent to the network adapater needs to include the sync_prev_prev_hash
                // so that a peer hosting the correct snapshot can be selected.
                let prev_header = chain
                    .get_block_header(&sync_hash)
                    .map(|header| chain.get_block_header(&header.prev_hash()));

                match prev_header {
                    Ok(Ok(prev_header)) => {
                        let sync_prev_prev_hash = prev_header.prev_hash();
                        request_part_from_peers(
                            part_id,
                            download,
                            shard_id,
                            sync_hash,
                            *sync_prev_prev_hash,
                            &self.network_adapter,
                            state_parts_future_spawner,
                        );

                        peer_requests_sent += 1;
                    }
                    Ok(Err(err)) => {
                        tracing::error!(target: "sync", %shard_id, %sync_hash, ?err, "could not get prev header");
                    }
                    Err(err) => {
                        tracing::error!(target: "sync", %shard_id, %sync_hash, ?err, "could not get header");
                    }
                }
            }
        }
    }

    /// The main 'step' function that should be called periodically to check and update the sync process.
    /// The current state/progress information is mostly kept within 'new_shard_sync' object.
    ///
    /// Returns the state of the sync.
    pub fn run(
        &mut self,
        me: &Option<AccountId>,
        sync_hash: CryptoHash,
        sync_status: &mut HashMap<u64, ShardSyncDownload>,
        chain: &mut Chain,
        epoch_manager: &dyn EpochManagerAdapter,
        highest_height_peers: &[HighestHeightPeerInfo],
        // Shards to sync.
        tracking_shards: Vec<ShardId>,
        state_parts_task_scheduler: &near_async::messaging::Sender<ApplyStatePartsRequest>,
        load_memtrie_scheduler: &near_async::messaging::Sender<LoadMemtrieRequest>,
        state_parts_future_spawner: &dyn FutureSpawner,
        use_colour: bool,
        runtime_adapter: Arc<dyn RuntimeAdapter>,
    ) -> Result<StateSyncResult, near_chain::Error> {
        let _span =
            tracing::debug_span!(target: "sync", "run_sync", sync_type = "StateSync").entered();
        tracing::trace!(target: "sync", %sync_hash, ?tracking_shards, "syncing state");
        let now = self.clock.now_utc();

        if tracking_shards.is_empty() {
            // This case is possible if a validator cares about the same shards in the new epoch as
            //    in the previous (or about a subset of them), return success right away

            return Ok(StateSyncResult::Completed);
        }
        // The downloaded parts are from all shards. This function takes all downloaded parts and
        // saves them to the DB.
        // TODO: Ideally, we want to process the downloads on a different thread than the one that runs the Client.
        self.process_downloaded_parts(chain, sync_hash, sync_status);
        let all_done = self.sync_shards_status(
            me,
            sync_hash,
            sync_status,
            chain,
            epoch_manager,
            highest_height_peers,
            tracking_shards,
            now,
            state_parts_task_scheduler,
            load_memtrie_scheduler,
            state_parts_future_spawner,
            use_colour,
            runtime_adapter,
        )?;

        if all_done {
            Ok(StateSyncResult::Completed)
        } else {
            Ok(StateSyncResult::InProgress)
        }
    }

    pub fn update_download_on_state_response_message(
        &mut self,
        shard_sync_download: &mut ShardSyncDownload,
        hash: CryptoHash,
        shard_id: u64,
        state_response: ShardStateSyncResponse,
        chain: &mut Chain,
    ) {
        match shard_sync_download.status {
            ShardSyncStatus::StateDownloadHeader => {
                let header_download = shard_sync_download.get_header_download_mut().unwrap();
                if let Some(header) = state_response.take_header() {
                    if !header_download.done {
                        match chain.set_state_header(shard_id, hash, header) {
                            Ok(()) => {
                                header_download.done = true;
                            }
                            Err(err) => {
                                tracing::error!(target: "sync", %shard_id, %hash, ?err, "State sync set_state_header error");
                                header_download.error = true;
                            }
                        }
                    }
                } else {
                    // No header found.
                    // It may happen because requested node couldn't build state response.
                    if !header_download.done {
                        tracing::info!(target: "sync", %shard_id, %hash, "state_response doesn't have header, should be re-requested");
                        header_download.error = true;
                    }
                }
            }
            ShardSyncStatus::StateDownloadParts => {
                if let Some(part) = state_response.take_part() {
                    let num_parts = shard_sync_download.downloads.len() as u64;
                    let (part_id, data) = part;
                    if part_id >= num_parts {
                        tracing::error!(target: "sync", %shard_id, %hash, part_id, "State sync received incorrect part_id, potential malicious peer");
                        return;
                    }
                    if !shard_sync_download.downloads[part_id as usize].done {
                        match chain.set_state_part(
                            shard_id,
                            hash,
                            PartId::new(part_id, num_parts),
                            &data,
                        ) {
                            Ok(()) => {
                                tracing::debug!(target: "sync", %shard_id, %hash, part_id, "Received correct start part");
                                self.network_adapter
                                    .send(StateSyncEvent::StatePartReceived(shard_id, part_id));
                                shard_sync_download.downloads[part_id as usize].done = true;
                            }
                            Err(err) => {
                                tracing::error!(target: "sync", %shard_id, %hash, part_id, ?err, "State sync set_state_part error");
                                shard_sync_download.downloads[part_id as usize].error = true;
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    /// Checks if the header is downloaded.
    /// If the download is complete, then moves forward to `StateDownloadParts`,
    /// otherwise retries the header request.
    /// Returns `(download_timeout, run_shard_state_download)` where:
    /// * `download_timeout` means that the state header request timed out (and needs to be retried).
    /// * `run_shard_state_download` means that header or part download requests need to run for this shard.
    fn sync_shards_download_header_status(
        &mut self,
        shard_id: ShardId,
        shard_sync_download: &mut ShardSyncDownload,
        sync_hash: CryptoHash,
        chain: &Chain,
        now: Utc,
    ) -> Result<(bool, bool), near_chain::Error> {
        let download = &mut shard_sync_download.downloads[0];
        // StateDownloadHeader is the first step. We want to fetch the basic information about the state (its size, hash etc).
        if download.done {
            let shard_state_header = chain.get_state_header(shard_id, sync_hash)?;
            let state_num_parts = shard_state_header.num_state_parts();
            // If the header was downloaded successfully - move to phase 2 (downloading parts).
            // Create the vector with entry for each part.
            *shard_sync_download =
                ShardSyncDownload::new_download_state_parts(now, state_num_parts);
            Ok((false, true))
        } else {
            let download_timeout = now - download.prev_update_time > self.timeout;
            if download_timeout {
                tracing::debug!(target: "sync", last_target = ?download.last_target, start_time = ?download.start_time, prev_update_time = ?download.prev_update_time, state_requests_count = download.state_requests_count, "header request timed out");
                metrics::STATE_SYNC_HEADER_TIMEOUT
                    .with_label_values(&[&shard_id.to_string()])
                    .inc();
            }
            if download.error {
                tracing::debug!(target: "sync", last_target = ?download.last_target, start_time = ?download.start_time, prev_update_time = ?download.prev_update_time, state_requests_count = download.state_requests_count, "header request error");
                metrics::STATE_SYNC_HEADER_ERROR.with_label_values(&[&shard_id.to_string()]).inc();
            }
            // Retry in case of timeout or failure.
            if download_timeout || download.error {
                download.run_me.store(true, Ordering::SeqCst);
                download.error = false;
                download.prev_update_time = now;
            }
            let run_me = download.run_me.load(Ordering::SeqCst);
            Ok((download_timeout, run_me))
        }
    }

    /// Checks if the parts are downloaded.
    /// If download of all parts is complete, then moves forward to `StateDownloadScheduling`.
    /// Returns `(download_timeout, run_shard_state_download)` where:
    /// * `download_timeout` means that the state header request timed out (and needs to be retried).
    /// * `run_shard_state_download` means that header or part download requests need to run for this shard.
    fn sync_shards_download_parts_status(
        &mut self,
        shard_id: ShardId,
        shard_sync_download: &mut ShardSyncDownload,
        now: Utc,
    ) -> (bool, bool) {
        // Step 2 - download all the parts (each part is usually around 1MB).
        let mut download_timeout = false;
        let mut run_shard_state_download = false;

        let mut parts_done = true;
        let num_parts = shard_sync_download.downloads.len();
        let mut num_parts_done = 0;
        for part_download in shard_sync_download.downloads.iter_mut() {
            if !part_download.done {
                parts_done = false;
                let prev = part_download.prev_update_time;
                let part_timeout = now - prev > self.timeout; // Retry parts that failed.
                if part_timeout || part_download.error {
                    download_timeout |= part_timeout;
                    if part_timeout || part_download.last_target.is_some() {
                        // Don't immediately retry failed requests from external
                        // storage. Most often error is a state part not
                        // available. That error doesn't get fixed by retrying,
                        // but rather by waiting.
                        metrics::STATE_SYNC_RETRY_PART
                            .with_label_values(&[&shard_id.to_string()])
                            .inc();
                        part_download.run_me.store(true, Ordering::SeqCst);
                        part_download.error = false;
                        part_download.prev_update_time = now;
                    }
                }
                if part_download.run_me.load(Ordering::SeqCst) {
                    run_shard_state_download = true;
                }
            }
            if part_download.done {
                num_parts_done += 1;
            }
        }
        metrics::STATE_SYNC_PARTS_DONE
            .with_label_values(&[&shard_id.to_string()])
            .set(num_parts_done);
        metrics::STATE_SYNC_PARTS_TOTAL
            .with_label_values(&[&shard_id.to_string()])
            .set(num_parts as i64);
        // If all parts are done - we can move towards scheduling.
        if parts_done {
            *shard_sync_download = ShardSyncDownload {
                downloads: vec![],
                status: ShardSyncStatus::StateApplyScheduling,
            };
        }
        (download_timeout, run_shard_state_download)
    }

    fn sync_shards_apply_scheduling_status(
        &mut self,
        shard_id: ShardId,
        shard_sync_download: &mut ShardSyncDownload,
        sync_hash: CryptoHash,
        chain: &mut Chain,
        now: Utc,
        state_parts_task_scheduler: &near_async::messaging::Sender<ApplyStatePartsRequest>,
    ) -> Result<(), near_chain::Error> {
        let shard_state_header = chain.get_state_header(shard_id, sync_hash)?;
        let state_num_parts = shard_state_header.num_state_parts();
        // Now apply all the parts to the chain / runtime.
        // TODO: not sure why this has to happen only after all the parts were downloaded -
        //       as we could have done this in parallel after getting each part.
        match chain.schedule_apply_state_parts(
            shard_id,
            sync_hash,
            state_num_parts,
            state_parts_task_scheduler,
        ) {
            Ok(()) => {
                *shard_sync_download = ShardSyncDownload {
                    downloads: vec![],
                    status: ShardSyncStatus::StateApplyInProgress,
                }
            }
            Err(err) => {
                // Cannot finalize the downloaded state.
                // The reasonable behavior here is to start from the very beginning.
                metrics::STATE_SYNC_DISCARD_PARTS.with_label_values(&[&shard_id.to_string()]).inc();
                tracing::error!(target: "sync", %shard_id, %sync_hash, ?err, "State sync finalizing error");
                *shard_sync_download = ShardSyncDownload::new_download_state_header(now);
                chain.clear_downloaded_parts(shard_id, sync_hash, state_num_parts)?;
            }
        }
        Ok(())
    }

    fn sync_shards_apply_status(
        &mut self,
        shard_id: ShardId,
        shard_sync_download: &mut ShardSyncDownload,
        sync_hash: CryptoHash,
        chain: &mut Chain,
        load_memtrie_scheduler: &near_async::messaging::Sender<LoadMemtrieRequest>,
    ) -> Result<(), near_chain::Error> {
        // Keep waiting until our shard is on the list of results
        // (these are set via callback from ClientActor - both for sync and catchup).
        if let Some(result) = self.state_parts_apply_results.remove(&shard_id) {
            result?;
            let epoch_id = *chain.get_block_header(&sync_hash)?.epoch_id();
            let shard_uid = chain.epoch_manager.shard_id_to_uid(shard_id, &epoch_id)?;
            let shard_state_header = chain.get_state_header(shard_id, sync_hash)?;
            let chunk = shard_state_header.cloned_chunk();
            let block_hash = chunk.prev_block();

            // We synced shard state on top of _previous_ block for chunk in shard state header and applied state parts to
            // flat storage. Now we can set flat head to hash of this block and create flat storage.
            // If block_hash is equal to default - this means that we're all the way back at genesis.
            // So we don't have to add the storage state for shard in such case.
            // TODO(8438) - add additional test scenarios for this case.
            if *block_hash != CryptoHash::default() {
                chain.create_flat_storage_for_shard(shard_uid, &chunk)?;
            }
            // We schedule load memtrie when flat storage state (if any) is ready.
            // It is possible that memtrie is not enabled for that shard,
            // in which case the task would finish immediately with Ok() status.
            // We require the task result to further proceed with state sync.
            chain.schedule_load_memtrie(shard_uid, sync_hash, &chunk, load_memtrie_scheduler);
            *shard_sync_download = ShardSyncDownload {
                downloads: vec![],
                status: ShardSyncStatus::StateApplyFinalizing,
            }
        }
        Ok(())
    }

    /// Checks and updates the status of state sync for the given shard.
    ///
    /// If shard sync is done the status is updated to either StateSyncDone or
    /// ReshardingScheduling in which case the next step is to start resharding.
    ///
    /// Returns true only when the shard sync is fully done. Returns false when
    /// the shard sync is in progress or if the next step is resharding.
    fn sync_shards_apply_finalizing_status(
        &mut self,
        shard_uid: ShardUId,
        chain: &mut Chain,
        sync_hash: CryptoHash,
        now: Utc,
        need_to_reshard: bool,
        shard_sync_download: &mut ShardSyncDownload,
    ) -> Result<bool, near_chain::Error> {
        let shard_id = shard_uid.shard_id();
        let result = self.sync_shards_apply_finalizing_status_impl(
            shard_uid,
            chain,
            sync_hash,
            need_to_reshard,
            shard_sync_download,
        );

        if let Err(err) = &result {
            // Cannot finalize the downloaded state.
            // The reasonable behavior here is to start from the very beginning.
            metrics::STATE_SYNC_DISCARD_PARTS.with_label_values(&[&shard_id.to_string()]).inc();
            tracing::error!(target: "sync", %shard_id, %sync_hash, ?err, "State sync finalizing error");
            *shard_sync_download = ShardSyncDownload::new_download_state_header(now);
            let shard_state_header = chain.get_state_header(shard_id, sync_hash)?;
            let state_num_parts = shard_state_header.num_state_parts();
            chain.clear_downloaded_parts(shard_id, sync_hash, state_num_parts)?;
        }

        return result;
    }

    fn sync_shards_apply_finalizing_status_impl(
        &mut self,
        shard_uid: ShardUId,
        chain: &mut Chain,
        sync_hash: CryptoHash,
        need_to_reshard: bool,
        shard_sync_download: &mut ShardSyncDownload,
    ) -> Result<bool, near_chain::Error> {
        // Keep waiting until our shard is on the list of results
        // (these are set via callback from ClientActor - both for sync and catchup).
        let mut shard_sync_done = false;
        let Some(result) = self.load_memtrie_results.remove(&shard_uid) else {
            return Ok(shard_sync_done);
        };

        result?;

        chain.set_state_finalize(shard_uid.shard_id(), sync_hash)?;
        if need_to_reshard {
            // If the shard layout is changing in this epoch - we have to apply it right now.
            let status = ShardSyncStatus::ReshardingScheduling;
            *shard_sync_download = ShardSyncDownload { downloads: vec![], status };
        } else {
            // If there is no layout change - we're done.
            let status = ShardSyncStatus::StateSyncDone;
            *shard_sync_download = ShardSyncDownload { downloads: vec![], status };
            shard_sync_done = true;
        }

        Ok(shard_sync_done)
    }
}

/// Returns parts that still need to be fetched.
fn parts_to_fetch(
    new_shard_sync_download: &mut ShardSyncDownload,
) -> impl Iterator<Item = (u64, &mut DownloadStatus)> {
    new_shard_sync_download
        .downloads
        .iter_mut()
        .enumerate()
        .filter(|(_, download)| download.run_me.load(Ordering::SeqCst))
        .map(|(part_id, download)| (part_id as u64, download))
}

async fn download_header_from_external_storage(
    shard_id: ShardId,
    sync_hash: CryptoHash,
    location: String,
    external: ExternalConnection,
) -> Result<StateSyncFileDownloadResult, std::string::String> {
    external
    .get_file(shard_id, &location, &StateFileType::StateHeader)
    .await
    .map_err(|err| err.to_string())
    .and_then(|data| {
        info!(target: "sync", ?shard_id, "downloaded state header");
        let header_length = data.len() as u64;
        ShardStateSyncResponseHeader::try_from_slice(&data)
        .map(|header| StateSyncFileDownloadResult::StateHeader { header_length , header })
        .map_err(|_| {
            tracing::info!(target: "sync", %shard_id, %sync_hash, "Could not parse downloaded header.");
            format!("Could not parse state sync header for shard {shard_id:?}")
        })
    })
}

/// Starts an asynchronous network request to external storage to fetch the given header.
fn request_header_from_external_storage(
    download: &mut DownloadStatus,
    shard_id: ShardId,
    sync_hash: CryptoHash,
    epoch_id: &EpochId,
    epoch_height: EpochHeight,
    chain_id: &str,
    external: ExternalConnection,
    state_parts_future_spawner: &dyn FutureSpawner,
    state_parts_mpsc_tx: Sender<StateSyncGetFileResult>,
) {
    if !download.run_me.swap(false, Ordering::SeqCst) {
        tracing::info!(target: "sync", %shard_id, "run_me is already false");
        return;
    }
    download.state_requests_count += 1;
    download.last_target = None;

    let location = external_storage_location(
        chain_id,
        epoch_id,
        epoch_height,
        shard_id,
        &StateFileType::StateHeader,
    );
    state_parts_future_spawner.spawn(
        "download_header_from_external_storage",
        async move {
            let result = download_header_from_external_storage(shard_id, sync_hash, location, external).await;
            match state_parts_mpsc_tx.send(StateSyncGetFileResult {
                sync_hash,
                shard_id,
                part_id: None,
                result,
            }) {
                Ok(_) => tracing::debug!(target: "sync", %shard_id, "Download header response sent to processing thread."),
                Err(err) => {
                    tracing::error!(target: "sync", ?err, %shard_id, "Unable to send header download response to processing thread.");
                },
            }
        }
    );
}

async fn download_and_store_part_from_external_storage(
    part_id: PartId,
    file_type: &StateFileType,
    location: String,
    shard_id: ShardId,
    sync_hash: CryptoHash,
    state_root: StateRoot,
    external: ExternalConnection,
    runtime_adapter: Arc<dyn RuntimeAdapter>,
) -> Result<StateSyncFileDownloadResult, String> {
    external
    .get_file(shard_id, &location, file_type)
    .await
    .map_err(|err| err.to_string())
    .and_then(|data|  {
        info!(target: "sync", ?shard_id, ?part_id, "downloaded state part");
        if runtime_adapter.validate_state_part(&state_root, part_id, &data) {
            let mut store_update = runtime_adapter.store().store_update();
            borsh::to_vec(&StatePartKey(sync_hash, shard_id, part_id.idx))
            .and_then(|key| {
                store_update.set(DBCol::StateParts, &key, &data);
                store_update.commit()
            })
            .map_err(|err| format!("Failed to store a state part. err={err:?}, state_root={state_root:?}, part_id={part_id:?}, shard_id={shard_id:?}"))
            .map(|_| data.len() as u64)
            .map(|part_length| StateSyncFileDownloadResult::StatePart { part_length })
        } else {
            Err(format!("validate_state_part failed. state_root={state_root:?}, part_id={part_id:?}, shard_id={shard_id}"))
        }
    })
}
/// Starts an asynchronous network request to external storage to fetch the given state part.
fn request_part_from_external_storage(
    part_id: u64,
    download: &mut DownloadStatus,
    shard_id: ShardId,
    sync_hash: CryptoHash,
    epoch_id: &EpochId,
    epoch_height: EpochHeight,
    num_parts: u64,
    chain_id: &str,
    state_root: StateRoot,
    semaphore: Arc<Semaphore>,
    external: ExternalConnection,
    runtime_adapter: Arc<dyn RuntimeAdapter>,
    state_parts_future_spawner: &dyn FutureSpawner,
    state_parts_mpsc_tx: Sender<StateSyncGetFileResult>,
) {
    if !download.run_me.swap(false, Ordering::SeqCst) {
        tracing::info!(target: "sync", %shard_id, part_id, "run_me is already false");
        return;
    }
    download.state_requests_count += 1;
    download.last_target = None;

    let location = external_storage_location(
        chain_id,
        epoch_id,
        epoch_height,
        shard_id,
        &StateFileType::StatePart { part_id, num_parts },
    );

    match semaphore.try_acquire_owned() {
        Ok(permit) => {
            state_parts_future_spawner.spawn(
                "download_and_store_part_from_external_storage",
                async move {
                    let file_type = StateFileType::StatePart { part_id, num_parts };
                    let part_id = PartId{ idx: part_id, total: num_parts };
                    let result = download_and_store_part_from_external_storage(
                        part_id,
                        &file_type,
                        location,
                        shard_id,
                        sync_hash,
                        state_root,
                        external,
                        runtime_adapter)
                        .await;

                    match state_parts_mpsc_tx.send(StateSyncGetFileResult {
                        sync_hash,
                        shard_id,
                        part_id: Some(part_id),
                        result,
                    }) {
                        Ok(_) => tracing::debug!(target: "sync", %shard_id, ?part_id, "Download response sent to processing thread."),
                        Err(err) => {
                            tracing::error!(target: "sync", ?err, %shard_id, ?part_id, "Unable to send part download response to processing thread.");
                        },
                    }
                    drop(permit)
                }
            );
        }
        Err(TryAcquireError::NoPermits) => {
            download.run_me.store(true, Ordering::SeqCst);
        }
        Err(TryAcquireError::Closed) => {
            download.run_me.store(true, Ordering::SeqCst);
            tracing::warn!(target: "sync", %shard_id, part_id, "Failed to schedule download. Semaphore closed.");
        }
    }
}

/// Asynchronously requests a state part from a suitable peer.
fn request_part_from_peers(
    part_id: u64,
    download: &mut DownloadStatus,
    shard_id: ShardId,
    sync_hash: CryptoHash,
    sync_prev_prev_hash: CryptoHash,
    network_adapter: &PeerManagerAdapter,
    state_parts_future_spawner: &dyn FutureSpawner,
) {
    download.run_me.store(false, Ordering::SeqCst);
    download.state_requests_count += 1;
    let run_me = download.run_me.clone();

    state_parts_future_spawner.spawn(
        "StateSync",
        network_adapter
            .send_async(PeerManagerMessageRequest::NetworkRequests(
                NetworkRequests::StateRequestPart {
                    shard_id,
                    sync_hash,
                    sync_prev_prev_hash,
                    part_id,
                },
            ))
            .then(move |result| {
                if let Ok(NetworkResponses::RouteNotFound) = result.map(|f| f.as_network_response())
                {
                    // Send a StateRequestPart on the next iteration
                    run_me.store(true, Ordering::SeqCst);
                }
                future::ready(())
            }),
    );
}

/// Works around how data requests to external storage are done.
/// This function investigates if the response is valid and updates `done` and `error` appropriately.
/// If the response is successful, then the downloaded state file was written to the DB.
fn process_download_response(
    shard_id: ShardId,
    sync_hash: CryptoHash,
    download: Option<&mut DownloadStatus>,
    file_type: String,
    download_result: Result<u64, String>,
) {
    match download_result {
        Ok(data_len) => {
            // No error, aka Success.
            metrics::STATE_SYNC_EXTERNAL_PARTS_DONE
                .with_label_values(&[&shard_id.to_string(), &file_type])
                .inc();
            metrics::STATE_SYNC_EXTERNAL_PARTS_SIZE_DOWNLOADED
                .with_label_values(&[&shard_id.to_string(), &file_type])
                .inc_by(data_len);
            download.map(|download| download.done = true);
        }
        // The request failed without reaching the external storage.
        Err(err) => {
            metrics::STATE_SYNC_EXTERNAL_PARTS_FAILED
                .with_label_values(&[&shard_id.to_string(), &file_type])
                .inc();
            tracing::debug!(target: "sync", ?err, %shard_id, %sync_hash, ?file_type, "Failed to get a file from external storage, will retry");
            download.map(|download| download.done = false);
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use actix::System;
    use actix_rt::Arbiter;
    use near_actix_test_utils::run_actix;
    use near_async::futures::ActixArbiterHandleFutureSpawner;
    use near_async::messaging::{noop, IntoMultiSender, IntoSender};
    use near_async::time::Clock;
    use near_chain::test_utils;
    use near_chain::{test_utils::process_block_sync, BlockProcessingArtifact, Provenance};
    use near_crypto::SecretKey;
    use near_epoch_manager::EpochManagerAdapter;
    use near_network::test_utils::MockPeerManagerAdapter;
    use near_network::types::PeerInfo;
    use near_primitives::state_sync::{
        CachedParts, ShardStateSyncResponseHeader, ShardStateSyncResponseV3,
    };
    use near_primitives::{test_utils::TestBlockBuilder, types::EpochId};

    #[test]
    // Start a new state sync - and check that it asks for a header.
    fn test_ask_for_header() {
        let mock_peer_manager = Arc::new(MockPeerManagerAdapter::default());
        let mut state_sync = StateSync::new(
            Clock::real(),
            mock_peer_manager.as_multi_sender(),
            Duration::seconds(1),
            "chain_id",
            &SyncConfig::Peers,
            false,
        );
        let mut new_shard_sync = HashMap::new();

        let (mut chain, kv, runtime, signer) = test_utils::setup(Clock::real());

        // TODO: lower the epoch length
        for _ in 0..(chain.epoch_length + 1) {
            let prev = chain.get_block(&chain.head().unwrap().last_block_hash).unwrap();
            let block = if kv.is_next_block_epoch_start(prev.hash()).unwrap() {
                TestBlockBuilder::new(Clock::real(), &prev, signer.clone())
                    .epoch_id(*prev.header().next_epoch_id())
                    .next_epoch_id(EpochId { 0: *prev.hash() })
                    .next_bp_hash(*prev.header().next_bp_hash())
                    .build()
            } else {
                TestBlockBuilder::new(Clock::real(), &prev, signer.clone()).build()
            };

            process_block_sync(
                &mut chain,
                &None,
                block.into(),
                Provenance::PRODUCED,
                &mut BlockProcessingArtifact::default(),
            )
            .unwrap();
        }

        let request_hash = &chain.head().unwrap().last_block_hash;
        let state_sync_header = chain.get_state_response_header(0, *request_hash).unwrap();
        let state_sync_header = match state_sync_header {
            ShardStateSyncResponseHeader::V1(_) => panic!("Invalid header"),
            ShardStateSyncResponseHeader::V2(internal) => internal,
        };

        let secret_key = SecretKey::from_random(near_crypto::KeyType::ED25519);
        let public_key = secret_key.public_key();
        let peer_id = PeerId::new(public_key);
        let highest_height_peer_info = HighestHeightPeerInfo {
            peer_info: PeerInfo { id: peer_id.clone(), addr: None, account_id: None },
            genesis_id: Default::default(),
            highest_block_height: chain.epoch_length + 10,
            highest_block_hash: Default::default(),
            tracked_shards: vec![0],
            archival: false,
        };

        run_actix(async {
            state_sync
                .run(
                    &None,
                    *request_hash,
                    &mut new_shard_sync,
                    &mut chain,
                    kv.as_ref(),
                    &[highest_height_peer_info],
                    vec![0],
                    &noop().into_sender(),
                    &noop().into_sender(),
                    &ActixArbiterHandleFutureSpawner(Arbiter::new().handle()),
                    false,
                    runtime,
                )
                .unwrap();

            // Wait for the message that is sent to peer manager.
            mock_peer_manager.notify.notified().await;
            let request = mock_peer_manager.pop().unwrap();

            assert_eq!(
                NetworkRequests::StateRequestHeader {
                    shard_id: 0,
                    sync_hash: *request_hash,
                    peer_id: peer_id.clone(),
                },
                request.as_network_requests()
            );

            assert_eq!(1, new_shard_sync.len());
            let download = new_shard_sync.get(&0).unwrap();

            assert_eq!(download.status, ShardSyncStatus::StateDownloadHeader);

            assert_eq!(download.downloads.len(), 1);
            let download_status = &download.downloads[0];

            // 'run me' is false - as we've just executed this peer manager request.
            assert_eq!(download_status.run_me.load(Ordering::SeqCst), false);
            assert_eq!(download_status.error, false);
            assert_eq!(download_status.done, false);
            assert_eq!(download_status.state_requests_count, 1);
            assert_eq!(download_status.last_target, Some(peer_id),);

            // Now let's simulate header return message.

            let state_response = ShardStateSyncResponse::V3(ShardStateSyncResponseV3 {
                header: Some(state_sync_header),
                part: None,
                cached_parts: Some(CachedParts::AllParts),
                can_generate: true,
            });

            state_sync.update_download_on_state_response_message(
                &mut new_shard_sync.get_mut(&0).unwrap(),
                *request_hash,
                0,
                state_response,
                &mut chain,
            );

            let download = new_shard_sync.get(&0).unwrap();
            assert_eq!(download.status, ShardSyncStatus::StateDownloadHeader);
            // Download should be marked as done.
            assert_eq!(download.downloads[0].done, true);

            System::current().stop()
        });
    }
}
