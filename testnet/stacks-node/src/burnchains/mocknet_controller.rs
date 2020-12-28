use std::collections::VecDeque;
use std::time::Instant;

use super::super::operations::BurnchainOpSigner;
use super::super::Config;

use super::{BurnchainController, BurnchainTip, Error as BurnchainControllerError};
use stacks::burnchains::bitcoin::BitcoinBlock;
use stacks::burnchains::{
    Burnchain, BurnchainBlock, BurnchainBlockHeader, BurnchainHeaderHash,
    BurnchainStateTransitionOps, Txid,
};
use stacks::chainstate::burn::db::sortdb::{PoxId, SortitionDB, SortitionHandleTx};
use stacks::chainstate::burn::operations::{
    leader_block_commit::BURN_BLOCK_MINED_AT_MODULUS, BlockstackOperationType, LeaderBlockCommitOp,
    LeaderKeyRegisterOp, PreStxOp, StackStxOp, TransferStxOp, UserBurnSupportOp,
};
use stacks::chainstate::burn::BlockSnapshot;
use stacks::util::get_epoch_time_secs;
use stacks::util::hash::Sha256Sum;

/// MocknetController is simulating a simplistic burnchain.
pub struct MocknetController {
    config: Config,
    burnchain: Burnchain,
    db: Option<SortitionDB>,
    chain_tip: Option<BurnchainTip>,
    queued_operations: VecDeque<BlockstackOperationType>,
}

impl MocknetController {
    pub fn generic(config: Config) -> Box<dyn BurnchainController> {
        Box::new(Self::new(config))
    }

    fn new(config: Config) -> Self {
        debug!("Opening Burnchain at {}", &config.get_burn_db_path());
        let burnchain = Burnchain::regtest(&config.get_burn_db_path());

        Self {
            config: config,
            burnchain: burnchain,
            db: None,
            queued_operations: VecDeque::new(),
            chain_tip: None,
        }
    }

    fn build_next_block_header(current_block: &BlockSnapshot) -> BurnchainBlockHeader {
        let curr_hash = &current_block.burn_header_hash.to_bytes()[..];
        let next_hash = Sha256Sum::from_data(&curr_hash);

        let block = BurnchainBlock::Bitcoin(BitcoinBlock::new(
            current_block.block_height + 1,
            &BurnchainHeaderHash::from_bytes(next_hash.as_bytes()).unwrap(),
            &current_block.burn_header_hash,
            &vec![],
            get_epoch_time_secs(),
        ));
        block.header()
    }
}

impl BurnchainController for MocknetController {
    fn sortdb_ref(&self) -> &SortitionDB {
        self.db.as_ref().expect("BUG: did not instantiate burn DB")
    }

    fn sortdb_mut(&mut self) -> &mut SortitionDB {
        match self.db {
            Some(ref mut sortdb) => sortdb,
            None => {
                unreachable!();
            }
        }
    }

    fn get_chain_tip(&mut self) -> BurnchainTip {
        match &self.chain_tip {
            Some(chain_tip) => chain_tip.clone(),
            None => {
                unreachable!();
            }
        }
    }

    fn start(
        &mut self,
        _ignored_target_height_opt: Option<u64>,
    ) -> Result<(BurnchainTip, u64), BurnchainControllerError> {
        let db = match SortitionDB::connect(
            &self.config.get_burn_db_file_path(),
            0,
            &BurnchainHeaderHash::zero(),
            get_epoch_time_secs(),
            true,
        ) {
            Ok(db) => db,
            Err(_) => panic!("Error while connecting to burnchain db"),
        };
        let block_snapshot = SortitionDB::get_canonical_burn_chain_tip(db.conn())
            .expect("FATAL: failed to get canonical chain tip");

        self.db = Some(db);

        let genesis_state = BurnchainTip {
            block_snapshot,
            state_transition: BurnchainStateTransitionOps::noop(),
            received_at: Instant::now(),
        };
        self.chain_tip = Some(genesis_state.clone());
        let block_height = genesis_state.block_snapshot.block_height;
        Ok((genesis_state, block_height))
    }

    fn submit_operation(
        &mut self,
        operation: BlockstackOperationType,
        _op_signer: &mut BurnchainOpSigner,
        _attempt: u64,
    ) -> bool {
        self.queued_operations.push_back(operation);
        true
    }

    fn sync(
        &mut self,
        _ignored_target_height_opt: Option<u64>,
    ) -> Result<(BurnchainTip, u64), BurnchainControllerError> {
        let chain_tip = self.get_chain_tip();

        // Simulating mining
        let next_block_header = Self::build_next_block_header(&chain_tip.block_snapshot);
        let mut vtxindex = 1;
        let mut ops = vec![];

        while let Some(payload) = self.queued_operations.pop_front() {
            let txid = Txid(
                Sha256Sum::from_data(
                    format!("{}::{}", next_block_header.block_height, vtxindex).as_bytes(),
                )
                .0,
            );
            let op = match payload {
                BlockstackOperationType::LeaderKeyRegister(payload) => {
                    BlockstackOperationType::LeaderKeyRegister(LeaderKeyRegisterOp {
                        consensus_hash: payload.consensus_hash,
                        public_key: payload.public_key,
                        memo: payload.memo,
                        address: payload.address,
                        txid,
                        vtxindex: vtxindex,
                        block_height: next_block_header.block_height,
                        burn_header_hash: next_block_header.block_hash,
                    })
                }
                BlockstackOperationType::LeaderBlockCommit(payload) => {
                    BlockstackOperationType::LeaderBlockCommit(LeaderBlockCommitOp {
                        sunset_burn: 0,
                        block_header_hash: payload.block_header_hash,
                        new_seed: payload.new_seed,
                        parent_block_ptr: payload.parent_block_ptr,
                        parent_vtxindex: payload.parent_vtxindex,
                        key_block_ptr: payload.key_block_ptr,
                        key_vtxindex: payload.key_vtxindex,
                        memo: payload.memo,
                        burn_fee: payload.burn_fee,
                        apparent_sender: payload.apparent_sender,
                        input: payload.input,
                        commit_outs: payload.commit_outs,
                        txid,
                        vtxindex: vtxindex,
                        block_height: next_block_header.block_height,
                        burn_parent_modulus: if next_block_header.block_height > 0 {
                            (next_block_header.block_height - 1) % BURN_BLOCK_MINED_AT_MODULUS
                        } else {
                            BURN_BLOCK_MINED_AT_MODULUS - 1
                        } as u8,
                        burn_header_hash: next_block_header.block_hash,
                    })
                }
                BlockstackOperationType::UserBurnSupport(payload) => {
                    BlockstackOperationType::UserBurnSupport(UserBurnSupportOp {
                        address: payload.address,
                        consensus_hash: payload.consensus_hash,
                        public_key: payload.public_key,
                        key_block_ptr: payload.key_block_ptr,
                        key_vtxindex: payload.key_vtxindex,
                        block_header_hash_160: payload.block_header_hash_160,
                        burn_fee: payload.burn_fee,
                        txid,
                        vtxindex: vtxindex,
                        block_height: next_block_header.block_height,
                        burn_header_hash: next_block_header.block_hash,
                    })
                }
                BlockstackOperationType::PreStx(payload) => {
                    BlockstackOperationType::PreStx(PreStxOp {
                        txid,
                        vtxindex,
                        block_height: next_block_header.block_height,
                        burn_header_hash: next_block_header.block_hash,
                        ..payload
                    })
                }
                BlockstackOperationType::TransferStx(payload) => {
                    BlockstackOperationType::TransferStx(TransferStxOp {
                        txid,
                        vtxindex,
                        block_height: next_block_header.block_height,
                        burn_header_hash: next_block_header.block_hash,
                        ..payload
                    })
                }
                BlockstackOperationType::StackStx(payload) => {
                    BlockstackOperationType::StackStx(StackStxOp {
                        txid,
                        vtxindex,
                        block_height: next_block_header.block_height,
                        burn_header_hash: next_block_header.block_hash,
                        ..payload
                    })
                }
            };
            ops.push(op);
            vtxindex += 1;
        }

        // Include txs in a new block
        let (block_snapshot, state_transition) = {
            match self.db {
                None => {
                    unreachable!();
                }
                Some(ref mut burn_db) => {
                    let mut burn_tx =
                        SortitionHandleTx::begin(burn_db, &chain_tip.block_snapshot.sortition_id)
                            .unwrap();
                    let new_chain_tip = burn_tx
                        .process_block_ops(
                            &self.burnchain,
                            &chain_tip.block_snapshot,
                            &next_block_header,
                            ops,
                            None,
                            PoxId::stubbed(),
                            None,
                            0,
                        )
                        .unwrap();
                    burn_tx.commit().unwrap();
                    new_chain_tip
                }
            }
        };

        let state_transition = BurnchainStateTransitionOps {
            accepted_ops: state_transition.accepted_ops,
            consumed_leader_keys: state_transition.consumed_leader_keys,
        };

        // Transmit the new state
        let new_state = BurnchainTip {
            block_snapshot,
            state_transition,
            received_at: Instant::now(),
        };
        self.chain_tip = Some(new_state.clone());

        let block_height = new_state.block_snapshot.block_height;
        Ok((new_state, block_height))
    }

    #[cfg(test)]
    fn bootstrap_chain(&mut self, _num_blocks: u64) {}
}
