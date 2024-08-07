use std::collections::VecDeque;
use std::num::NonZeroUsize;

use anyhow::Context;
use p2p::client::types::UnverifiedStateUpdateData;
use p2p::PeerData;
use pathfinder_common::state_update::{self, ContractClassUpdate, ContractUpdate, StateUpdateData};
use pathfinder_common::{
    BlockHash,
    BlockHeader,
    BlockNumber,
    ClassCommitment,
    StarknetVersion,
    StateCommitment,
    StateDiffCommitment,
    StateUpdate,
    StorageCommitment,
};
use pathfinder_merkle_tree::contract_state::{update_contract_state, ContractStateUpdateResult};
use pathfinder_merkle_tree::StorageCommitmentTree;
use pathfinder_storage::{Storage, TrieUpdate};
use tokio::task::spawn_blocking;

use crate::state::update_starknet_state;
use crate::sync::error::{SyncError, SyncError2};
use crate::sync::stream::ProcessStage;

/// Returns the first block number whose state update is missing, counting from
/// genesis or `None` if all class definitions up to `head` are present.
pub(super) async fn next_missing(
    storage: Storage,
    head: BlockNumber,
) -> anyhow::Result<Option<BlockNumber>> {
    spawn_blocking(move || {
        let mut db = storage
            .connection()
            .context("Creating database connection")?;
        let db = db.transaction().context("Creating database transaction")?;

        let highest = db
            .highest_block_with_state_update()
            .context("Querying highest block with state update")?;

        match highest {
            // No state updates at all, start from genesis
            None => Ok((head != BlockNumber::GENESIS).then_some(BlockNumber::GENESIS)),
            // Otherwise start from the next block
            Some(highest) => Ok((highest < head).then_some(highest + 1)),
        }
    })
    .await
    .context("Joining blocking task")?
}

pub(super) fn length_and_commitment_stream(
    storage: Storage,
    mut start: BlockNumber,
    stop: BlockNumber,
) -> impl futures::Stream<Item = anyhow::Result<(usize, StateDiffCommitment)>> {
    const BATCH_SIZE: usize = 1000;

    async_stream::try_stream! {
        let mut batch = VecDeque::new();

        while start <= stop {
            if let Some(counts) = batch.pop_front() {
                yield counts;
                continue;
            }

            let batch_size = NonZeroUsize::new(
                BATCH_SIZE.min(
                    (stop.get() - start.get() + 1)
                        .try_into()
                        .expect("ptr size is 64bits"),
                ),
            )
            .expect(">0");
            let storage = storage.clone();

            batch = tokio::task::spawn_blocking(move || {
                let mut db = storage
                    .connection()
                    .context("Creating database connection")?;
                let db = db.transaction().context("Creating database transaction")?;
                db.state_diff_lengths_and_commitments(start, batch_size)
                    .context("Querying state update counts")
            })
            .await
            .context("Joining blocking task")??;

            if batch.is_empty() {
                Err(anyhow::anyhow!(
                    "No state update counts found for range: start {start}, batch_size {batch_size}"
                ))?;
                break;
            }

            start += batch.len().try_into().expect("ptr size is 64bits");
        }

        while let Some(counts) = batch.pop_front() {
            yield counts;
        }
    }
}

pub struct VerifyCommitment;

impl ProcessStage for VerifyCommitment {
    const NAME: &'static str = "StateDiff::Verify";
    type Input = (UnverifiedStateUpdateData, StarknetVersion);
    type Output = StateUpdateData;

    fn map(&mut self, input: Self::Input) -> Result<Self::Output, SyncError2> {
        let (
            UnverifiedStateUpdateData {
                expected_commitment,
                state_diff,
            },
            version,
        ) = input;
        let actual = state_diff.compute_state_diff_commitment(version);

        if actual != expected_commitment {
            return Err(SyncError2::StateDiffCommitmentMismatch);
        }

        Ok(state_diff)
    }
}

pub struct UpdateStarknetState {
    pub storage: pathfinder_storage::Storage,
    pub connection: pathfinder_storage::Connection,
    pub current_block: BlockNumber,
    pub verify_tree_hashes: bool,
}

impl ProcessStage for UpdateStarknetState {
    type Input = StateUpdateData;
    type Output = BlockNumber;

    const NAME: &'static str = "StateDiff::UpdateStarknetState";

    fn map(&mut self, state_update: Self::Input) -> Result<Self::Output, SyncError2> {
        let mut db = self
            .connection
            .transaction()
            .context("Creating database transaction")?;

        let tail = self.current_block;

        let header = db
            .block_header(tail.into())
            .context("Querying block header")?
            .context("Block header not found")?;
        let parent_state_commitment = match self.current_block.parent() {
            Some(parent) => db
                .state_commitment(parent.into())
                .context("Querying parent block header")?
                .context("Parent block header not found")?,
            None => StateCommitment::default(),
        };
        let state_update = StateUpdate {
            block_hash: header.hash,
            parent_state_commitment,
            state_commitment: header.state_commitment,
            contract_updates: state_update.contract_updates,
            system_contract_updates: state_update.system_contract_updates,
            declared_cairo_classes: state_update.declared_cairo_classes,
            declared_sierra_classes: state_update.declared_sierra_classes,
        };

        let (storage_commitment, class_commitment) = update_starknet_state(
            &db,
            &state_update,
            self.verify_tree_hashes,
            header.number,
            self.storage.clone(),
        )
        .context("Updating Starknet state")?;
        let state_commitment = StateCommitment::calculate(storage_commitment, class_commitment);
        // Ensure that roots match.
        if state_commitment != header.state_commitment {
            return Err(SyncError2::StateRootMismatch);
        }

        db.update_storage_and_class_commitments(tail, storage_commitment, class_commitment)
            .context("Updating storage commitment")?;
        db.insert_state_update(self.current_block, &state_update)
            .context("Inserting state update data")?;
        db.commit().context("Committing db transaction")?;

        self.current_block += 1;

        Ok(tail)
    }
}
