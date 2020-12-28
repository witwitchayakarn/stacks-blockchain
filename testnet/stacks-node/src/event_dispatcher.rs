use stacks::chainstate::coordinator::BlockEventDispatcher;
use stacks::chainstate::stacks::db::StacksHeaderInfo;
use stacks::chainstate::stacks::StacksBlock;
use stacks::net::atlas::AttachmentInstance;
use std::collections::hash_map::Entry;
use std::thread::sleep;
use std::time::Duration;
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
};

use async_h1::client;
use async_std::net::TcpStream;
use http_types::{Method, Request, Url};

use serde_json::json;

use stacks::burnchains::{BurnchainHeaderHash, Txid};
use stacks::chainstate::stacks::events::{
    FTEventType, NFTEventType, STXEventType, StacksTransactionEvent, StacksTransactionReceipt,
    TransactionOrigin,
};
use stacks::chainstate::stacks::{
    db::accounts::MinerReward, db::MinerRewardInfo, StacksAddress, StacksBlockId, StacksTransaction,
};
use stacks::net::StacksMessageCodec;
use stacks::util::hash::bytes_to_hex;
use stacks::vm::analysis::contract_interface_builder::build_contract_interface;
use stacks::vm::types::{AssetIdentifier, QualifiedContractIdentifier, Value};

use super::config::{EventKeyType, EventObserverConfig};
use super::node::ChainTip;

#[derive(Debug, Clone)]
struct EventObserver {
    endpoint: String,
}

const STATUS_RESP_TRUE: &str = "success";
const STATUS_RESP_NOT_COMMITTED: &str = "abort_by_response";
const STATUS_RESP_POST_CONDITION: &str = "abort_by_post_condition";

pub const PATH_MEMPOOL_TX_SUBMIT: &str = "new_mempool_tx";
pub const PATH_BURN_BLOCK_SUBMIT: &str = "new_burn_block";
pub const PATH_BLOCK_PROCESSED: &str = "new_block";
pub const PATH_ATTACHMENT_PROCESSED: &str = "attachments/new";

impl EventObserver {
    fn send_payload(&self, payload: &serde_json::Value, path: &str) {
        let body = match serde_json::to_vec(&payload) {
            Ok(body) => body,
            Err(err) => {
                error!("Event dispatcher: serialization failed  - {:?}", err);
                return;
            }
        };

        let url = {
            let joined_components = match path.starts_with("/") {
                true => format!("{}{}", &self.endpoint, path),
                false => format!("{}/{}", &self.endpoint, path),
            };
            let url = format!("http://{}", joined_components);
            Url::parse(&url).expect(&format!(
                "Event dispatcher: unable to parse {} as a URL",
                url
            ))
        };

        let backoff = Duration::from_millis((1.0 * 1_000.0) as u64);

        loop {
            let body = body.clone();
            let mut req = Request::new(Method::Post, url.clone());
            req.append_header("Content-Type", "application/json")
                .expect("Unable to set header");
            req.set_body(body);

            let response = async_std::task::block_on(async {
                let stream = match TcpStream::connect(self.endpoint.clone()).await {
                    Ok(stream) => stream,
                    Err(err) => {
                        println!("Event dispatcher: connection failed  - {:?}", err);
                        return None;
                    }
                };

                match client::connect(stream, req).await {
                    Ok(response) => Some(response),
                    Err(err) => {
                        println!("Event dispatcher: rpc invokation failed  - {:?}", err);
                        return None;
                    }
                }
            });

            if let Some(response) = response {
                if response.status().is_success() {
                    break;
                } else {
                    error!(
                        "Event dispatcher: POST {} failed with error {:?}",
                        self.endpoint, response
                    );
                }
            }
            sleep(backoff);
        }
    }

    fn make_new_mempool_txs_payload(transactions: Vec<StacksTransaction>) -> serde_json::Value {
        let raw_txs = transactions
            .into_iter()
            .map(|tx| {
                serde_json::Value::String(format!("0x{}", &bytes_to_hex(&tx.serialize_to_vec())))
            })
            .collect();

        serde_json::Value::Array(raw_txs)
    }

    fn make_new_burn_block_payload(
        burn_block: &BurnchainHeaderHash,
        burn_block_height: u64,
        rewards: Vec<(StacksAddress, u64)>,
        burns: u64,
    ) -> serde_json::Value {
        let reward_recipients = rewards
            .into_iter()
            .map(|(stx_addr, amt)| {
                json!({
                    "recipient": stx_addr.to_b58(),
                    "amt": amt,
                })
            })
            .collect();

        json!({
            "burn_block_hash": format!("0x{}", burn_block),
            "burn_block_height": burn_block_height,
            "reward_recipients": serde_json::Value::Array(reward_recipients),
            "burn_amount": burns
        })
    }

    fn make_new_block_txs_payload(
        receipt: &StacksTransactionReceipt,
        tx_index: u32,
    ) -> serde_json::Value {
        let tx = &receipt.transaction;

        let success = match (receipt.post_condition_aborted, &receipt.result) {
            (false, Value::Response(response_data)) => {
                if response_data.committed {
                    STATUS_RESP_TRUE
                } else {
                    STATUS_RESP_NOT_COMMITTED
                }
            }
            (true, Value::Response(_)) => STATUS_RESP_POST_CONDITION,
            _ => unreachable!(), // Transaction results should always be a Value::Response type
        };

        let (txid, raw_tx) = match tx {
            TransactionOrigin::Burn(txid) => (txid.to_string(), "00".to_string()),
            TransactionOrigin::Stacks(ref tx) => {
                let txid = tx.txid().to_string();
                let bytes = tx.serialize_to_vec();
                (txid, bytes_to_hex(&bytes))
            }
        };

        let raw_result = {
            let bytes = receipt.result.serialize_to_vec();
            bytes_to_hex(&bytes)
        };
        let contract_interface_json = {
            match &receipt.contract_analysis {
                Some(analysis) => json!(build_contract_interface(analysis)),
                None => json!(null),
            }
        };
        json!({
            "txid": format!("0x{}", &txid),
            "tx_index": tx_index,
            "status": success,
            "raw_result": format!("0x{}", &raw_result),
            "raw_tx": format!("0x{}", &raw_tx),
            "contract_abi": contract_interface_json,
        })
    }

    fn make_new_attachment_payload(attachment: &AttachmentInstance) -> serde_json::Value {
        json!(attachment)
    }

    fn send_new_attachments(&self, payload: &serde_json::Value) {
        self.send_payload(payload, PATH_ATTACHMENT_PROCESSED);
    }

    fn send_new_mempool_txs(&self, payload: &serde_json::Value) {
        self.send_payload(payload, PATH_MEMPOOL_TX_SUBMIT);
    }

    fn send_new_burn_block(&self, payload: &serde_json::Value) {
        self.send_payload(payload, PATH_BURN_BLOCK_SUBMIT);
    }

    fn send(
        &self,
        filtered_events: Vec<(usize, &(bool, Txid, &StacksTransactionEvent))>,
        chain_tip: &ChainTip,
        parent_index_hash: &StacksBlockId,
        boot_receipts: &Vec<StacksTransactionReceipt>,
        winner_txid: &Txid,
        mature_rewards: &serde_json::Value,
    ) {
        // Serialize events to JSON
        let serialized_events: Vec<serde_json::Value> = filtered_events
            .iter()
            .map(|(event_index, (committed, txid, event))| {
                event.json_serialize(*event_index, txid, *committed)
            })
            .collect();

        let mut tx_index: u32 = 0;
        let mut serialized_txs = vec![];

        for receipt in chain_tip.receipts.iter().chain(boot_receipts.iter()) {
            let payload = EventObserver::make_new_block_txs_payload(receipt, tx_index);
            serialized_txs.push(payload);
            tx_index += 1;
        }

        // Wrap events
        let payload = json!({
            "block_hash": format!("0x{}", chain_tip.block.block_hash()),
            "block_height": chain_tip.metadata.block_height,
            "burn_block_hash": format!("0x{}", chain_tip.metadata.burn_header_hash),
            "burn_block_height": chain_tip.metadata.burn_header_height,
            "miner_txid": format!("0x{}", winner_txid),
            "burn_block_time": chain_tip.metadata.burn_header_timestamp,
            "index_block_hash": format!("0x{}", chain_tip.metadata.index_block_hash()),
            "parent_block_hash": format!("0x{}", chain_tip.block.header.parent_block),
            "parent_index_block_hash": format!("0x{}", parent_index_hash),
            "parent_microblock": format!("0x{}", chain_tip.block.header.parent_microblock),
            "matured_miner_rewards": mature_rewards.clone(),
            "events": serialized_events,
            "transactions": serialized_txs,
        });

        // Send payload
        self.send_payload(&payload, PATH_BLOCK_PROCESSED);
    }
}

#[derive(Clone)]
pub struct EventDispatcher {
    registered_observers: Vec<EventObserver>,
    contract_events_observers_lookup: HashMap<(QualifiedContractIdentifier, String), HashSet<u16>>,
    assets_observers_lookup: HashMap<AssetIdentifier, HashSet<u16>>,
    burn_block_observers_lookup: HashSet<u16>,
    mempool_observers_lookup: HashSet<u16>,
    stx_observers_lookup: HashSet<u16>,
    any_event_observers_lookup: HashSet<u16>,
    boot_receipts: Arc<Mutex<Option<Vec<StacksTransactionReceipt>>>>,
}

impl BlockEventDispatcher for EventDispatcher {
    fn announce_block(
        &self,
        block: StacksBlock,
        metadata: StacksHeaderInfo,
        receipts: Vec<StacksTransactionReceipt>,
        parent: &StacksBlockId,
        winner_txid: Txid,
        mature_rewards: Vec<MinerReward>,
        mature_rewards_info: Option<MinerRewardInfo>,
    ) {
        let chain_tip = ChainTip {
            metadata,
            block,
            receipts,
        };
        self.process_chain_tip(
            &chain_tip,
            parent,
            winner_txid,
            mature_rewards,
            mature_rewards_info,
        )
    }

    fn announce_burn_block(
        &self,
        burn_block: &BurnchainHeaderHash,
        burn_block_height: u64,
        rewards: Vec<(StacksAddress, u64)>,
        burns: u64,
    ) {
        self.process_burn_block(burn_block, burn_block_height, rewards, burns)
    }

    fn dispatch_boot_receipts(&mut self, receipts: Vec<StacksTransactionReceipt>) {
        self.process_boot_receipts(receipts)
    }
}

impl EventDispatcher {
    pub fn new() -> EventDispatcher {
        EventDispatcher {
            registered_observers: vec![],
            contract_events_observers_lookup: HashMap::new(),
            assets_observers_lookup: HashMap::new(),
            stx_observers_lookup: HashSet::new(),
            any_event_observers_lookup: HashSet::new(),
            burn_block_observers_lookup: HashSet::new(),
            mempool_observers_lookup: HashSet::new(),
            boot_receipts: Arc::new(Mutex::new(None)),
        }
    }

    pub fn process_burn_block(
        &self,
        burn_block: &BurnchainHeaderHash,
        burn_block_height: u64,
        rewards: Vec<(StacksAddress, u64)>,
        burns: u64,
    ) {
        // lazily assemble payload only if we have observers
        let interested_observers: Vec<_> = self
            .registered_observers
            .iter()
            .enumerate()
            .filter(|(obs_id, _observer)| {
                self.burn_block_observers_lookup.contains(&(*obs_id as u16))
                    || self.any_event_observers_lookup.contains(&(*obs_id as u16))
            })
            .collect();
        if interested_observers.len() < 1 {
            return;
        }

        let payload = EventObserver::make_new_burn_block_payload(
            burn_block,
            burn_block_height,
            rewards,
            burns,
        );

        for (_, observer) in interested_observers.iter() {
            observer.send_new_burn_block(&payload);
        }
    }

    pub fn process_chain_tip(
        &self,
        chain_tip: &ChainTip,
        parent_index_hash: &StacksBlockId,
        winner_txid: Txid,
        mature_rewards: Vec<MinerReward>,
        mature_rewards_info: Option<MinerRewardInfo>,
    ) {
        let mut dispatch_matrix: Vec<HashSet<usize>> = self
            .registered_observers
            .iter()
            .map(|_| HashSet::new())
            .collect();
        let mut events: Vec<(bool, Txid, &StacksTransactionEvent)> = vec![];
        let mut i: usize = 0;

        let boot_receipts = if chain_tip.metadata.block_height == 1 {
            let mut boot_receipts_result = self
                .boot_receipts
                .lock()
                .expect("Unexpected concurrent access to `boot_receipts` in the event dispatcher!");
            if let Some(val) = boot_receipts_result.take() {
                val
            } else {
                vec![]
            }
        } else {
            vec![]
        };

        for receipt in chain_tip.receipts.iter().chain(boot_receipts.iter()) {
            let tx_hash = receipt.transaction.txid();
            for event in receipt.events.iter() {
                match event {
                    StacksTransactionEvent::SmartContractEvent(event_data) => {
                        if let Some(observer_indexes) =
                            self.contract_events_observers_lookup.get(&event_data.key)
                        {
                            for o_i in observer_indexes {
                                dispatch_matrix[*o_i as usize].insert(i);
                            }
                        }
                    }
                    StacksTransactionEvent::STXEvent(STXEventType::STXTransferEvent(_))
                    | StacksTransactionEvent::STXEvent(STXEventType::STXMintEvent(_))
                    | StacksTransactionEvent::STXEvent(STXEventType::STXBurnEvent(_))
                    | StacksTransactionEvent::STXEvent(STXEventType::STXLockEvent(_)) => {
                        for o_i in &self.stx_observers_lookup {
                            dispatch_matrix[*o_i as usize].insert(i);
                        }
                    }
                    StacksTransactionEvent::NFTEvent(NFTEventType::NFTTransferEvent(
                        event_data,
                    )) => {
                        self.update_dispatch_matrix_if_observer_subscribed(
                            &event_data.asset_identifier,
                            i,
                            &mut dispatch_matrix,
                        );
                    }
                    StacksTransactionEvent::NFTEvent(NFTEventType::NFTMintEvent(event_data)) => {
                        self.update_dispatch_matrix_if_observer_subscribed(
                            &event_data.asset_identifier,
                            i,
                            &mut dispatch_matrix,
                        );
                    }
                    StacksTransactionEvent::NFTEvent(NFTEventType::NFTBurnEvent(event_data)) => {
                        self.update_dispatch_matrix_if_observer_subscribed(
                            &event_data.asset_identifier,
                            i,
                            &mut dispatch_matrix,
                        );
                    }
                    StacksTransactionEvent::FTEvent(FTEventType::FTTransferEvent(event_data)) => {
                        self.update_dispatch_matrix_if_observer_subscribed(
                            &event_data.asset_identifier,
                            i,
                            &mut dispatch_matrix,
                        );
                    }
                    StacksTransactionEvent::FTEvent(FTEventType::FTMintEvent(event_data)) => {
                        self.update_dispatch_matrix_if_observer_subscribed(
                            &event_data.asset_identifier,
                            i,
                            &mut dispatch_matrix,
                        );
                    }
                    StacksTransactionEvent::FTEvent(FTEventType::FTBurnEvent(event_data)) => {
                        self.update_dispatch_matrix_if_observer_subscribed(
                            &event_data.asset_identifier,
                            i,
                            &mut dispatch_matrix,
                        );
                    }
                }
                events.push((!receipt.post_condition_aborted, tx_hash, event));
                for o_i in &self.any_event_observers_lookup {
                    dispatch_matrix[*o_i as usize].insert(i);
                }
                i += 1;
            }
        }

        if dispatch_matrix.len() > 0 {
            let mature_rewards_vec = if let Some(rewards_info) = mature_rewards_info {
                mature_rewards
                    .iter()
                    .map(|reward| {
                        json!({
                            "recipient": reward.address.to_string(),
                            "coinbase_amount": reward.coinbase.to_string(),
                            "tx_fees_anchored": reward.tx_fees_anchored.to_string(),
                            "tx_fees_streamed_confirmed": reward.tx_fees_streamed_confirmed.to_string(),
                            "from_stacks_block_hash": format!("0x{}", &rewards_info.from_stacks_block_hash),
                            "from_index_consensus_hash": format!("0x{}", StacksBlockId::new(&rewards_info.from_block_consensus_hash,
                                                                                            &rewards_info.from_stacks_block_hash)),
                        })
                    })
                    .collect()
            } else {
                vec![]
            };

            let mature_rewards = serde_json::Value::Array(mature_rewards_vec);

            for (observer_id, filtered_events_ids) in dispatch_matrix.iter().enumerate() {
                let filtered_events: Vec<_> = filtered_events_ids
                    .iter()
                    .map(|event_id| (*event_id, &events[*event_id]))
                    .collect();

                self.registered_observers[observer_id].send(
                    filtered_events,
                    chain_tip,
                    parent_index_hash,
                    &boot_receipts,
                    &winner_txid,
                    &mature_rewards,
                );
            }
        }
    }

    pub fn process_new_mempool_txs(&self, txs: Vec<StacksTransaction>) {
        // lazily assemble payload only if we have observers
        let interested_observers: Vec<_> = self
            .registered_observers
            .iter()
            .enumerate()
            .filter(|(obs_id, _observer)| {
                self.mempool_observers_lookup.contains(&(*obs_id as u16))
                    || self.any_event_observers_lookup.contains(&(*obs_id as u16))
            })
            .collect();
        if interested_observers.len() < 1 {
            return;
        }

        let payload = EventObserver::make_new_mempool_txs_payload(txs);

        for (_, observer) in interested_observers.iter() {
            observer.send_new_mempool_txs(&payload);
        }
    }

    pub fn process_new_attachments(&self, attachments: &Vec<AttachmentInstance>) {
        let interested_observers: Vec<_> = self.registered_observers.iter().enumerate().collect();
        if interested_observers.len() < 1 {
            return;
        }

        let mut serialized_attachments = vec![];
        for attachment in attachments.iter() {
            let payload = EventObserver::make_new_attachment_payload(attachment);
            serialized_attachments.push(payload);
        }

        for (_, observer) in interested_observers.iter() {
            observer.send_new_attachments(&json!(serialized_attachments));
        }
    }

    pub fn process_boot_receipts(&mut self, receipts: Vec<StacksTransactionReceipt>) {
        self.boot_receipts = Arc::new(Mutex::new(Some(receipts)));
    }

    fn update_dispatch_matrix_if_observer_subscribed(
        &self,
        asset_identifier: &AssetIdentifier,
        event_index: usize,
        dispatch_matrix: &mut Vec<HashSet<usize>>,
    ) {
        if let Some(observer_indexes) = self.assets_observers_lookup.get(asset_identifier) {
            for o_i in observer_indexes {
                dispatch_matrix[*o_i as usize].insert(event_index);
            }
        }
    }

    pub fn register_observer(&mut self, conf: &EventObserverConfig) {
        // let event_observer = EventObserver::new(&conf.address, conf.port);
        info!("Registering event observer at: {}", conf.endpoint);
        let event_observer = EventObserver {
            endpoint: conf.endpoint.clone(),
        };

        let observer_index = self.registered_observers.len() as u16;

        for event_key_type in conf.events_keys.iter() {
            match event_key_type {
                EventKeyType::SmartContractEvent(event_key) => {
                    match self
                        .contract_events_observers_lookup
                        .entry(event_key.clone())
                    {
                        Entry::Occupied(observer_indexes) => {
                            observer_indexes.into_mut().insert(observer_index);
                        }
                        Entry::Vacant(v) => {
                            let mut observer_indexes = HashSet::new();
                            observer_indexes.insert(observer_index);
                            v.insert(observer_indexes);
                        }
                    };
                }
                EventKeyType::BurnchainBlocks => {
                    self.burn_block_observers_lookup.insert(observer_index);
                }
                EventKeyType::MemPoolTransactions => {
                    self.mempool_observers_lookup.insert(observer_index);
                }
                EventKeyType::STXEvent => {
                    self.stx_observers_lookup.insert(observer_index);
                }
                EventKeyType::AssetEvent(event_key) => {
                    match self.assets_observers_lookup.entry(event_key.clone()) {
                        Entry::Occupied(observer_indexes) => {
                            observer_indexes.into_mut().insert(observer_index);
                        }
                        Entry::Vacant(v) => {
                            let mut observer_indexes = HashSet::new();
                            observer_indexes.insert(observer_index);
                            v.insert(observer_indexes);
                        }
                    };
                }
                EventKeyType::AnyEvent => {
                    self.any_event_observers_lookup.insert(observer_index);
                }
            }
        }

        self.registered_observers.push(event_observer);
    }
}
