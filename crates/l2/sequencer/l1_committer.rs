use crate::{
    BlockProducerConfig, CommitterConfig, EthConfig, SequencerConfig,
    based::sequencer_state::{SequencerState, SequencerStatus},
    sequencer::{
        errors::CommitterError,
        utils::{
            self, fetch_blocks_with_respective_fee_configs, get_git_commit_hash, system_now_ms,
        },
    },
};

use bytes::Bytes;
use ethrex_blockchain::{Blockchain, vm::StoreVmDatabase};
use ethrex_common::{
    Address, H256, U256,
    types::{
        AccountUpdate, BLOB_BASE_FEE_UPDATE_FRACTION, BlobsBundle, Block, BlockNumber, Genesis,
        MIN_BASE_FEE_PER_BLOB_GAS, TxType, batch::Batch, blobs_bundle, fake_exponential_checked,
    },
};
use ethrex_l2_common::{
    calldata::Value,
    l1_messages::{get_block_l1_messages, get_l1_message_hash},
    merkle_tree::compute_merkle_root,
    privileged_transactions::{
        PRIVILEGED_TX_BUDGET, compute_privileged_transactions_hash,
        get_block_privileged_transactions,
    },
    prover::ProverInputData,
    state_diff::{StateDiff, prepare_state_diff},
};
use ethrex_l2_rpc::signer::{Signer, SignerHealth};
use ethrex_l2_sdk::{
    build_generic_tx, calldata::encode_calldata, get_last_committed_batch,
    send_tx_bump_gas_exponential_backoff,
};
#[cfg(feature = "metrics")]
use ethrex_metrics::l2::metrics::{METRICS, MetricsBlockType};
use ethrex_metrics::metrics;
use ethrex_rlp::encode::RLPEncode;
use ethrex_rpc::{
    clients::eth::{EthClient, Overrides},
    types::block_identifier::{BlockIdentifier, BlockTag},
};
use ethrex_storage::EngineType;
use ethrex_storage::Store;
use ethrex_storage_rollup::StoreRollup;
use ethrex_vm::{BlockExecutionResult, Evm};
use serde::Serialize;
use std::{
    collections::{BTreeMap, HashMap},
    fs::remove_dir_all,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, trace, warn};

use super::{errors::BlobEstimationError, utils::random_duration};
use spawned_concurrency::tasks::{
    CallResponse, CastResponse, GenServer, GenServerHandle, send_after,
};

const COMMIT_FUNCTION_SIGNATURE_BASED: &str =
    "commitBatch(uint256,bytes32,bytes32,bytes32,bytes32,bytes[])";
const COMMIT_FUNCTION_SIGNATURE: &str = "commitBatch(uint256,bytes32,bytes32,bytes32,bytes32)";
/// Default wake up time for the committer to check if it should send a commit tx
const COMMITTER_DEFAULT_WAKE_TIME_MS: u64 = 60_000;

#[derive(Clone)]
pub enum CallMessage {
    Stop,
    /// time to wait in ms before sending commit
    Start(u64),
    Health,
}

#[derive(Clone)]
pub enum InMessage {
    Commit,
}

#[derive(Clone)]
pub enum OutMessage {
    Done,
    Error(String),
    Stopped,
    Started,
    Health(Box<L1CommitterHealth>),
}

pub struct L1Committer {
    eth_client: EthClient,
    blockchain: Arc<Blockchain>,
    on_chain_proposer_address: Address,
    store: Store,
    rollup_store: StoreRollup,
    commit_time_ms: u64,
    batch_gas_limit: Option<u64>,
    arbitrary_base_blob_gas_price: u64,
    validium: bool,
    signer: Signer,
    based: bool,
    sequencer_state: SequencerState,
    /// Time to wait before checking if it should send a new batch
    committer_wake_up_ms: u64,
    /// Timestamp of last successful committed batch
    last_committed_batch_timestamp: u128,
    /// Last succesful committed batch number
    last_committed_batch: u64,
    /// Cancellation token for the next inbound InMessage::Commit
    cancellation_token: Option<CancellationToken>,
    /// Elasticity multiplier for prover input generation
    elasticity_multiplier: u64,
    /// Git commit hash of the build
    git_commit_hash: String,
    /// Store containing the state checkpoint at the last committed batch.
    ///
    /// It is used to ensure state availability for batch preparation and
    /// witness generation.
    current_checkpoint_store: Store,
    /// Blockchain instance using the current checkpoint store.
    ///
    /// It is used for witness generation.
    current_checkpoint_blockchain: Arc<Blockchain>,
    /// Network genesis.
    ///
    /// It is used for creating checkpoints.
    genesis: Genesis,
    /// Directory where checkpoints are stored.
    checkpoints_dir: PathBuf,
}

#[derive(Clone, Serialize)]
pub struct L1CommitterHealth {
    rpc_healthcheck: BTreeMap<String, serde_json::Value>,
    commit_time_ms: u64,
    arbitrary_base_blob_gas_price: u64,
    validium: bool,
    based: bool,
    sequencer_state: String,
    committer_wake_up_ms: u64,
    last_committed_batch_timestamp: u128,
    last_committed_batch: u64,
    signer_status: SignerHealth,
    running: bool,
    on_chain_proposer_address: Address,
}

impl L1Committer {
    #[expect(clippy::too_many_arguments)]
    pub async fn new(
        committer_config: &CommitterConfig,
        proposer_config: &BlockProducerConfig,
        eth_config: &EthConfig,
        blockchain: Arc<Blockchain>,
        store: Store,
        rollup_store: StoreRollup,
        based: bool,
        sequencer_state: SequencerState,
        initial_checkpoint_store: Store,
        initial_checkpoint_blockchain: Arc<Blockchain>,
        genesis: Genesis,
        checkpoints_dir: PathBuf,
    ) -> Result<Self, CommitterError> {
        let eth_client = EthClient::new_with_config(
            eth_config.rpc_url.iter().map(AsRef::as_ref).collect(),
            eth_config.max_number_of_retries,
            eth_config.backoff_factor,
            eth_config.min_retry_delay,
            eth_config.max_retry_delay,
            Some(eth_config.maximum_allowed_max_fee_per_gas),
            Some(eth_config.maximum_allowed_max_fee_per_blob_gas),
        )?;
        let last_committed_batch =
            get_last_committed_batch(&eth_client, committer_config.on_chain_proposer_address)
                .await?;
        Ok(Self {
            eth_client,
            blockchain,
            on_chain_proposer_address: committer_config.on_chain_proposer_address,
            store,
            rollup_store,
            commit_time_ms: committer_config.commit_time_ms,
            batch_gas_limit: committer_config.batch_gas_limit,
            arbitrary_base_blob_gas_price: committer_config.arbitrary_base_blob_gas_price,
            validium: committer_config.validium,
            signer: committer_config.signer.clone(),
            based,
            sequencer_state,
            committer_wake_up_ms: committer_config
                .commit_time_ms
                .min(COMMITTER_DEFAULT_WAKE_TIME_MS),
            last_committed_batch_timestamp: 0,
            last_committed_batch,
            cancellation_token: None,
            elasticity_multiplier: proposer_config.elasticity_multiplier,
            git_commit_hash: get_git_commit_hash(),
            current_checkpoint_store: initial_checkpoint_store,
            current_checkpoint_blockchain: initial_checkpoint_blockchain,
            genesis,
            checkpoints_dir,
        })
    }

    #[expect(clippy::too_many_arguments)]
    pub async fn spawn(
        store: Store,
        blockchain: Arc<Blockchain>,
        rollup_store: StoreRollup,
        cfg: SequencerConfig,
        sequencer_state: SequencerState,
        initial_checkpoint_store: Store,
        initial_checkpoint_blockchain: Arc<Blockchain>,
        genesis: Genesis,
        checkpoints_dir: PathBuf,
    ) -> Result<GenServerHandle<L1Committer>, CommitterError> {
        let state = Self::new(
            &cfg.l1_committer,
            &cfg.block_producer,
            &cfg.eth,
            blockchain,
            store.clone(),
            rollup_store.clone(),
            cfg.based.enabled,
            sequencer_state,
            initial_checkpoint_store,
            initial_checkpoint_blockchain,
            genesis,
            checkpoints_dir,
        )
        .await?;
        // NOTE: we spawn as blocking due to `generate_blobs_bundle` and
        // `send_tx_bump_gas_exponential_backoff` blocking for more than 40ms
        let l1_committer = state.start_blocking();
        if let OutMessage::Error(reason) = l1_committer
            .clone()
            .call(CallMessage::Start(cfg.l1_committer.first_wake_up_time_ms))
            .await?
        {
            Err(CommitterError::UnexpectedError(format!(
                "Failed to send first wake up message to committer {reason}"
            )))
        } else {
            Ok(l1_committer)
        }
    }

    async fn commit_next_batch_to_l1(&mut self) -> Result<(), CommitterError> {
        info!("Running committer main loop");
        // Get the batch to commit
        let last_committed_batch_number =
            get_last_committed_batch(&self.eth_client, self.on_chain_proposer_address).await?;
        let batch_to_commit = last_committed_batch_number + 1;

        let batch = match self.rollup_store.get_batch(batch_to_commit).await? {
            Some(batch) => batch,
            None => {
                let last_committed_blocks = self
                    .rollup_store
                    .get_block_numbers_by_batch(last_committed_batch_number)
                    .await?
                    .ok_or(
                        CommitterError::RetrievalError(format!("Failed to get batch with batch number {last_committed_batch_number}. Batch is missing when it should be present. This is a bug"))
                    )?;
                let last_block = last_committed_blocks
                    .last()
                    .ok_or(
                        CommitterError::RetrievalError(format!("Last committed batch ({last_committed_batch_number}) doesn't have any blocks. This is probably a bug."))
                    )?;
                let first_block_to_commit = last_block + 1;

                // Try to prepare batch
                let (
                    blobs_bundle,
                    new_state_root,
                    message_hashes,
                    privileged_transactions_hash,
                    last_block_of_batch,
                ) = self
                    .prepare_batch_from_block(*last_block, batch_to_commit)
                    .await?;

                if *last_block == last_block_of_batch {
                    debug!("No new blocks to commit, skipping");
                    return Ok(());
                }

                let batch = Batch {
                    number: batch_to_commit,
                    first_block: first_block_to_commit,
                    last_block: last_block_of_batch,
                    state_root: new_state_root,
                    privileged_transactions_hash,
                    message_hashes,
                    blobs_bundle,
                    commit_tx: None,
                    verify_tx: None,
                };

                self.rollup_store.seal_batch(batch.clone()).await?;

                debug!(
                    first_block = batch.first_block,
                    last_block = batch.last_block,
                    "Batch {} stored in database",
                    batch.number
                );

                batch
            }
        };

        info!(
            first_block = batch.first_block,
            last_block = batch.last_block,
            "Generating and storing witness for batch {}",
            batch.number,
        );

        self.generate_and_store_batch_prover_input(&batch).await?;

        // We need to update the current checkpoint after generating the witness
        // with it, and before sending the commitment.
        // The actual checkpoint store directory is not pruned until the batch
        // it served in is verified on L1.
        self.update_current_checkpoint(&batch).await?;

        info!(
            first_block = batch.first_block,
            last_block = batch.last_block,
            "Sending commitment for batch {}",
            batch.number,
        );

        match self.send_commitment(&batch).await {
            Ok(commit_tx_hash) => {
                metrics!(
                let _ = METRICS
                    .set_block_type_and_block_number(
                        MetricsBlockType::LastCommittedBlock,
                        batch.last_block,
                    )
                    .inspect_err(|e| {
                        tracing::error!(
                            "Failed to set metric: last committed block {}",
                            e.to_string()
                        )
                    });
                );

                self.rollup_store
                    .store_commit_tx_by_batch(batch.number, commit_tx_hash)
                    .await?;

                info!(
                    "Commitment sent for batch {}, with tx hash {commit_tx_hash:#x}.",
                    batch.number
                );
                Ok(())
            }
            Err(error) => Err(CommitterError::FailedToSendCommitment(format!(
                "Failed to send commitment for batch {}. first_block: {} last_block: {}: {error}",
                batch.number, batch.first_block, batch.last_block
            ))),
        }
    }

    async fn prepare_batch_from_block(
        &mut self,
        mut last_added_block_number: BlockNumber,
        batch_number: u64,
    ) -> Result<(BlobsBundle, H256, Vec<H256>, H256, BlockNumber), CommitterError> {
        let first_block_of_batch = last_added_block_number + 1;
        let mut blobs_bundle = BlobsBundle::default();

        let mut acc_messages = vec![];
        let mut acc_privileged_txs = vec![];
        let mut acc_account_updates: HashMap<Address, AccountUpdate> = HashMap::new();
        let mut message_hashes = vec![];
        let mut privileged_transactions_hashes = vec![];
        let mut new_state_root = H256::default();
        let mut acc_gas_used = 0_u64;

        #[cfg(feature = "metrics")]
        let mut tx_count = 0_u64;
        #[cfg(feature = "metrics")]
        let mut blob_size = 0_usize;
        #[cfg(feature = "metrics")]
        let mut batch_gas_used = 0_u64;

        info!("Preparing state diff from block {first_block_of_batch}, {batch_number}");

        let one_time_checkpoint_path = self
            .checkpoints_dir
            .join(format!("temp_checkpoint_batch_{batch_number}"));

        // For re-execution we need to use a checkpoint to the previous state
        // (i.e. checkpoint of the state to the latest block from the previous
        // batch, or the state of the genesis if this is the first batch).
        // We already have this initial checkpoint as part of the L1Committer
        // struct, but we need to create a one-time copy of it because
        // we still need to use the current checkpoint store later for witness
        // generation.
        let (one_time_checkpoint_store, one_time_checkpoint_blockchain) = self
            .create_checkpoint(&self.current_checkpoint_store, &one_time_checkpoint_path)
            .await?;

        loop {
            let block_to_commit_number = last_added_block_number + 1;

            // Get potential block to include in the batch
            // Here it is ok to fetch the blocks from the main store and not from
            // the checkpoint because the blocks will be available. We only need
            // the checkpoint for re-execution, this is during witness generation
            // in generate_and_store_batch_prover_input and for later in this
            // function.
            let potential_batch_block = {
                let Some(block_to_commit_body) = self
                    .store
                    .get_block_body(block_to_commit_number)
                    .await
                    .map_err(CommitterError::from)?
                else {
                    debug!("No new block to commit, skipping..");
                    break;
                };
                let block_to_commit_header = self
                    .store
                    .get_block_header(block_to_commit_number)
                    .map_err(CommitterError::from)?
                    .ok_or(CommitterError::FailedToGetInformationFromStorage(
                        "Failed to get_block_header() after get_block_body()".to_owned(),
                    ))?;

                Block::new(block_to_commit_header, block_to_commit_body)
            };

            let current_block_gas_used = potential_batch_block.header.gas_used;

            // Check if adding this block would exceed the batch gas limit
            if self.batch_gas_limit.is_some_and(|batch_gas_limit| {
                acc_gas_used + current_block_gas_used > batch_gas_limit
            }) {
                debug!(
                    "Batch gas limit reached. Any remaining blocks will be processed in the next batch"
                );
                break;
            }

            // Get block transactions and receipts
            let mut txs = vec![];
            let mut receipts = vec![];
            for (index, tx) in potential_batch_block.body.transactions.iter().enumerate() {
                let receipt = self
                    .store
                    .get_receipt(block_to_commit_number, index.try_into()?)
                    .await?
                    .ok_or(CommitterError::RetrievalError(
                        "Transactions in a block should have a receipt".to_owned(),
                    ))?;
                txs.push(tx.clone());
                receipts.push(receipt);
            }

            metrics!(
                tx_count += txs
                    .len()
                    .try_into()
                    .inspect_err(|_| tracing::error!("Failed to collect metric tx count"))
                    .unwrap_or(0);
                batch_gas_used += potential_batch_block.header.gas_used;
            );
            // Get block messages and privileged transactions
            let messages = get_block_l1_messages(&receipts);
            let privileged_transactions = get_block_privileged_transactions(&txs);

            // Get block account updates.
            let account_updates = if let Some(account_updates) = self
                .rollup_store
                .get_account_updates_by_block_number(block_to_commit_number)
                .await?
            {
                account_updates
            } else {
                warn!(
                    "Could not find execution cache result for block {}, falling back to re-execution",
                    last_added_block_number + 1
                );

                // Here we use the checkpoint store because we need the previous
                // state available (i.e. not pruned) for re-execution.
                let vm_db = StoreVmDatabase::new(
                    one_time_checkpoint_store.clone(),
                    potential_batch_block.header.parent_hash,
                );

                let fee_config = self
                    .rollup_store
                    .get_fee_config_by_block(block_to_commit_number)
                    .await?
                    .ok_or(CommitterError::FailedToGetInformationFromStorage(
                        "Failed to get fee config for re-execution".to_owned(),
                    ))?;

                let mut vm = Evm::new_for_l2(vm_db, fee_config)?;

                vm.execute_block(&potential_batch_block)?;

                vm.get_state_transitions()?
            };

            // The checkpoint store's state corresponds to the parent state of
            // the first block of the batch. Therefore, we need to apply the
            // account updates of each block as we go, to be able to continue
            // re-executing the next blocks in the batch.
            {
                let account_updates_list = one_time_checkpoint_store
                    .apply_account_updates_batch(
                        potential_batch_block.header.parent_hash,
                        &account_updates,
                    )
                    .await?
                    .ok_or(CommitterError::FailedToGetInformationFromStorage(
                        "no account updated".to_owned(),
                    ))?;

                one_time_checkpoint_blockchain
                    .store_block(
                        potential_batch_block.clone(),
                        account_updates_list,
                        BlockExecutionResult {
                            receipts,
                            requests: vec![],
                        },
                    )
                    .await?;
            }

            // Accumulate block data with the rest of the batch.
            acc_messages.extend(messages.clone());
            acc_privileged_txs.extend(privileged_transactions.clone());
            for account in account_updates {
                let address = account.address;
                if let Some(existing) = acc_account_updates.get_mut(&address) {
                    existing.merge(account);
                } else {
                    acc_account_updates.insert(address, account);
                }
            }

            // It is safe to retrieve this from the main store because blocks
            // are available there. What's not available is the state
            let parent_block_hash = self
                .store
                .get_block_header(first_block_of_batch)?
                .ok_or(CommitterError::FailedToGetInformationFromStorage(
                    "Failed to get_block_header() of the last added block".to_owned(),
                ))?
                .parent_hash;

            // Again, here the VM database should be instantiated from the checkpoint
            // store to have access to the previous state
            let parent_db =
                StoreVmDatabase::new(one_time_checkpoint_store.clone(), parent_block_hash);

            let acc_privileged_txs_len: u64 = acc_privileged_txs.len().try_into()?;
            if acc_privileged_txs_len > PRIVILEGED_TX_BUDGET {
                warn!(
                    "Privileged transactions budget exceeded. Any remaining blocks will be processed in the next batch."
                );
                // Break loop. Use the previous generated blobs_bundle.
                break;
            }

            let result = if !self.validium {
                // Prepare current state diff.
                let state_diff: StateDiff = prepare_state_diff(
                    potential_batch_block.header.clone(),
                    &parent_db,
                    &acc_messages,
                    &acc_privileged_txs,
                    acc_account_updates.clone().into_values().collect(),
                )?;
                generate_blobs_bundle(&state_diff)
            } else {
                Ok((BlobsBundle::default(), 0_usize))
            };

            let Ok((bundle, latest_blob_size)) = result else {
                if block_to_commit_number == first_block_of_batch {
                    return Err(CommitterError::Unreachable(
                        "Not enough blob space for a single block batch. This means a block was incorrectly produced.".to_string(),
                    ));
                }
                warn!(
                    "Batch size limit reached. Any remaining blocks will be processed in the next batch."
                );
                // Break loop. Use the previous generated blobs_bundle.
                break;
            };

            trace!("Got bundle, latest blob size {latest_blob_size}");

            // Save current blobs_bundle and continue to add more blocks.
            blobs_bundle = bundle;

            metrics!(
                blob_size = latest_blob_size;
            );

            privileged_transactions_hashes.extend(
                privileged_transactions
                    .iter()
                    .filter_map(|tx| tx.get_privileged_hash())
                    .collect::<Vec<H256>>(),
            );

            message_hashes.extend(messages.iter().map(get_l1_message_hash));

            new_state_root = one_time_checkpoint_store
                .state_trie(potential_batch_block.hash())?
                .ok_or(CommitterError::FailedToGetInformationFromStorage(
                    "Failed to get state root from storage".to_owned(),
                ))?
                .hash_no_commit();

            last_added_block_number += 1;
            acc_gas_used += current_block_gas_used;
        } // end loop

        metrics!(if let (Ok(privileged_transaction_count), Ok(messages_count)) = (
                privileged_transactions_hashes.len().try_into(),
                message_hashes.len().try_into()
            ) {
                let _ = self
                    .rollup_store
                    .update_operations_count(tx_count, privileged_transaction_count, messages_count)
                    .await
                    .inspect_err(|e| {
                        tracing::error!("Failed to update operations metric: {}", e.to_string())
                    });
            }
            #[allow(clippy::as_conversions)]
            let blob_usage_percentage = blob_size as f64 * 100_f64 / ethrex_common::types::BYTES_PER_BLOB_F64;
            let batch_gas_used = batch_gas_used.try_into()?;
            let batch_size = (last_added_block_number - first_block_of_batch).try_into()?;
            let tx_count = tx_count.try_into()?;
            METRICS.set_blob_usage_percentage(blob_usage_percentage);
            METRICS.set_batch_gas_used(batch_number, batch_gas_used)?;
            METRICS.set_batch_size(batch_number, batch_size)?;
            METRICS.set_batch_tx_count(batch_number, tx_count)?;
        );

        info!(
            "Added {} privileged transactions to the batch",
            privileged_transactions_hashes.len()
        );

        let privileged_transactions_hash =
            compute_privileged_transactions_hash(privileged_transactions_hashes)?;

        remove_dir_all(&one_time_checkpoint_path).map_err(|e| {
            CommitterError::FailedToCreateCheckpoint(format!(
                "Failed to remove one-time checkpoint directory {one_time_checkpoint_path:?}: {e}"
            ))
        })?;

        Ok((
            blobs_bundle,
            new_state_root,
            message_hashes,
            privileged_transactions_hash,
            last_added_block_number,
        ))
    }

    async fn generate_and_store_batch_prover_input(
        &self,
        batch: &Batch,
    ) -> Result<(), CommitterError> {
        let (blocks, fee_configs) = fetch_blocks_with_respective_fee_configs::<CommitterError>(
            batch.number,
            &self.store,
            &self.rollup_store,
        )
        .await?;

        let batch_witness = self
            .current_checkpoint_blockchain
            .generate_witness_for_blocks_with_fee_configs(&blocks, Some(&fee_configs))
            .await
            .map_err(CommitterError::FailedToGenerateBatchWitness)?;

        // We still need to differentiate the validium case because for validium
        // we are generating the BlobsBundle with BlobsBundle::default which
        // sets the commitments and proofs to empty vectors.
        let (blob_commitment, blob_proof) = if self.validium {
            ([0; 48], [0; 48])
        } else {
            let BlobsBundle {
                commitments,
                proofs,
                ..
            } = &batch.blobs_bundle;

            (
                commitments
                    .first()
                    .cloned()
                    .ok_or(CommitterError::Unreachable(
                        "Blob commitment missing in batch blobs bundle".to_string(),
                    ))?,
                proofs.first().cloned().ok_or(CommitterError::Unreachable(
                    "Blob proof missing in batch blobs bundle".to_string(),
                ))?,
            )
        };

        let prover_input = ProverInputData {
            blocks,
            execution_witness: batch_witness,
            elasticity_multiplier: self.elasticity_multiplier,
            blob_commitment,
            blob_proof,
            fee_configs,
        };

        self.rollup_store
            .store_prover_input_by_batch_and_version(
                batch.number,
                &self.git_commit_hash,
                prover_input,
            )
            .await?;

        Ok(())
    }

    /// Updates the current checkpoint store and blockchain to the state at the
    /// given latest batch.
    ///
    /// The reference to the previous checkpoint is lost after this operation,
    /// but the directory is not deleted until the batch it serves in is verified
    /// on L1.
    async fn update_current_checkpoint(
        &mut self,
        latest_batch: &Batch,
    ) -> Result<(), CommitterError> {
        let new_checkpoint_path = self
            .checkpoints_dir
            .join(format!("checkpoint_batch_{}", latest_batch.number));

        // CAUTION
        // We need to skip checkpoint creation if the directory already exists.
        // Sometimes the commit_next_batch task is retried after a failure, and in
        // that case we would try to create a checkpoint again at the same path,
        // causing an lock error under rocksdb feature.
        if new_checkpoint_path.exists() {
            debug!("Checkpoint at path {new_checkpoint_path:?} already exists, skipping creation");
            return Ok(());
        }

        let (new_checkpoint_store, new_checkpoint_blockchain) = self
            .create_checkpoint(&self.store, &new_checkpoint_path)
            .await?;

        self.current_checkpoint_store = new_checkpoint_store;

        self.current_checkpoint_blockchain = new_checkpoint_blockchain;

        Ok(())
    }

    /// Creates a checkpoint of the given store at the specified path.
    ///
    /// This function performs the following steps:
    /// 1. Creates a checkpoint of the provided store at the specified path.
    /// 2. Initializes a new store and blockchain for the checkpoint.
    /// 3. Regenerates the head state in the checkpoint store.
    /// 4. Validates that the checkpoint store's head block number and latest block match those of the original store.
    async fn create_checkpoint(
        &self,
        checkpointee: &Store,
        path: &Path,
    ) -> Result<(Store, Arc<Blockchain>), CommitterError> {
        checkpointee.create_checkpoint(&path).await?;

        #[cfg(feature = "rocksdb")]
        let engine_type = EngineType::RocksDB;
        #[cfg(not(feature = "rocksdb"))]
        let engine_type = EngineType::InMemory;

        let checkpoint_store = {
            let checkpoint_store_inner = Store::new(path, engine_type)?;

            checkpoint_store_inner
                .add_initial_state(self.genesis.clone())
                .await?;

            checkpoint_store_inner
        };

        let checkpoint_blockchain = Arc::new(Blockchain::new(
            checkpoint_store.clone(),
            self.blockchain.options.clone(),
        ));

        let checkpoint_head_block_number = checkpoint_store.get_latest_block_number().await?;

        let db_head_block_number = checkpointee.get_latest_block_number().await?;

        if checkpoint_head_block_number != db_head_block_number {
            return Err(CommitterError::FailedToCreateCheckpoint(
                "checkpoint store head block number does not match main store head block number before regeneration".to_string(),
            ));
        }

        regenerate_head_state(&checkpoint_store, &checkpoint_blockchain).await?;

        let checkpoint_latest_block_number = checkpoint_store.get_latest_block_number().await?;

        let db_latest_block_number = checkpointee.get_latest_block_number().await?;

        let checkpoint_latest_block = checkpoint_store
            .get_block_by_number(checkpoint_latest_block_number)
            .await?
            .ok_or(CommitterError::FailedToCreateCheckpoint(
                "latest block not found in checkpoint store".to_string(),
            ))?;

        let db_latest_block = checkpointee
            .get_block_by_number(db_latest_block_number)
            .await?
            .ok_or(CommitterError::FailedToCreateCheckpoint(
                "latest block not found in main store".to_string(),
            ))?;

        if !checkpoint_store.has_state_root(checkpoint_latest_block.header.state_root)? {
            return Err(CommitterError::FailedToCreateCheckpoint(
                "checkpoint store state is not regenerated properly".to_string(),
            ));
        }

        if checkpoint_latest_block_number != db_head_block_number {
            return Err(CommitterError::FailedToCreateCheckpoint(
                "checkpoint store latest block number does not match main store head block number after regeneration".to_string(),
            ));
        }

        if checkpoint_latest_block.hash() != db_latest_block.hash() {
            return Err(CommitterError::FailedToCreateCheckpoint(
                "checkpoint store latest block hash does not match main store latest block hash after regeneration".to_string(),
            ));
        }

        Ok((checkpoint_store, checkpoint_blockchain))
    }

    async fn send_commitment(&mut self, batch: &Batch) -> Result<H256, CommitterError> {
        let messages_merkle_root = compute_merkle_root(&batch.message_hashes);
        let last_block_hash = get_last_block_hash(&self.store, batch.last_block)?;

        let mut calldata_values = vec![
            Value::Uint(U256::from(batch.number)),
            Value::FixedBytes(batch.state_root.0.to_vec().into()),
            Value::FixedBytes(messages_merkle_root.0.to_vec().into()),
            Value::FixedBytes(batch.privileged_transactions_hash.0.to_vec().into()),
            Value::FixedBytes(last_block_hash.0.to_vec().into()),
        ];

        let (commit_function_signature, values) = if self.based {
            let mut encoded_blocks: Vec<Bytes> = Vec::new();

            let (blocks, _) = fetch_blocks_with_respective_fee_configs::<CommitterError>(
                batch.number,
                &self.store,
                &self.rollup_store,
            )
            .await?;

            for block in blocks {
                encoded_blocks.push(block.encode_to_vec().into());
            }

            calldata_values.push(Value::Array(
                encoded_blocks.into_iter().map(Value::Bytes).collect(),
            ));

            (COMMIT_FUNCTION_SIGNATURE_BASED, calldata_values)
        } else {
            (COMMIT_FUNCTION_SIGNATURE, calldata_values)
        };

        let calldata = encode_calldata(commit_function_signature, &values)?;

        let gas_price = self
            .eth_client
            .get_gas_price_with_extra(20)
            .await?
            .try_into()
            .map_err(|_| {
                CommitterError::ConversionError("Failed to convert gas_price to a u64".to_owned())
            })?;

        // Validium: EIP1559 Transaction.
        // Rollup: EIP4844 Transaction -> For on-chain Data Availability.
        let tx = if !self.validium {
            info!("L2 is in rollup mode, sending EIP-4844 (including blob) tx to commit block");
            let le_bytes = estimate_blob_gas(
                &self.eth_client,
                self.arbitrary_base_blob_gas_price,
                20, // 20% of headroom
            )
            .await?
            .to_le_bytes();

            let gas_price_per_blob = U256::from_little_endian(&le_bytes);

            build_generic_tx(
                &self.eth_client,
                TxType::EIP4844,
                self.on_chain_proposer_address,
                self.signer.address(),
                calldata.into(),
                Overrides {
                    from: Some(self.signer.address()),
                    gas_price_per_blob: Some(gas_price_per_blob),
                    max_fee_per_gas: Some(gas_price),
                    max_priority_fee_per_gas: Some(gas_price),
                    blobs_bundle: Some(batch.blobs_bundle.clone()),
                    ..Default::default()
                },
            )
            .await
            .map_err(CommitterError::from)?
        } else {
            info!("L2 is in validium mode, sending EIP-1559 (no blob) tx to commit block");
            build_generic_tx(
                &self.eth_client,
                TxType::EIP1559,
                self.on_chain_proposer_address,
                self.signer.address(),
                calldata.into(),
                Overrides {
                    from: Some(self.signer.address()),
                    max_fee_per_gas: Some(gas_price),
                    max_priority_fee_per_gas: Some(gas_price),
                    ..Default::default()
                },
            )
            .await
            .map_err(CommitterError::from)?
        };

        let commit_tx_hash =
            send_tx_bump_gas_exponential_backoff(&self.eth_client, tx, &self.signer).await?;

        metrics!(
            let commit_tx_receipt = self
                .eth_client
                .get_transaction_receipt(commit_tx_hash)
                .await?
                .ok_or(CommitterError::UnexpectedError("no commit tx receipt".to_string()))?;
            let commit_gas_used = commit_tx_receipt.tx_info.gas_used.try_into()?;
            METRICS.set_batch_commitment_gas(batch.number, commit_gas_used)?;
            if !self.validium {
                let blob_gas_used = commit_tx_receipt.tx_info.blob_gas_used
                    .ok_or(CommitterError::UnexpectedError("no blob in rollup mode".to_string()))?
                    .try_into()?;
                METRICS.set_batch_commitment_blob_gas(batch.number, blob_gas_used)?;
            }
        );

        info!("Commitment sent: {commit_tx_hash:#x}");

        Ok(commit_tx_hash)
    }

    fn stop_committer(&mut self) -> CallResponse<Self> {
        if let Some(token) = self.cancellation_token.take() {
            token.cancel();
            info!("L1 committer stopped");
            CallResponse::Reply(OutMessage::Stopped)
        } else {
            warn!("L1 committer received stop command but it is already stopped");
            CallResponse::Reply(OutMessage::Error("Already stopped".to_string()))
        }
    }

    fn start_committer(&mut self, handle: GenServerHandle<Self>, delay: u64) -> CallResponse<Self> {
        if self.cancellation_token.is_none() {
            self.schedule_commit(delay, handle);
            info!("L1 committer restarted next commit will be sent in {delay}ms");
            CallResponse::Reply(OutMessage::Started)
        } else {
            warn!("L1 committer received start command but it is already running");
            CallResponse::Reply(OutMessage::Error("Already started".to_string()))
        }
    }

    fn schedule_commit(&mut self, delay: u64, handle: GenServerHandle<Self>) {
        let check_interval = random_duration(delay);
        let handle = send_after(check_interval, handle, InMessage::Commit);
        self.cancellation_token = Some(handle.cancellation_token);
    }

    async fn health(&mut self) -> CallResponse<Self> {
        let rpc_urls = self.eth_client.test_urls().await;
        let signer_status = self.signer.health().await;

        CallResponse::Reply(OutMessage::Health(Box::new(L1CommitterHealth {
            rpc_healthcheck: rpc_urls,
            commit_time_ms: self.commit_time_ms,
            arbitrary_base_blob_gas_price: self.arbitrary_base_blob_gas_price,
            validium: self.validium,
            based: self.based,
            sequencer_state: format!("{:?}", self.sequencer_state.status().await),
            committer_wake_up_ms: self.committer_wake_up_ms,
            last_committed_batch_timestamp: self.last_committed_batch_timestamp,
            last_committed_batch: self.last_committed_batch,
            signer_status,
            running: self.cancellation_token.is_some(),
            on_chain_proposer_address: self.on_chain_proposer_address,
        })))
    }
}

impl GenServer for L1Committer {
    type CallMsg = CallMessage;
    type CastMsg = InMessage;
    type OutMsg = OutMessage;

    type Error = CommitterError;

    // Right now we only have the `Commit` message, so we ignore the `message` parameter
    async fn handle_cast(
        &mut self,
        _message: Self::CastMsg,
        handle: &GenServerHandle<Self>,
    ) -> CastResponse {
        if let SequencerStatus::Sequencing = self.sequencer_state.status().await {
            let current_last_committed_batch =
                get_last_committed_batch(&self.eth_client, self.on_chain_proposer_address)
                    .await
                    .unwrap_or(self.last_committed_batch);
            let Some(current_time) = utils::system_now_ms() else {
                self.schedule_commit(self.committer_wake_up_ms, handle.clone());
                return CastResponse::NoReply;
            };

            // In the event that the current batch in L1 is greater than the one we have recorded we shouldn't send a new batch
            if current_last_committed_batch > self.last_committed_batch {
                info!(
                    l1_batch = current_last_committed_batch,
                    last_batch_registered = self.last_committed_batch,
                    "Committer was not aware of new L1 committed batches, updating internal state accordingly"
                );
                self.last_committed_batch = current_last_committed_batch;
                self.last_committed_batch_timestamp = current_time;
                self.schedule_commit(self.committer_wake_up_ms, handle.clone());
                return CastResponse::NoReply;
            }

            let commit_time: u128 = self.commit_time_ms.into();
            let should_send_commitment =
                current_time - self.last_committed_batch_timestamp > commit_time;

            debug!(
                last_committed_batch_at = self.last_committed_batch_timestamp,
                will_send_commitment = should_send_commitment,
                last_committed_batch = self.last_committed_batch,
                "Committer woke up"
            );

            #[expect(clippy::collapsible_if)]
            if should_send_commitment {
                if self
                    .commit_next_batch_to_l1()
                    .await
                    .inspect_err(|e| error!("L1 Committer Error: {e}"))
                    .is_ok()
                {
                    self.last_committed_batch_timestamp = system_now_ms().unwrap_or(current_time);
                    self.last_committed_batch = current_last_committed_batch + 1;
                }
            }
        }
        self.schedule_commit(self.committer_wake_up_ms, handle.clone());
        CastResponse::NoReply
    }

    async fn handle_call(
        &mut self,
        message: Self::CallMsg,
        handle: &GenServerHandle<Self>,
    ) -> CallResponse<Self> {
        match message {
            CallMessage::Stop => self.stop_committer(),
            CallMessage::Start(delay) => self.start_committer(handle.clone(), delay),
            CallMessage::Health => self.health().await,
        }
    }
}

/// Generate the blob bundle necessary for the EIP-4844 transaction.
pub fn generate_blobs_bundle(
    state_diff: &StateDiff,
) -> Result<(BlobsBundle, usize), CommitterError> {
    let blob_data = state_diff.encode().map_err(CommitterError::from)?;

    let blob_size = blob_data.len();

    let blob = blobs_bundle::blob_from_bytes(blob_data).map_err(CommitterError::from)?;

    Ok((
        BlobsBundle::create_from_blobs(&vec![blob]).map_err(CommitterError::from)?,
        blob_size,
    ))
}

fn get_last_block_hash(
    store: &Store,
    last_block_number: BlockNumber,
) -> Result<H256, CommitterError> {
    store
        .get_block_header(last_block_number)?
        .map(|header| header.hash())
        .ok_or(CommitterError::RetrievalError(
            "Failed to get last block hash from storage".to_owned(),
        ))
}

/// Estimates the gas price for blob transactions based on the current state of the blockchain.
///
/// # Parameters:
/// - `eth_client`: The Ethereum client used to fetch the latest block.
/// - `arbitrary_base_blob_gas_price`: The base gas price that serves as the minimum price for blob transactions.
/// - `headroom`: Percentage applied to the estimated gas price to provide a buffer against fluctuations.
///
/// # Formula:
/// The gas price is estimated using an exponential function based on the blob gas used in the latest block and the
/// excess blob gas from the block header, following the formula from EIP-4844:
/// ```txt
///    blob_gas = arbitrary_base_blob_gas_price + (excess_blob_gas + blob_gas_used) * headroom
/// ```
async fn estimate_blob_gas(
    eth_client: &EthClient,
    arbitrary_base_blob_gas_price: u64,
    headroom: u64,
) -> Result<u64, CommitterError> {
    let latest_block = eth_client
        .get_block_by_number(BlockIdentifier::Tag(BlockTag::Latest), false)
        .await?;

    let blob_gas_used = latest_block.header.blob_gas_used.unwrap_or(0);
    let excess_blob_gas = latest_block.header.excess_blob_gas.unwrap_or(0);

    // Using the formula from the EIP-4844
    // https://eips.ethereum.org/EIPS/eip-4844
    // def get_base_fee_per_blob_gas(header: Header) -> int:
    // return fake_exponential(
    //     MIN_BASE_FEE_PER_BLOB_GAS,
    //     header.excess_blob_gas,
    //     BLOB_BASE_FEE_UPDATE_FRACTION
    // )
    //
    // factor * e ** (numerator / denominator)
    // def fake_exponential(factor: int, numerator: int, denominator: int) -> int:

    // Check if adding the blob gas used and excess blob gas would overflow
    let total_blob_gas = excess_blob_gas
        .checked_add(blob_gas_used)
        .ok_or(BlobEstimationError::OverflowError)?;

    // If the blob's market is in high demand, the equation may give a really big number.
    // This function doesn't panic, it performs checked/saturating operations.
    let blob_gas = fake_exponential_checked(
        MIN_BASE_FEE_PER_BLOB_GAS,
        total_blob_gas,
        BLOB_BASE_FEE_UPDATE_FRACTION,
    )
    .map_err(BlobEstimationError::FakeExponentialError)?;

    let gas_with_headroom = (blob_gas * (100 + headroom)) / 100;

    // Check if we have an overflow when we take the headroom into account.
    let blob_gas = arbitrary_base_blob_gas_price
        .checked_add(gas_with_headroom)
        .ok_or(BlobEstimationError::OverflowError)?;

    Ok(blob_gas)
}

/// Regenerates the state up to the head block by re-applying blocks from the
/// last known state root.
///
/// Since the path-based feature was added, the database stores the state 128
/// blocks behind the head block while the state of the blocks in between are
/// kept in in-memory-diff-layers.
///
/// After the node is shut down, those in-memory layers are lost, and the database
/// won't have the state for those blocks. It will have the blocks though.
///
/// When the node is started again, the state needs to be regenerated by
/// re-applying the blocks from the last known state root up to the head block.
///
/// This function performs that regeneration.
pub async fn regenerate_head_state(
    store: &Store,
    blockchain: &Arc<Blockchain>,
) -> Result<(), CommitterError> {
    let head_block_number = store.get_latest_block_number().await?;

    let Some(last_header) = store.get_block_header(head_block_number)? else {
        unreachable!("Database is empty, genesis block should be present");
    };

    let mut current_last_header = last_header;

    // Find the last block with a known state root
    while !store.has_state_root(current_last_header.state_root)? {
        if current_last_header.number == 0 {
            return Err(CommitterError::FailedToCreateCheckpoint(
                "unknown state found in DB. Please run `ethrex removedb` and restart node"
                    .to_string(),
            ));
        }
        let parent_number = current_last_header.number - 1;

        debug!("Need to regenerate state for block {parent_number}");

        let Some(parent_header) = store.get_block_header(parent_number)? else {
            return Err(CommitterError::FailedToCreateCheckpoint(format!(
                "parent header for block {parent_number} not found"
            )));
        };

        current_last_header = parent_header;
    }

    let last_state_number = current_last_header.number;

    if last_state_number == head_block_number {
        debug!("State is already up to date");
        return Ok(());
    }

    info!("Regenerating state from block {last_state_number} to {head_block_number}");

    // Re-apply blocks from the last known state root to the head block
    for i in (last_state_number + 1)..=head_block_number {
        debug!("Re-applying block {i} to regenerate state");

        let block = store.get_block_by_number(i).await?.ok_or_else(|| {
            CommitterError::FailedToCreateCheckpoint(format!("Block {i} not found"))
        })?;

        blockchain
            .add_block(block)
            .await
            .map_err(|err| CommitterError::FailedToCreateCheckpoint(err.to_string()))?;
    }

    info!("Finished regenerating state");

    Ok(())
}
