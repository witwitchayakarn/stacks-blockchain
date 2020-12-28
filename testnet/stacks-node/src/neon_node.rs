use super::{BurnchainController, BurnchainTip, Config, EventDispatcher, Keychain};
use crate::config::HELIUM_BLOCK_LIMIT;
use crate::run_loop::RegisteredKey;
use std::collections::HashMap;

use std::cmp;
use std::collections::{HashSet, VecDeque};
use std::convert::{TryFrom, TryInto};
use std::default::Default;
use std::net::SocketAddr;
use std::{thread, thread::JoinHandle};

use stacks::burnchains::{Burnchain, BurnchainHeaderHash, BurnchainParameters, Txid};
use stacks::chainstate::burn::db::sortdb::{SortitionDB, SortitionId};
use stacks::chainstate::burn::operations::{
    leader_block_commit::{RewardSetInfo, BURN_BLOCK_MINED_AT_MODULUS},
    BlockstackOperationType, LeaderBlockCommitOp, LeaderKeyRegisterOp,
};
use stacks::chainstate::burn::BlockSnapshot;
use stacks::chainstate::burn::{BlockHeaderHash, ConsensusHash, VRFSeed};
use stacks::chainstate::stacks::db::{ChainStateBootData, ClarityTx, StacksChainState};
use stacks::chainstate::stacks::Error as ChainstateError;
use stacks::chainstate::stacks::StacksPublicKey;
use stacks::chainstate::stacks::{miner::StacksMicroblockBuilder, StacksBlockBuilder};
use stacks::chainstate::stacks::{
    CoinbasePayload, StacksAddress, StacksBlock, StacksBlockHeader, StacksMicroblock,
    StacksTransaction, StacksTransactionSigner, TransactionAnchorMode, TransactionPayload,
    TransactionVersion,
};
use stacks::core::mempool::MemPoolDB;
use stacks::net::{
    atlas::{AtlasDB, AttachmentInstance},
    db::{LocalPeer, PeerDB},
    dns::DNSResolver,
    p2p::PeerNetwork,
    relay::Relayer,
    rpc::RPCHandlerArgs,
    Error as NetError, NetworkResult, PeerAddress, StacksMessageCodec,
};
use stacks::util::get_epoch_time_ms;
use stacks::util::get_epoch_time_secs;
use stacks::util::hash::{to_hex, Hash160, Sha256Sum};
use stacks::util::secp256k1::Secp256k1PrivateKey;
use stacks::util::strings::UrlString;
use stacks::util::vrf::VRFPublicKey;
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};

use crate::burnchains::bitcoin_regtest_controller::BitcoinRegtestController;
use crate::syncctl::PoxSyncWatchdogComms;

use crate::ChainTip;
use stacks::burnchains::BurnchainSigner;
use stacks::core::FIRST_BURNCHAIN_CONSENSUS_HASH;

use stacks::chainstate::coordinator::comm::CoordinatorChannels;
use stacks::chainstate::coordinator::{get_next_recipients, OnChainRewardSetProvider};

use stacks::monitoring::{increment_stx_blocks_mined_counter, update_active_miners_count_gauge};

use crate::burn_fee::read_burn_fee;

pub const TESTNET_CHAIN_ID: u32 = 0x80000000;
pub const TESTNET_PEER_VERSION: u32 = 0xfacade01;
pub const RELAYER_MAX_BUFFER: usize = 100;

struct AssembledAnchorBlock {
    parent_consensus_hash: ConsensusHash,
    my_burn_hash: BurnchainHeaderHash,
    anchored_block: StacksBlock,
    attempt: u64,
}

struct MicroblockMinerState {
    parent_consensus_hash: ConsensusHash,
    parent_block_hash: BlockHeaderHash,
    miner_key: Secp256k1PrivateKey,
    frequency: u64,
    last_mined: u128,
    quantity: u64,
}

enum RelayerDirective {
    HandleNetResult(NetworkResult),
    ProcessTenure(ConsensusHash, BurnchainHeaderHash, BlockHeaderHash),
    RunTenure(RegisteredKey, BlockSnapshot),
    RegisterKey(BlockSnapshot),
    BroadcastMicroblock(ConsensusHash, BlockHeaderHash, StacksMicroblock),
}

pub struct InitializedNeonNode {
    relay_channel: SyncSender<RelayerDirective>,
    burnchain_signer: BurnchainSigner,
    last_burn_block: Option<BlockSnapshot>,
    active_keys: Vec<RegisteredKey>,
    sleep_before_tenure: u64,
    is_miner: bool,
}

pub struct NeonGenesisNode {
    pub config: Config,
    keychain: Keychain,
    event_dispatcher: EventDispatcher,
    burnchain: Burnchain,
}

#[cfg(test)]
type BlocksProcessedCounter = std::sync::Arc<std::sync::atomic::AtomicU64>;

#[cfg(not(test))]
type BlocksProcessedCounter = ();

#[cfg(test)]
fn bump_processed_counter(blocks_processed: &BlocksProcessedCounter) {
    blocks_processed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
}

#[cfg(not(test))]
fn bump_processed_counter(_blocks_processed: &BlocksProcessedCounter) {}

/// Process artifacts from the tenure.
/// At this point, we're modifying the chainstate, and merging the artifacts from the previous tenure.
fn inner_process_tenure(
    anchored_block: &StacksBlock,
    consensus_hash: &ConsensusHash,
    parent_consensus_hash: &ConsensusHash,
    burn_db: &mut SortitionDB,
    chain_state: &mut StacksChainState,
    coord_comms: &CoordinatorChannels,
) -> Result<bool, ChainstateError> {
    let stacks_blocks_processed = coord_comms.get_stacks_blocks_processed();

    let ic = burn_db.index_conn();

    // Preprocess the anchored block
    chain_state.preprocess_anchored_block(
        &ic,
        consensus_hash,
        &anchored_block,
        &parent_consensus_hash,
        0,
    )?;

    if !coord_comms.announce_new_stacks_block() {
        return Ok(false);
    }
    if !coord_comms.wait_for_stacks_blocks_processed(stacks_blocks_processed, 15000) {
        warn!("ChainsCoordinator timed out while waiting for new stacks block to be processed");
    }

    Ok(true)
}

fn inner_generate_coinbase_tx(keychain: &mut Keychain, nonce: u64) -> StacksTransaction {
    let mut tx_auth = keychain.get_transaction_auth().unwrap();
    tx_auth.set_origin_nonce(nonce);

    let mut tx = StacksTransaction::new(
        TransactionVersion::Testnet,
        tx_auth,
        TransactionPayload::Coinbase(CoinbasePayload([0u8; 32])),
    );
    tx.chain_id = TESTNET_CHAIN_ID;
    tx.anchor_mode = TransactionAnchorMode::OnChainOnly;
    let mut tx_signer = StacksTransactionSigner::new(&tx);
    keychain.sign_as_origin(&mut tx_signer);

    tx_signer.get_tx().unwrap()
}

fn inner_generate_poison_microblock_tx(
    keychain: &mut Keychain,
    nonce: u64,
    poison_payload: TransactionPayload,
) -> StacksTransaction {
    let mut tx_auth = keychain.get_transaction_auth().unwrap();
    tx_auth.set_origin_nonce(nonce);

    let mut tx = StacksTransaction::new(TransactionVersion::Testnet, tx_auth, poison_payload);
    tx.chain_id = TESTNET_CHAIN_ID;
    tx.anchor_mode = TransactionAnchorMode::OnChainOnly;
    let mut tx_signer = StacksTransactionSigner::new(&tx);
    keychain.sign_as_origin(&mut tx_signer);

    tx_signer.get_tx().unwrap()
}

/// Constructs and returns a LeaderKeyRegisterOp out of the provided params
fn inner_generate_leader_key_register_op(
    address: StacksAddress,
    vrf_public_key: VRFPublicKey,
    consensus_hash: &ConsensusHash,
) -> BlockstackOperationType {
    BlockstackOperationType::LeaderKeyRegister(LeaderKeyRegisterOp {
        public_key: vrf_public_key,
        memo: vec![],
        address,
        consensus_hash: consensus_hash.clone(),
        vtxindex: 0,
        txid: Txid([0u8; 32]),
        block_height: 0,
        burn_header_hash: BurnchainHeaderHash::zero(),
    })
}

fn rotate_vrf_and_register(
    keychain: &mut Keychain,
    burn_block: &BlockSnapshot,
    btc_controller: &mut BitcoinRegtestController,
) -> bool {
    let vrf_pk = keychain.rotate_vrf_keypair(burn_block.block_height);
    let burnchain_tip_consensus_hash = &burn_block.consensus_hash;
    let op = inner_generate_leader_key_register_op(
        keychain.get_address(),
        vrf_pk,
        burnchain_tip_consensus_hash,
    );

    let mut one_off_signer = keychain.generate_op_signer();
    btc_controller.submit_operation(op, &mut one_off_signer, 1)
}

/// Constructs and returns a LeaderBlockCommitOp out of the provided params
fn inner_generate_block_commit_op(
    sender: BurnchainSigner,
    block_header_hash: BlockHeaderHash,
    burn_fee: u64,
    key: &RegisteredKey,
    parent_burnchain_height: u32,
    parent_winning_vtx: u16,
    vrf_seed: VRFSeed,
    commit_outs: Vec<StacksAddress>,
    sunset_burn: u64,
    current_burn_height: u64,
) -> BlockstackOperationType {
    let (parent_block_ptr, parent_vtxindex) = (parent_burnchain_height, parent_winning_vtx);
    let burn_parent_modulus = (current_burn_height % BURN_BLOCK_MINED_AT_MODULUS) as u8;

    BlockstackOperationType::LeaderBlockCommit(LeaderBlockCommitOp {
        sunset_burn,
        block_header_hash,
        burn_fee,
        input: (Txid([0; 32]), 0),
        apparent_sender: sender,
        key_block_ptr: key.block_height as u32,
        key_vtxindex: key.op_vtxindex as u16,
        memo: vec![],
        new_seed: vrf_seed,
        parent_block_ptr,
        parent_vtxindex,
        vtxindex: 0,
        txid: Txid([0u8; 32]),
        block_height: 0,
        burn_header_hash: BurnchainHeaderHash::zero(),
        burn_parent_modulus,
        commit_outs,
    })
}

/// Mine and broadcast a single microblock, unconditionally.
/// Note that the StacksChainState here **must** be the **same** StacksChainState that gets
/// maintained by the peer network thread!
fn mine_one_microblock(
    microblock_state: &mut MicroblockMinerState,
    sortdb: &SortitionDB,
    chainstate: &mut StacksChainState,
    mempool: &MemPoolDB,
) -> Result<StacksMicroblock, NetError> {
    debug!(
        "Try to mine one microblock off of {}/{} (at seq {})",
        &microblock_state.parent_consensus_hash,
        &microblock_state.parent_block_hash,
        chainstate
            .unconfirmed_state
            .as_ref()
            .map(|us| us.last_mblock_seq)
            .unwrap_or(0)
    );
    let mint_result = {
        let ic = sortdb.index_conn();
        let mut microblock_miner =
            match StacksMicroblockBuilder::resume_unconfirmed(chainstate, &ic) {
                Ok(x) => x,
                Err(e) => {
                    let msg = format!(
                        "Failed to create a microblock miner at chaintip {}/{}: {:?}",
                        &microblock_state.parent_consensus_hash,
                        &microblock_state.parent_block_hash,
                        &e
                    );
                    error!("{}", msg);
                    return Err(NetError::ChainstateError(msg));
                }
            };

        let mblock = microblock_miner.mine_next_microblock(mempool, &microblock_state.miner_key)?;

        info!("Minted microblock with {} transactions", mblock.txs.len());

        Ok(mblock)
    };

    let mined_microblock = match mint_result {
        Ok(mined_microblock) => mined_microblock,
        Err(e) => {
            warn!("Failed to mine microblock: {}", e);
            return Err(e);
        }
    };

    // preprocess the microblock locally
    chainstate
        .preprocess_streamed_microblock(
            &microblock_state.parent_consensus_hash,
            &microblock_state.parent_block_hash,
            &mined_microblock,
        )
        .map_err(|e| {
            error!(
                "Error while pre-processing microblock {}: {}",
                mined_microblock.header.block_hash(),
                e
            );
            NetError::ChainstateError(format!("{:?}", &e))
        })?;

    microblock_state.quantity += 1;
    return Ok(mined_microblock);
}

fn try_mine_microblock(
    config: &Config,
    microblock_miner_state: &mut Option<MicroblockMinerState>,
    chainstate: &mut StacksChainState,
    sortdb: &SortitionDB,
    mem_pool: &MemPoolDB,
    coord_comms: &CoordinatorChannels,
    miner_tip_arc: Arc<Mutex<Option<(ConsensusHash, BlockHeaderHash, Secp256k1PrivateKey)>>>,
) -> Result<Option<StacksMicroblock>, NetError> {
    let mut next_microblock = None;
    let winning_tip_opt = match miner_tip_arc.lock() {
        Ok(tip_opt) => tip_opt.clone(),
        Err(e) => {
            // can only happen due to a thread panic in the relayer
            error!("FATAL: miner tip arc mutex is poisoned: {:?}", &e);
            panic!();
        }
    };
    if microblock_miner_state.is_none() {
        // are we the current sortition winner?  Do we need to instantiate?
        if let Some((ch, bhh, microblock_privkey)) = winning_tip_opt.as_ref() {
            debug!(
                "Instantiate microblock mining state off of {}/{}",
                &ch, &bhh
            );
            // we won a block! proceed to build a microblock tail if we've stored it
            match StacksChainState::get_anchored_block_header_info(chainstate.db(), ch, bhh) {
                Ok(Some(_)) => {
                    microblock_miner_state.replace(MicroblockMinerState {
                        parent_consensus_hash: ch.clone(),
                        parent_block_hash: bhh.clone(),
                        miner_key: microblock_privkey.clone(),
                        frequency: config.node.microblock_frequency,
                        last_mined: 0,
                        quantity: 0,
                    });
                }
                Ok(None) => {
                    warn!(
                        "No such anchored block: {}/{}.  Cannot mine microblocks",
                        ch, bhh
                    );
                }
                Err(e) => {
                    warn!(
                        "Failed to get anchored block cost for {}/{}: {:?}",
                        ch, bhh, &e
                    );
                }
            }
        }
    }

    if let Some((ch, bhh, ..)) = winning_tip_opt.as_ref() {
        if let Some(mut microblock_miner) = microblock_miner_state.take() {
            if microblock_miner.parent_consensus_hash == *ch
                && microblock_miner.parent_block_hash == *bhh
            {
                if microblock_miner.last_mined + (microblock_miner.frequency as u128)
                    < get_epoch_time_ms()
                {
                    // opportunistically try and mine, but only if there's no attachable blocks
                    let num_attachable =
                        StacksChainState::count_attachable_staging_blocks(chainstate.db(), 1, 0)?;
                    if num_attachable == 0 {
                        match mine_one_microblock(
                            &mut microblock_miner,
                            sortdb,
                            chainstate,
                            &mem_pool,
                        ) {
                            Ok(microblock) => {
                                // will need to relay this
                                next_microblock = Some(microblock);
                            }
                            Err(e) => {
                                warn!("Failed to mine one microblock: {:?}", &e);
                            }
                        }
                    }
                }
                microblock_miner.last_mined = get_epoch_time_ms();
                microblock_miner_state.replace(microblock_miner);
            }
            // otherwise, we're not the sortition winner, and the microblock miner state can be
            // discarded.
        }
    } else {
        // no longer a winning tip
        let _ = microblock_miner_state.take();
    }

    Ok(next_microblock)
}

fn spawn_peer(
    mut this: PeerNetwork,
    p2p_sock: &SocketAddr,
    rpc_sock: &SocketAddr,
    config: Config,
    poll_timeout: u64,
    relay_channel: SyncSender<RelayerDirective>,
    coord_comms: CoordinatorChannels,
    mut sync_comms: PoxSyncWatchdogComms,
    miner_tip_arc: Arc<Mutex<Option<(ConsensusHash, BlockHeaderHash, Secp256k1PrivateKey)>>>,
    attachments_rx: Receiver<HashSet<AttachmentInstance>>,
) -> Result<JoinHandle<()>, NetError> {
    let burn_db_path = config.get_burn_db_file_path();
    let stacks_chainstate_path = config.get_chainstate_path();
    let block_limit = config.block_limit.clone();
    let exit_at_block_height = config.burnchain.process_exit_at_block_height;

    this.bind(p2p_sock, rpc_sock).unwrap();
    let (mut dns_resolver, mut dns_client) = DNSResolver::new(10);
    let sortdb = SortitionDB::open(&burn_db_path, false).map_err(NetError::DBError)?;

    let (mut chainstate, _) = StacksChainState::open_with_block_limit(
        false,
        TESTNET_CHAIN_ID,
        &stacks_chainstate_path,
        block_limit,
    )
    .map_err(|e| NetError::ChainstateError(e.to_string()))?;

    let mut mem_pool = MemPoolDB::open(false, TESTNET_CHAIN_ID, &stacks_chainstate_path)
        .map_err(NetError::DBError)?;

    // buffer up blocks to store without stalling the p2p thread
    let mut results_with_data = VecDeque::new();

    // microblock miner state
    let mut microblock_miner_state = None;

    let server_thread = thread::spawn(move || {
        let handler_args = RPCHandlerArgs {
            exit_at_block_height: exit_at_block_height.as_ref(),
            genesis_chainstate_hash: Sha256Sum::from_hex(stx_genesis::GENESIS_CHAINSTATE_HASH)
                .unwrap(),
            ..RPCHandlerArgs::default()
        };

        let mut disconnected = false;
        let mut num_p2p_state_machine_passes = 0;
        let mut num_inv_sync_passes = 0;

        while !disconnected {
            let download_backpressure = results_with_data.len() > 0;
            let poll_ms = if !download_backpressure && this.has_more_downloads() {
                // keep getting those blocks -- drive the downloader state-machine
                debug!(
                    "P2P: backpressure: {}, more downloads: {}",
                    download_backpressure,
                    this.has_more_downloads()
                );
                100
            } else {
                cmp::min(poll_timeout, config.node.microblock_frequency)
            };

            // Mine microblocks.
            // NOTE: this has to go *here* because control over who can refresh the unconfirmed
            // state (or mutate it in general) *must* reside within the same thread as the p2p
            // thread, so that the p2p thread's in-RAM MARF state stays in-sync with the DB.
            //
            // If you don't do this, you'll get runtime panics in the RPC code due to the network
            // thread's in-RAM view of the unconfirmed chain state trie (namely, the rowid of the
            // trie) being out-of-sync with what's actually in the DB.  An attempt to read from
            // the MARF may lead to a panic, because there may not be a trie on-disk with the
            // network code's rowid any longer in the case where *some other thread* modifies the
            // unconfirmed state trie and invalidates the network thread's in-RAM copy of the trie
            // rowid.
            //
            // Fortunately, microblock-mining isn't a very CPU or I/O-intensive process, and the
            // node operator can bound how expensive each microblock can be in order to limit the
            // amount of time the microblock miner spends mining.
            //
            // Once the Clarity DB has been refactored to be safe to write to by multiple threads
            // concurrently, then microblock mining can be moved to the relayer thread (where it
            // really ought to occur, since microblock mining can be both CPU- and I/O-intensive).
            let next_microblock = match try_mine_microblock(
                &config,
                &mut microblock_miner_state,
                &mut chainstate,
                &sortdb,
                &mem_pool,
                &coord_comms,
                miner_tip_arc.clone(),
            ) {
                Ok(x) => x,
                Err(e) => {
                    warn!("Failed to mine next microblock: {:?}", &e);
                    None
                }
            };

            let (canonical_consensus_tip, canonical_block_tip) =
                SortitionDB::get_canonical_stacks_chain_tip_hash(sortdb.conn())
                    .expect("Failed to read canonical stacks chain tip");

            let mut expected_attachments = match attachments_rx.try_recv() {
                Ok(expected_attachments) => expected_attachments,
                _ => {
                    debug!("Atlas: attachment channel is empty");
                    HashSet::new()
                }
            };

            let network_result = match this.run(
                &sortdb,
                &mut chainstate,
                &mut mem_pool,
                Some(&mut dns_client),
                download_backpressure,
                poll_ms,
                &handler_args,
                &mut expected_attachments,
            ) {
                Ok(res) => res,
                Err(e) => {
                    error!("P2P: Failed to process network dispatch: {:?}", &e);
                    panic!();
                }
            };

            if num_p2p_state_machine_passes < network_result.num_state_machine_passes {
                // p2p state-machine did a full pass. Notify anyone listening.
                sync_comms.notify_p2p_state_pass();
                num_p2p_state_machine_passes = network_result.num_state_machine_passes;
            }

            if num_inv_sync_passes < network_result.num_inv_sync_passes {
                // inv-sync state-machine did a full pass. Notify anyone listening.
                sync_comms.notify_inv_sync_pass();
                num_inv_sync_passes = network_result.num_inv_sync_passes;
            }

            if network_result.has_data_to_store() {
                results_with_data.push_back(RelayerDirective::HandleNetResult(network_result));
            }
            if let Some(microblock) = next_microblock {
                results_with_data.push_back(RelayerDirective::BroadcastMicroblock(
                    canonical_consensus_tip,
                    canonical_block_tip,
                    microblock,
                ));
            }

            while let Some(next_result) = results_with_data.pop_front() {
                // have blocks, microblocks, and/or transactions (don't care about anything else),
                if let Err(e) = relay_channel.try_send(next_result) {
                    debug!(
                        "P2P: {:?}: download backpressure detected",
                        &this.local_peer
                    );
                    match e {
                        TrySendError::Full(directive) => {
                            // don't lose this data -- just try it again
                            results_with_data.push_front(directive);
                            break;
                        }
                        TrySendError::Disconnected(_) => {
                            info!("P2P: Relayer hang up with p2p channel");
                            disconnected = true;
                            break;
                        }
                    }
                } else {
                    debug!("P2P: Dispatched result to Relayer!");
                }
            }
        }
        debug!("P2P thread exit!");
    });

    let _jh = thread::spawn(move || {
        dns_resolver.thread_main();
    });

    Ok(server_thread)
}

fn spawn_miner_relayer(
    mut relayer: Relayer,
    local_peer: LocalPeer,
    config: Config,
    mut keychain: Keychain,
    burn_db_path: String,
    stacks_chainstate_path: String,
    relay_channel: Receiver<RelayerDirective>,
    event_dispatcher: EventDispatcher,
    blocks_processed: BlocksProcessedCounter,
    burnchain: Burnchain,
    coord_comms: CoordinatorChannels,
    miner_tip_arc: Arc<Mutex<Option<(ConsensusHash, BlockHeaderHash, Secp256k1PrivateKey)>>>,
) -> Result<(), NetError> {
    // Note: the relayer is *the* block processor, it is responsible for writes to the chainstate --
    //   no other codepaths should be writing once this is spawned.
    //
    // the relayer _should not_ be modifying the sortdb,
    //   however, it needs a mut reference to create read TXs.
    //   should address via #1449
    let mut sortdb = SortitionDB::open(&burn_db_path, true).map_err(NetError::DBError)?;

    let (mut chainstate, _) = StacksChainState::open_with_block_limit(
        false,
        TESTNET_CHAIN_ID,
        &stacks_chainstate_path,
        config.block_limit.clone(),
    )
    .map_err(|e| NetError::ChainstateError(e.to_string()))?;

    let mut mem_pool = MemPoolDB::open(false, TESTNET_CHAIN_ID, &stacks_chainstate_path)
        .map_err(NetError::DBError)?;

    let mut last_mined_blocks: HashMap<
        BurnchainHeaderHash,
        Vec<(AssembledAnchorBlock, Secp256k1PrivateKey)>,
    > = HashMap::new();
    let burn_fee_cap = config.burnchain.burn_fee_cap;

    let mut bitcoin_controller = BitcoinRegtestController::new_dummy(config.clone());

    let _relayer_handle = thread::spawn(move || {
        let mut did_register_key = false;
        let mut key_registered_at_block = 0;
        while let Ok(mut directive) = relay_channel.recv() {
            match directive {
                RelayerDirective::HandleNetResult(ref mut net_result) => {
                    debug!("Relayer: Handle network result");
                    let net_receipts = relayer
                        .process_network_result(
                            &local_peer,
                            net_result,
                            &mut sortdb,
                            &mut chainstate,
                            &mut mem_pool,
                            Some(&coord_comms),
                        )
                        .expect("BUG: failure processing network results");

                    let mempool_txs_added = net_receipts.mempool_txs_added.len();
                    if mempool_txs_added > 0 {
                        event_dispatcher.process_new_mempool_txs(net_receipts.mempool_txs_added);
                    }

                    // Dispatch retrieved attachments, if any.
                    if net_result.has_attachments() {
                        event_dispatcher.process_new_attachments(&net_result.attachments);
                    }
                }
                RelayerDirective::ProcessTenure(consensus_hash, burn_hash, block_header_hash) => {
                    debug!(
                        "Relayer: Process tenure {}/{} in {}",
                        &consensus_hash, &block_header_hash, &burn_hash
                    );
                    if let Some(last_mined_blocks_at_burn_hash) =
                        last_mined_blocks.remove(&burn_hash)
                    {
                        for (last_mined_block, microblock_privkey) in
                            last_mined_blocks_at_burn_hash.into_iter()
                        {
                            let AssembledAnchorBlock {
                                parent_consensus_hash,
                                anchored_block: mined_block,
                                my_burn_hash: mined_burn_hash,
                                attempt: _,
                            } = last_mined_block;
                            if mined_block.block_hash() == block_header_hash
                                && burn_hash == mined_burn_hash
                            {
                                // we won!
                                info!("Won sortition!";
                                      "stacks_header" => %block_header_hash,
                                      "burn_hash" => %mined_burn_hash,
                                );

                                increment_stx_blocks_mined_counter();

                                match inner_process_tenure(
                                    &mined_block,
                                    &consensus_hash,
                                    &parent_consensus_hash,
                                    &mut sortdb,
                                    &mut chainstate,
                                    &coord_comms,
                                ) {
                                    Ok(coordinator_running) => {
                                        if !coordinator_running {
                                            warn!(
                                                "Coordinator stopped, stopping relayer thread..."
                                            );
                                            return;
                                        }
                                    }
                                    Err(e) => {
                                        warn!(
                                            "Error processing my tenure, bad block produced: {}",
                                            e
                                        );
                                        warn!(
                                            "Bad block";
                                            "stacks_header" => %block_header_hash,
                                            "data" => %to_hex(&mined_block.serialize_to_vec()),
                                        );
                                        continue;
                                    }
                                };

                                // advertize _and_ push blocks for now
                                let blocks_available = Relayer::load_blocks_available_data(
                                    &sortdb,
                                    vec![consensus_hash.clone()],
                                )
                                .expect("Failed to obtain block information for a block we mined.");
                                if let Err(e) = relayer.advertize_blocks(blocks_available) {
                                    warn!("Failed to advertise new block: {}", e);
                                }

                                let snapshot = SortitionDB::get_block_snapshot_consensus(
                                    sortdb.conn(),
                                    &consensus_hash,
                                )
                                .expect("Failed to obtain snapshot for block")
                                .expect("Failed to obtain snapshot for block");
                                if !snapshot.pox_valid {
                                    warn!(
                                        "Snapshot for {} is no longer valid; discarding {}...",
                                        &consensus_hash,
                                        &mined_block.block_hash()
                                    );
                                } else {
                                    let ch = snapshot.consensus_hash.clone();
                                    let bh = mined_block.block_hash();

                                    if let Err(e) = relayer
                                        .broadcast_block(snapshot.consensus_hash, mined_block)
                                    {
                                        warn!("Failed to push new block: {}", e);
                                    }

                                    // proceed to mine microblocks, via the p2p thread
                                    match miner_tip_arc.lock() {
                                        Ok(mut tip) => *tip = Some((ch, bh, microblock_privkey)),
                                        Err(e) => {
                                            // can only happen if the p2p thread panics while holding
                                            // the lock.
                                            error!("FATAL: miner tip arc is poisoned: {:?}", &e);
                                            break;
                                        }
                                    }
                                }
                            } else {
                                debug!("Did not win sortition, my blocks [burn_hash= {}, block_hash= {}], their blocks [parent_consenus_hash= {}, burn_hash= {}, block_hash ={}]",
                                  mined_burn_hash, mined_block.block_hash(), parent_consensus_hash, burn_hash, block_header_hash);

                                match miner_tip_arc.lock() {
                                    Ok(mut tip) => *tip = None,
                                    Err(e) => {
                                        // can only happen if the p2p thread panics while holding
                                        // the lock.
                                        error!("FATAL: miner tip arc is poisoned: {:?}", &e);
                                        break;
                                    }
                                }
                            }
                        }
                    }
                }
                RelayerDirective::RunTenure(registered_key, last_burn_block) => {
                    let burn_header_hash = last_burn_block.burn_header_hash.clone();
                    debug!(
                        "Relayer: Run tenure";
                        "height" => last_burn_block.block_height,
                        "burn_header_hash" => %burn_header_hash
                    );
                    let mut last_mined_blocks_vec = last_mined_blocks
                        .remove(&burn_header_hash)
                        .unwrap_or_default();

                    let last_mined_block_opt = InitializedNeonNode::relayer_run_tenure(
                        &config,
                        registered_key,
                        &mut chainstate,
                        &mut sortdb,
                        &burnchain,
                        last_burn_block,
                        &mut keychain,
                        &mut mem_pool,
                        burn_fee_cap,
                        &mut bitcoin_controller,
                        &last_mined_blocks_vec.iter().map(|(blk, _)| blk).collect(),
                    );
                    if let Some((last_mined_block, microblock_privkey)) = last_mined_block_opt {
                        if last_mined_blocks_vec.len() == 0 {
                            // (for testing) only bump once per epoch
                            bump_processed_counter(&blocks_processed);
                        }
                        last_mined_blocks_vec.push((last_mined_block, microblock_privkey));
                    }
                    last_mined_blocks.insert(burn_header_hash, last_mined_blocks_vec);
                }
                RelayerDirective::RegisterKey(ref last_burn_block) => {
                    // Ensure that we're submitting this one time per block.
                    if did_register_key && key_registered_at_block == last_burn_block.block_height {
                        debug!("Relayer: Received RegisterKey directive - ignoring");
                        continue;
                    }
                    did_register_key = rotate_vrf_and_register(
                        &mut keychain,
                        last_burn_block,
                        &mut bitcoin_controller,
                    );
                    if did_register_key {
                        key_registered_at_block = last_burn_block.block_height;
                    }
                    bump_processed_counter(&blocks_processed);
                }
                RelayerDirective::BroadcastMicroblock(
                    parent_consensus_hash,
                    parent_block_hash,
                    microblock,
                ) => {
                    let microblock_hash = microblock.block_hash();
                    if let Err(e) = relayer.broadcast_microblock(
                        &parent_consensus_hash,
                        &parent_block_hash,
                        microblock,
                    ) {
                        error!(
                            "Failure trying to broadcast microblock {}: {}",
                            microblock_hash, e
                        );
                    }
                }
            }
        }
        debug!("Relayer exit!");
    });

    Ok(())
}

impl InitializedNeonNode {
    fn new(
        config: Config,
        keychain: Keychain,
        event_dispatcher: EventDispatcher,
        last_burn_block: Option<BurnchainTip>,
        miner: bool,
        blocks_processed: BlocksProcessedCounter,
        coord_comms: CoordinatorChannels,
        sync_comms: PoxSyncWatchdogComms,
        burnchain: Burnchain,
        attachments_rx: Receiver<HashSet<AttachmentInstance>>,
    ) -> InitializedNeonNode {
        // we can call _open_ here rather than _connect_, since connect is first called in
        //   make_genesis_block
        let sortdb = SortitionDB::open(&config.get_burn_db_file_path(), false)
            .expect("Error while instantiating sortition db");

        let view = {
            let ic = sortdb.index_conn();
            let sortition_tip = SortitionDB::get_canonical_burn_chain_tip(&ic)
                .expect("Failed to get sortition tip");
            ic.get_burnchain_view(&burnchain, &sortition_tip).unwrap()
        };

        // create a new peerdb
        let data_url = UrlString::try_from(format!("{}", &config.node.data_url)).unwrap();
        let mut initial_neighbors = vec![];
        if let Some(ref bootstrap_node) = &config.node.bootstrap_node {
            initial_neighbors.push(bootstrap_node.clone());
        }

        println!("BOOTSTRAP WITH {:?}", initial_neighbors);

        let p2p_sock: SocketAddr = config.node.p2p_bind.parse().expect(&format!(
            "Failed to parse socket: {}",
            &config.node.p2p_bind
        ));
        let rpc_sock = config.node.rpc_bind.parse().expect(&format!(
            "Failed to parse socket: {}",
            &config.node.rpc_bind
        ));
        let p2p_addr: SocketAddr = config.node.p2p_address.parse().expect(&format!(
            "Failed to parse socket: {}",
            &config.node.p2p_address
        ));
        let node_privkey = {
            let mut re_hashed_seed = config.node.local_peer_seed.clone();
            let my_private_key = loop {
                match Secp256k1PrivateKey::from_slice(&re_hashed_seed[..]) {
                    Ok(sk) => break sk,
                    Err(_) => {
                        re_hashed_seed = Sha256Sum::from_data(&re_hashed_seed[..])
                            .as_bytes()
                            .to_vec()
                    }
                }
            };
            my_private_key
        };

        let mut peerdb = PeerDB::connect(
            &config.get_peer_db_path(),
            true,
            TESTNET_CHAIN_ID,
            burnchain.network_id,
            Some(node_privkey),
            config.connection_options.private_key_lifetime.clone(),
            PeerAddress::from_socketaddr(&p2p_addr),
            p2p_sock.port(),
            data_url.clone(),
            &vec![],
            Some(&initial_neighbors),
        )
        .unwrap();

        println!("DENY NEIGHBORS {:?}", &config.node.deny_nodes);
        {
            let mut tx = peerdb.tx_begin().unwrap();
            for denied in config.node.deny_nodes.iter() {
                PeerDB::set_deny_peer(
                    &mut tx,
                    denied.addr.network_id,
                    &denied.addr.addrbytes,
                    denied.addr.port,
                    get_epoch_time_secs() + 24 * 365 * 3600,
                )
                .unwrap();
            }
            tx.commit().unwrap();
        }
        let atlasdb = AtlasDB::connect(&config.get_atlas_db_path(), true).unwrap();

        let local_peer = match PeerDB::get_local_peer(peerdb.conn()) {
            Ok(local_peer) => local_peer,
            _ => panic!("Unable to retrieve local peer"),
        };

        // now we're ready to instantiate a p2p network object, the relayer, and the event dispatcher
        let mut p2p_net = PeerNetwork::new(
            peerdb,
            atlasdb,
            local_peer.clone(),
            TESTNET_PEER_VERSION,
            burnchain.clone(),
            view,
            config.connection_options.clone(),
        );

        // setup the relayer channel
        let (relay_send, relay_recv) = sync_channel(RELAYER_MAX_BUFFER);

        let burnchain_signer = keychain.get_burnchain_signer();
        let relayer = Relayer::from_p2p(&mut p2p_net);

        let sleep_before_tenure = config.node.wait_time_for_microblocks;

        // set up shared flag to indicate whether or not the node has won a sortition, so
        // microblock mining can commense
        let miner_tip_arc = Arc::new(Mutex::new(None));

        spawn_miner_relayer(
            relayer,
            local_peer,
            config.clone(),
            keychain,
            config.get_burn_db_file_path(),
            config.get_chainstate_path(),
            relay_recv,
            event_dispatcher,
            blocks_processed.clone(),
            burnchain,
            coord_comms.clone(),
            miner_tip_arc.clone(),
        )
        .expect("Failed to initialize mine/relay thread");

        spawn_peer(
            p2p_net,
            &p2p_sock,
            &rpc_sock,
            config.clone(),
            5000,
            relay_send.clone(),
            coord_comms,
            sync_comms,
            miner_tip_arc.clone(),
            attachments_rx,
        )
        .expect("Failed to initialize mine/relay thread");

        info!("Bound HTTP server on: {}", &config.node.rpc_bind);
        info!("Bound P2P server on: {}", &config.node.p2p_bind);

        let last_burn_block = last_burn_block.map(|x| x.block_snapshot);

        let is_miner = miner;

        let active_keys = vec![];

        InitializedNeonNode {
            relay_channel: relay_send,
            last_burn_block,
            burnchain_signer,
            is_miner,
            sleep_before_tenure,
            active_keys,
        }
    }

    /// Tell the relayer to fire off a tenure and a block commit op.
    pub fn relayer_issue_tenure(&mut self) -> bool {
        if !self.is_miner {
            // node is a follower, don't try to issue a tenure
            return true;
        }

        if let Some(burnchain_tip) = self.last_burn_block.clone() {
            if let Some(key) = self.active_keys.first() {
                debug!("Using key {:?}", &key.vrf_public_key);
                // sleep a little before building the anchor block, to give any broadcasted
                //   microblocks time to propagate.
                thread::sleep(std::time::Duration::from_millis(self.sleep_before_tenure));
                self.relay_channel
                    .send(RelayerDirective::RunTenure(key.clone(), burnchain_tip))
                    .is_ok()
            } else {
                warn!("Skipped tenure because no active VRF key. Trying to register one.");
                self.relay_channel
                    .send(RelayerDirective::RegisterKey(burnchain_tip))
                    .is_ok()
            }
        } else {
            warn!("Do not know the last burn block. As a miner, this is bad.");
            true
        }
    }

    /// Notify the relayer of a sortition, telling it to process the block
    ///  and advertize it if it was mined by the node.
    /// returns _false_ if the relayer hung up the channel.
    pub fn relayer_sortition_notify(&self) -> bool {
        if !self.is_miner {
            // node is a follower, don't try to process my own tenure.
            return true;
        }

        if let Some(ref snapshot) = &self.last_burn_block {
            if snapshot.sortition {
                return self
                    .relay_channel
                    .send(RelayerDirective::ProcessTenure(
                        snapshot.consensus_hash.clone(),
                        snapshot.parent_burn_header_hash.clone(),
                        snapshot.winning_stacks_block_hash.clone(),
                    ))
                    .is_ok();
            }
        }
        true
    }

    // return stack's parent's burn header hash,
    //        the anchored block,
    //        the burn header hash of the burnchain tip
    fn relayer_run_tenure(
        config: &Config,
        registered_key: RegisteredKey,
        chain_state: &mut StacksChainState,
        burn_db: &mut SortitionDB,
        burnchain: &Burnchain,
        burn_block: BlockSnapshot,
        keychain: &mut Keychain,
        mem_pool: &mut MemPoolDB,
        burn_fee_cap: u64,
        bitcoin_controller: &mut BitcoinRegtestController,
        last_mined_blocks: &Vec<&AssembledAnchorBlock>,
    ) -> Option<(AssembledAnchorBlock, Secp256k1PrivateKey)> {
        let (
            mut stacks_parent_header,
            parent_consensus_hash,
            parent_block_burn_height,
            parent_block_total_burn,
            parent_winning_vtxindex,
            coinbase_nonce,
        ) = if let Some(stacks_tip) = chain_state.get_stacks_chain_tip(burn_db).unwrap() {
            let stacks_tip_header = match StacksChainState::get_anchored_block_header_info(
                chain_state.db(),
                &stacks_tip.consensus_hash,
                &stacks_tip.anchored_block_hash,
            )
            .unwrap()
            {
                Some(x) => x,
                None => {
                    error!("Could not mine new tenure, since could not find header for known chain tip.");
                    return None;
                }
            };

            // the consensus hash of my Stacks block parent
            let parent_consensus_hash = stacks_tip.consensus_hash.clone();

            // the stacks block I'm mining off of's burn header hash and vtxindex:
            let parent_snapshot = SortitionDB::get_block_snapshot_consensus(
                burn_db.conn(),
                &stacks_tip.consensus_hash,
            )
            .expect("Failed to look up block's parent snapshot")
            .expect("Failed to look up block's parent snapshot");

            let parent_sortition_id = &parent_snapshot.sortition_id;
            let parent_winning_vtxindex =
                match SortitionDB::get_block_winning_vtxindex(burn_db.conn(), parent_sortition_id)
                    .expect("SortitionDB failure.")
                {
                    Some(x) => x,
                    None => {
                        warn!(
                            "Failed to find winning vtx index for the parent sortition {}",
                            parent_sortition_id
                        );
                        return None;
                    }
                };

            let parent_block =
                match SortitionDB::get_block_snapshot(burn_db.conn(), parent_sortition_id)
                    .expect("SortitionDB failure.")
                {
                    Some(x) => x,
                    None => {
                        warn!(
                            "Failed to find block snapshot for the parent sortition {}",
                            parent_sortition_id
                        );
                        return None;
                    }
                };

            // don't mine off of an old burnchain block
            let burn_chain_tip = SortitionDB::get_canonical_burn_chain_tip(burn_db.conn())
                .expect("FATAL: failed to query sortition DB for canonical burn chain tip");

            if burn_chain_tip.consensus_hash != burn_block.consensus_hash {
                debug!("New canonical burn chain tip detected: {} ({}) > {} ({}). Will not try to mine.", burn_chain_tip.consensus_hash, burn_chain_tip.block_height, &burn_block.consensus_hash, &burn_block.block_height);
                return None;
            }

            debug!("Mining tenure's last consensus hash: {} (height {} hash {}), stacks tip consensus hash: {} (height {} hash {})",
                       &burn_block.consensus_hash, burn_block.block_height, &burn_block.burn_header_hash,
                       &stacks_tip.consensus_hash, parent_snapshot.block_height, &parent_snapshot.burn_header_hash);

            let coinbase_nonce = {
                let principal = keychain.origin_address().unwrap().into();
                let account = chain_state
                    .with_read_only_clarity_tx(
                        &burn_db.index_conn(),
                        &StacksBlockHeader::make_index_block_hash(
                            &stacks_tip.consensus_hash,
                            &stacks_tip.anchored_block_hash,
                        ),
                        |conn| StacksChainState::get_account(conn, &principal),
                    )
                    .expect(&format!(
                        "BUG: stacks tip block {}/{} no longer exists after we queried it",
                        &stacks_tip.consensus_hash, &stacks_tip.anchored_block_hash
                    ));
                account.nonce
            };

            (
                stacks_tip_header,
                parent_consensus_hash,
                parent_block.block_height,
                parent_block.total_burn,
                parent_winning_vtxindex,
                coinbase_nonce,
            )
        } else {
            warn!("No Stacks chain tip known, attempting to mine a genesis block");
            let (network, _) = config.burnchain.get_bitcoin_network();
            let burnchain_params =
                BurnchainParameters::from_params(&config.burnchain.chain, &network)
                    .expect("Bitcoin network unsupported");

            let chain_tip = ChainTip::genesis(
                config.get_initial_liquid_ustx(),
                &burnchain_params.first_block_hash,
                burnchain_params.first_block_height.into(),
                burnchain_params.first_block_timestamp.into(),
            );

            (
                chain_tip.metadata,
                FIRST_BURNCHAIN_CONSENSUS_HASH.clone(),
                0,
                0,
                0,
                0,
            )
        };

        // has the tip changed from our previously-mined block for this epoch?
        let attempt = {
            let mut best_attempt = 0;
            debug!(
                "Consider {} in-flight Stacks tip(s)",
                &last_mined_blocks.len()
            );
            for prev_block in last_mined_blocks.iter() {
                debug!(
                    "Consider in-flight Stacks tip {}/{} in {}",
                    &prev_block.parent_consensus_hash,
                    &prev_block.anchored_block.header.parent_block,
                    &prev_block.my_burn_hash
                );
                if prev_block.parent_consensus_hash == parent_consensus_hash
                    && prev_block.my_burn_hash == burn_block.burn_header_hash
                    && prev_block.anchored_block.header.parent_block
                        == stacks_parent_header.anchored_header.block_hash()
                {
                    // the anchored chain tip hasn't changed since we attempted to build a block.
                    // But, have discovered any new microblocks worthy of being mined?
                    if let Ok(Some(stream)) =
                        StacksChainState::load_descendant_staging_microblock_stream(
                            chain_state.db(),
                            &StacksBlockHeader::make_index_block_hash(
                                &prev_block.parent_consensus_hash,
                                &stacks_parent_header.anchored_header.block_hash(),
                            ),
                            0,
                            u16::MAX,
                        )
                    {
                        if (prev_block.anchored_block.header.parent_microblock
                            == BlockHeaderHash([0u8; 32])
                            && stream.len() == 0)
                            || (prev_block.anchored_block.header.parent_microblock
                                != BlockHeaderHash([0u8; 32])
                                && stream.len()
                                    <= (prev_block.anchored_block.header.parent_microblock_sequence
                                        as usize)
                                        + 1)
                        {
                            // the chain tip hasn't changed since we attempted to build a block.  Use what we
                            // already have.
                            debug!("Stacks tip is unchanged since we last tried to mine a block ({}/{} at height {} with {} txs, in {} at burn height {}), and no new microblocks ({} <= {})",
                                   &prev_block.parent_consensus_hash, &prev_block.anchored_block.block_hash(), prev_block.anchored_block.header.total_work.work,
                                   prev_block.anchored_block.txs.len(), prev_block.my_burn_hash, parent_block_burn_height, stream.len(), prev_block.anchored_block.header.parent_microblock_sequence);

                            return None;
                        } else {
                            // there are new microblocks!
                            // TODO: only consider rebuilding our anchored block if we (a) have
                            // time, and (b) the new microblocks are worth more than the new BTC
                            // fee minus the old BTC fee
                            debug!("Stacks tip is unchanged since we last tried to mine a block ({}/{} at height {} with {} txs, in {} at burn height {}), but there are new microblocks ({} > {})",
                                   &prev_block.parent_consensus_hash, &prev_block.anchored_block.block_hash(), prev_block.anchored_block.header.total_work.work,
                                   prev_block.anchored_block.txs.len(), prev_block.my_burn_hash, parent_block_burn_height, stream.len(), prev_block.anchored_block.header.parent_microblock_sequence);

                            best_attempt = cmp::max(best_attempt, prev_block.attempt);
                        }
                    } else {
                        // no microblock stream to confirm, and the stacks tip hasn't changed
                        debug!("Stacks tip is unchanged since we last tried to mine a block ({}/{} at height {} with {} txs, in {} at burn height {}), and no microblocks present",
                               &prev_block.parent_consensus_hash, &prev_block.anchored_block.block_hash(), prev_block.anchored_block.header.total_work.work,
                               prev_block.anchored_block.txs.len(), prev_block.my_burn_hash, parent_block_burn_height);

                        return None;
                    }
                } else {
                    debug!("Stacks tip has changed since we last tried to mine a block in {} at burn height {}; attempt was {} (for {}/{})",
                           prev_block.my_burn_hash, parent_block_burn_height, prev_block.attempt, &prev_block.parent_consensus_hash, &prev_block.anchored_block.header.parent_block);
                    best_attempt = cmp::max(best_attempt, prev_block.attempt);
                }
            }
            best_attempt + 1
        };

        // Generates a proof out of the sortition hash provided in the params.
        let vrf_proof = match keychain.generate_proof(
            &registered_key.vrf_public_key,
            burn_block.sortition_hash.as_bytes(),
        ) {
            Some(vrfp) => vrfp,
            None => {
                // Try to recover a key registered in a former session.
                // registered_key.block_height gives us a pointer to the height of the block
                // holding the key register op, but the VRF was derived using the height of one
                // of the parents blocks.
                let _ = keychain.rotate_vrf_keypair(registered_key.block_height - 1);
                match keychain.generate_proof(
                    &registered_key.vrf_public_key,
                    burn_block.sortition_hash.as_bytes(),
                ) {
                    Some(vrfp) => vrfp,
                    None => {
                        error!(
                            "Failed to generate proof with {:?}",
                            &registered_key.vrf_public_key
                        );
                        return None;
                    }
                }
            }
        };

        debug!(
            "Generated VRF Proof: {} over {} with key {}",
            vrf_proof.to_hex(),
            &burn_block.sortition_hash,
            &registered_key.vrf_public_key.to_hex()
        );

        // Generates a new secret key for signing the trail of microblocks
        // of the upcoming tenure.
        let microblock_secret_key = if attempt > 1 {
            match keychain.get_microblock_key() {
                Some(k) => k,
                None => {
                    error!(
                        "Failed to obtain microblock key for mining attempt";
                        "attempt" => %attempt
                    );
                    return None;
                }
            }
        } else {
            keychain.rotate_microblock_keypair(burn_block.block_height)
        };
        let mblock_pubkey_hash =
            Hash160::from_node_public_key(&StacksPublicKey::from_private(&microblock_secret_key));

        let coinbase_tx = inner_generate_coinbase_tx(keychain, coinbase_nonce);

        // find the longest microblock tail we can build off of
        let microblock_info_opt =
            match StacksChainState::load_descendant_staging_microblock_stream_with_poison(
                chain_state.db(),
                &StacksBlockHeader::make_index_block_hash(
                    &parent_consensus_hash,
                    &stacks_parent_header.anchored_header.block_hash(),
                ),
                0,
                u16::MAX,
            ) {
                Ok(x) => x,
                Err(e) => {
                    warn!(
                        "Failed to load descendant microblock stream from {}/{}: {:?}",
                        &parent_consensus_hash,
                        &stacks_parent_header.anchored_header.block_hash(),
                        &e
                    );
                    None
                }
            };

        if let Some((microblocks, poison_opt)) = microblock_info_opt {
            if let Some(ref tail) = microblocks.last() {
                debug!(
                    "Confirm microblock stream tailed at {} (seq {})",
                    &tail.block_hash(),
                    tail.header.sequence
                );
            }

            stacks_parent_header.microblock_tail =
                microblocks.last().clone().map(|blk| blk.header.clone());

            if let Some(poison_payload) = poison_opt {
                let poison_microblock_tx = inner_generate_poison_microblock_tx(
                    keychain,
                    coinbase_nonce + 1,
                    poison_payload,
                );

                // submit the poison payload, privately, so we'll mine it when building the
                // anchored block.
                if let Err(e) = mem_pool.submit(
                    chain_state,
                    &parent_consensus_hash,
                    &stacks_parent_header.anchored_header.block_hash(),
                    poison_microblock_tx,
                ) {
                    warn!(
                        "Detected but failed to mine poison-microblock transaction: {:?}",
                        &e
                    );
                }
            }
        }

        let (anchored_block, _, _) = match StacksBlockBuilder::build_anchored_block(
            chain_state,
            &burn_db.index_conn(),
            mem_pool,
            &stacks_parent_header,
            parent_block_total_burn,
            vrf_proof.clone(),
            mblock_pubkey_hash,
            &coinbase_tx,
            HELIUM_BLOCK_LIMIT.clone(),
        ) {
            Ok(block) => block,
            Err(e) => {
                error!("Failure mining anchored block: {}", e);
                return None;
            }
        };

        info!(
            "{} block assembled: {}, with {} txs, attempt {}",
            if parent_block_total_burn == 0 {
                "Genesis"
            } else {
                "Stacks"
            },
            anchored_block.block_hash(),
            anchored_block.txs.len(),
            attempt
        );

        // let's figure out the recipient set!
        let recipients = match get_next_recipients(
            &burn_block,
            chain_state,
            burn_db,
            burnchain,
            &OnChainRewardSetProvider(),
        ) {
            Ok(x) => x,
            Err(e) => {
                error!("Failure fetching recipient set: {:?}", e);
                return None;
            }
        };

        let dyn_burn_fee_cap = read_burn_fee();
        let sunset_burn = burnchain.expected_sunset_burn(burn_block.block_height + 1, dyn_burn_fee_cap);
        let rest_commit = dyn_burn_fee_cap - sunset_burn;
        info!("BURN-FEE: In relayer_run_tenure, burn_fee_cap: {}, dyn_burn_fee_cap: {}, sunset_burn: {}, rest_commit: {}", burn_fee_cap, dyn_burn_fee_cap, sunset_burn, rest_commit);

        let commit_outs = if burn_block.block_height + 1 < burnchain.pox_constants.sunset_end
            && !burnchain.is_in_prepare_phase(burn_block.block_height + 1)
        {
            RewardSetInfo::into_commit_outs(recipients, false)
        } else {
            vec![StacksAddress::burn_address(false)]
        };

        // let's commit
        let op = inner_generate_block_commit_op(
            keychain.get_burnchain_signer(),
            anchored_block.block_hash(),
            rest_commit,
            &registered_key,
            parent_block_burn_height
                .try_into()
                .expect("Could not convert parent block height into u32"),
            parent_winning_vtxindex,
            VRFSeed::from_proof(&vrf_proof),
            commit_outs,
            sunset_burn,
            burn_block.block_height,
        );
        let mut op_signer = keychain.generate_op_signer();
        debug!(
            "Submit block-commit for block {} off of {}/{}",
            &anchored_block.block_hash(),
            &parent_consensus_hash,
            &anchored_block.header.parent_block
        );

        let res = bitcoin_controller.submit_operation(op, &mut op_signer, attempt);
        if !res {
            warn!("Failed to submit Bitcoin transaction");
            return None;
        }

        Some((
            AssembledAnchorBlock {
                parent_consensus_hash: parent_consensus_hash,
                my_burn_hash: burn_block.burn_header_hash,
                anchored_block,
                attempt,
            },
            microblock_secret_key,
        ))
    }

    /// Process a state coming from the burnchain, by extracting the validated KeyRegisterOp
    /// and inspecting if a sortition was won.
    /// `ibd`: boolean indicating whether or not we are in the initial block download
    pub fn process_burnchain_state(
        &mut self,
        sortdb: &SortitionDB,
        sort_id: &SortitionId,
        ibd: bool,
    ) -> Option<BlockSnapshot> {
        let mut last_sortitioned_block = None;

        let ic = sortdb.index_conn();

        let block_snapshot = SortitionDB::get_block_snapshot(&ic, sort_id)
            .expect("Failed to obtain block snapshot for processed burn block.")
            .expect("Failed to obtain block snapshot for processed burn block.");
        let block_height = block_snapshot.block_height;

        let block_commits =
            SortitionDB::get_block_commits_by_block(&ic, &block_snapshot.sortition_id)
                .expect("Unexpected SortitionDB error fetching block commits");

        update_active_miners_count_gauge(block_commits.len() as i64);

        for op in block_commits.into_iter() {
            if op.txid == block_snapshot.winning_block_txid {
                info!(
                    "Received burnchain block #{} including block_commit_op (winning) - {} ({})",
                    block_height,
                    op.apparent_sender.to_testnet_address(),
                    &op.block_header_hash
                );
                last_sortitioned_block = Some((block_snapshot.clone(), op.vtxindex));
            } else {
                if self.is_miner {
                    info!(
                        "Received burnchain block #{} including block_commit_op - {} ({})",
                        block_height,
                        op.apparent_sender.to_testnet_address(),
                        &op.block_header_hash
                    );
                }
            }
        }

        let key_registers =
            SortitionDB::get_leader_keys_by_block(&ic, &block_snapshot.sortition_id)
                .expect("Unexpected SortitionDB error fetching key registers");

        for op in key_registers.into_iter() {
            if self.is_miner {
                info!(
                    "Received burnchain block #{} including key_register_op - {}",
                    block_height, op.address
                );
            }
            if op.address == Keychain::address_from_burnchain_signer(&self.burnchain_signer) {
                if !ibd {
                    // not in initial block download, so we're not just replaying an old key.
                    // Registered key has been mined
                    self.active_keys.push(RegisteredKey {
                        vrf_public_key: op.public_key,
                        block_height: op.block_height as u64,
                        op_vtxindex: op.vtxindex as u32,
                    });
                }
            }
        }

        // no-op on UserBurnSupport ops are not supported / produced at this point.
        self.last_burn_block = Some(block_snapshot);

        last_sortitioned_block.map(|x| x.0)
    }
}

impl NeonGenesisNode {
    /// Instantiate and initialize a new node, given a config
    pub fn new(
        config: Config,
        mut event_dispatcher: EventDispatcher,
        burnchain: Burnchain,
        boot_block_exec: Box<dyn FnOnce(&mut ClarityTx) -> ()>,
    ) -> Self {
        let keychain = Keychain::default(config.node.seed.clone());
        let initial_balances = config
            .initial_balances
            .iter()
            .map(|e| (e.address.clone(), e.amount))
            .collect();

        let mut boot_data =
            ChainStateBootData::new(&burnchain, initial_balances, Some(boot_block_exec));

        // do the initial open!
        let (_chain_state, receipts) = match StacksChainState::open_and_exec(
            false,
            TESTNET_CHAIN_ID,
            &config.get_chainstate_path(),
            Some(&mut boot_data),
            config.block_limit.clone(),
        ) {
            Ok(res) => res,
            Err(err) => panic!(
                "Error while opening chain state at path {}: {:?}",
                config.get_chainstate_path(),
                err
            ),
        };

        event_dispatcher.process_boot_receipts(receipts);

        Self {
            keychain,
            config,
            event_dispatcher,
            burnchain,
        }
    }

    pub fn into_initialized_leader_node(
        self,
        burnchain_tip: BurnchainTip,
        blocks_processed: BlocksProcessedCounter,
        coord_comms: CoordinatorChannels,
        sync_comms: PoxSyncWatchdogComms,
        attachments_rx: Receiver<HashSet<AttachmentInstance>>,
    ) -> InitializedNeonNode {
        let config = self.config;
        let keychain = self.keychain;
        let event_dispatcher = self.event_dispatcher;

        InitializedNeonNode::new(
            config,
            keychain,
            event_dispatcher,
            Some(burnchain_tip),
            true,
            blocks_processed,
            coord_comms,
            sync_comms,
            self.burnchain,
            attachments_rx,
        )
    }

    pub fn into_initialized_node(
        self,
        burnchain_tip: BurnchainTip,
        blocks_processed: BlocksProcessedCounter,
        coord_comms: CoordinatorChannels,
        sync_comms: PoxSyncWatchdogComms,
        attachments_rx: Receiver<HashSet<AttachmentInstance>>,
    ) -> InitializedNeonNode {
        let config = self.config;
        let keychain = self.keychain;
        let event_dispatcher = self.event_dispatcher;

        InitializedNeonNode::new(
            config,
            keychain,
            event_dispatcher,
            Some(burnchain_tip),
            false,
            blocks_processed,
            coord_comms,
            sync_comms,
            self.burnchain,
            attachments_rx,
        )
    }
}
