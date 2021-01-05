use stacks::address::AddressHashMode;
use stacks::chainstate::burn::BlockHeaderHash;
use stacks::net::{Error as NetError, StacksMessageCodec};
use stacks::util::{hash::*, secp256k1::*};
use stacks::vm::{
    representations::ContractName, types::PrincipalData, types::QualifiedContractIdentifier,
    types::StandardPrincipalData, Value,
};

use stacks::chainstate::stacks::{
    db::blocks::MemPoolRejection, Error as ChainstateError, StacksAddress, StacksBlockHeader,
    StacksMicroblockHeader, StacksPrivateKey, StacksPublicKey, StacksTransaction,
    StacksTransactionSigner, TokenTransferMemo, TransactionAuth, TransactionPayload,
    TransactionSpendingCondition, TransactionVersion, C32_ADDRESS_VERSION_MAINNET_SINGLESIG,
};

use stacks::core::mempool::MemPoolDB;
use std::convert::From;
use std::convert::TryFrom;
use std::sync::Mutex;

use crate::helium::RunLoop;
use crate::Keychain;

use crate::config::TESTNET_CHAIN_ID;

use super::{
    make_coinbase, make_contract_call, make_contract_publish, make_poison, make_stacks_transfer,
    to_addr, SK_1, SK_2,
};

const FOO_CONTRACT: &'static str = "(define-public (foo) (ok 1))
                                    (define-public (bar (x uint)) (ok x))";
const TRAIT_CONTRACT: &'static str = "(define-trait tr ((value () (response uint uint))))";
const USE_TRAIT_CONTRACT: &'static str = "(use-trait tr-trait .trait-contract.tr)
                                         (define-public (baz (abc <tr-trait>)) (ok (contract-of abc)))";
const IMPLEMENT_TRAIT_CONTRACT: &'static str = "(define-public (value) (ok u1))";
const BAD_TRAIT_CONTRACT: &'static str = "(define-public (foo-bar) (ok u1))";

pub fn make_bad_stacks_transfer(
    sender: &StacksPrivateKey,
    nonce: u64,
    tx_fee: u64,
    recipient: &PrincipalData,
    amount: u64,
) -> Vec<u8> {
    let payload =
        TransactionPayload::TokenTransfer(recipient.clone(), amount, TokenTransferMemo([0; 34]));

    let mut spending_condition =
        TransactionSpendingCondition::new_singlesig_p2pkh(StacksPublicKey::from_private(sender))
            .expect("Failed to create p2pkh spending condition from public key.");
    spending_condition.set_nonce(nonce);
    spending_condition.set_tx_fee(tx_fee);
    let auth = TransactionAuth::Standard(spending_condition);

    let mut unsigned_tx = StacksTransaction::new(TransactionVersion::Testnet, auth, payload);
    unsigned_tx.chain_id = TESTNET_CHAIN_ID;

    let mut tx_signer = StacksTransactionSigner::new(&unsigned_tx);

    tx_signer.sign_origin(&StacksPrivateKey::new()).unwrap();

    let mut buf = vec![];
    tx_signer
        .get_tx()
        .unwrap()
        .consensus_serialize(&mut buf)
        .unwrap();
    buf
}

lazy_static! {
    static ref CHAINSTATE_PATH: Mutex<Option<String>> = Mutex::new(None);
}

#[test]
fn mempool_setup_chainstate() {
    let mut conf = super::new_test_conf();

    // force seeds to be the same
    conf.node.seed = vec![0x00];

    conf.burnchain.commit_anchor_block_within = 1500;

    let contract_sk = StacksPrivateKey::from_hex(SK_1).unwrap();
    let contract_addr = to_addr(&contract_sk);
    conf.add_initial_balance(contract_addr.to_string(), 100000);

    {
        CHAINSTATE_PATH
            .lock()
            .unwrap()
            .replace(conf.get_chainstate_path());
    }

    let num_rounds = 4;

    let mut run_loop = RunLoop::new(conf.clone());

    run_loop
        .callbacks
        .on_new_tenure(|round, _burnchain_tip, chain_tip, tenure| {
            let mut chainstate_copy = tenure.open_chainstate();
            let contract_sk = StacksPrivateKey::from_hex(SK_1).unwrap();
            let header_hash = chain_tip.block.block_hash();
            let consensus_hash = chain_tip.metadata.consensus_hash;

            if round == 1 {
                eprintln!("Tenure in 1 started!");

                let publish_tx1 =
                    make_contract_publish(&contract_sk, 0, 100, "foo_contract", FOO_CONTRACT);
                tenure
                    .mem_pool
                    .submit_raw(
                        &mut chainstate_copy,
                        &consensus_hash,
                        &header_hash,
                        publish_tx1,
                    )
                    .unwrap();

                let publish_tx2 =
                    make_contract_publish(&contract_sk, 1, 100, "trait-contract", TRAIT_CONTRACT);
                tenure
                    .mem_pool
                    .submit_raw(
                        &mut chainstate_copy,
                        &consensus_hash,
                        &header_hash,
                        publish_tx2,
                    )
                    .unwrap();

                let publish_tx3 = make_contract_publish(
                    &contract_sk,
                    2,
                    100,
                    "use-trait-contract",
                    USE_TRAIT_CONTRACT,
                );
                tenure
                    .mem_pool
                    .submit_raw(
                        &mut chainstate_copy,
                        &consensus_hash,
                        &header_hash,
                        publish_tx3,
                    )
                    .unwrap();

                let publish_tx4 = make_contract_publish(
                    &contract_sk,
                    3,
                    100,
                    "implement-trait-contract",
                    IMPLEMENT_TRAIT_CONTRACT,
                );
                tenure
                    .mem_pool
                    .submit_raw(
                        &mut chainstate_copy,
                        &consensus_hash,
                        &header_hash,
                        publish_tx4,
                    )
                    .unwrap();

                let publish_tx4 = make_contract_publish(
                    &contract_sk,
                    4,
                    100,
                    "bad-trait-contract",
                    BAD_TRAIT_CONTRACT,
                );
                tenure
                    .mem_pool
                    .submit_raw(
                        &mut chainstate_copy,
                        &consensus_hash,
                        &header_hash,
                        publish_tx4,
                    )
                    .unwrap();
            }
        });

    run_loop.callbacks.on_new_stacks_chain_state(
        |round, _burnchain_tip, chain_tip, chain_state, _burn_dbconn| {
            let contract_sk = StacksPrivateKey::from_hex(SK_1).unwrap();
            let contract_addr = to_addr(&contract_sk);

            let other_sk = StacksPrivateKey::from_hex(SK_2).unwrap();
            let other_addr = to_addr(&other_sk).into();

            let chainstate_path = { CHAINSTATE_PATH.lock().unwrap().clone().unwrap() };

            let _mempool = MemPoolDB::open(false, TESTNET_CHAIN_ID, &chainstate_path).unwrap();

            if round == 3 {
                let block_header = chain_tip.metadata.clone();
                let consensus_hash = &block_header.consensus_hash;
                let block_hash = &block_header.anchored_header.block_hash();

                let micro_pubkh = &block_header.anchored_header.microblock_pubkey_hash;

                // let's throw some transactions at it.
                // first a couple valid ones:
                let tx_bytes =
                    make_contract_publish(&contract_sk, 5, 1000, "bar_contract", FOO_CONTRACT);
                let tx =
                    StacksTransaction::consensus_deserialize(&mut tx_bytes.as_slice()).unwrap();
                chain_state
                    .will_admit_mempool_tx(consensus_hash, block_hash, &tx, tx_bytes.len() as u64)
                    .unwrap();

                let tx_bytes = make_contract_call(
                    &contract_sk,
                    5,
                    200,
                    &contract_addr,
                    "foo_contract",
                    "bar",
                    &[Value::UInt(1)],
                );
                let tx =
                    StacksTransaction::consensus_deserialize(&mut tx_bytes.as_slice()).unwrap();
                chain_state
                    .will_admit_mempool_tx(consensus_hash, block_hash, &tx, tx_bytes.len() as u64)
                    .unwrap();

                let tx_bytes = make_stacks_transfer(&contract_sk, 5, 200, &other_addr, 1000);
                let tx =
                    StacksTransaction::consensus_deserialize(&mut tx_bytes.as_slice()).unwrap();
                chain_state
                    .will_admit_mempool_tx(consensus_hash, block_hash, &tx, tx_bytes.len() as u64)
                    .unwrap();

                // bad signature
                let tx_bytes = make_bad_stacks_transfer(&contract_sk, 5, 200, &other_addr, 1000);
                let tx =
                    StacksTransaction::consensus_deserialize(&mut tx_bytes.as_slice()).unwrap();
                let e = chain_state
                    .will_admit_mempool_tx(consensus_hash, block_hash, &tx, tx_bytes.len() as u64)
                    .unwrap_err();
                eprintln!("Err: {:?}", e);
                assert!(
                    if let MemPoolRejection::FailedToValidate(ChainstateError::NetError(
                        NetError::VerifyingError(_),
                    )) = e
                    {
                        true
                    } else {
                        false
                    }
                );

                // mismatched network on contract-call!
                let bad_addr = StacksAddress::from_public_keys(
                    88,
                    &AddressHashMode::SerializeP2PKH,
                    1,
                    &vec![StacksPublicKey::from_private(&other_sk)],
                )
                .unwrap()
                .into();

                let tx_bytes = make_contract_call(
                    &contract_sk,
                    5,
                    200,
                    &bad_addr,
                    "foo_contract",
                    "bar",
                    &[Value::UInt(1), Value::Int(2)],
                );
                let tx =
                    StacksTransaction::consensus_deserialize(&mut tx_bytes.as_slice()).unwrap();
                let e = chain_state
                    .will_admit_mempool_tx(consensus_hash, block_hash, &tx, tx_bytes.len() as u64)
                    .unwrap_err();

                assert!(if let MemPoolRejection::BadAddressVersionByte = e {
                    true
                } else {
                    false
                });

                // mismatched network on transfer!
                let bad_addr = StacksAddress::from_public_keys(
                    C32_ADDRESS_VERSION_MAINNET_SINGLESIG,
                    &AddressHashMode::SerializeP2PKH,
                    1,
                    &vec![StacksPublicKey::from_private(&other_sk)],
                )
                .unwrap()
                .into();

                let tx_bytes = make_stacks_transfer(&contract_sk, 5, 200, &bad_addr, 1000);
                let tx =
                    StacksTransaction::consensus_deserialize(&mut tx_bytes.as_slice()).unwrap();
                let e = chain_state
                    .will_admit_mempool_tx(consensus_hash, block_hash, &tx, tx_bytes.len() as u64)
                    .unwrap_err();
                assert!(if let MemPoolRejection::BadAddressVersionByte = e {
                    true
                } else {
                    false
                });

                // bad fees
                let tx_bytes = make_stacks_transfer(&contract_sk, 5, 0, &other_addr, 1000);
                let tx =
                    StacksTransaction::consensus_deserialize(&mut tx_bytes.as_slice()).unwrap();
                let e = chain_state
                    .will_admit_mempool_tx(consensus_hash, block_hash, &tx, tx_bytes.len() as u64)
                    .unwrap_err();
                eprintln!("Err: {:?}", e);
                assert!(if let MemPoolRejection::FeeTooLow(0, _) = e {
                    true
                } else {
                    false
                });

                // bad nonce
                let tx_bytes = make_stacks_transfer(&contract_sk, 0, 200, &other_addr, 1000);
                let tx =
                    StacksTransaction::consensus_deserialize(&mut tx_bytes.as_slice()).unwrap();
                let e = chain_state
                    .will_admit_mempool_tx(consensus_hash, block_hash, &tx, tx_bytes.len() as u64)
                    .unwrap_err();
                eprintln!("Err: {:?}", e);
                assert!(if let MemPoolRejection::BadNonces(_) = e {
                    true
                } else {
                    false
                });

                // not enough funds
                let tx_bytes = make_stacks_transfer(&contract_sk, 5, 110000, &other_addr, 1000);
                let tx =
                    StacksTransaction::consensus_deserialize(&mut tx_bytes.as_slice()).unwrap();
                let e = chain_state
                    .will_admit_mempool_tx(consensus_hash, block_hash, &tx, tx_bytes.len() as u64)
                    .unwrap_err();
                eprintln!("Err: {:?}", e);
                assert!(if let MemPoolRejection::NotEnoughFunds(111000, 99500) = e {
                    true
                } else {
                    false
                });

                let tx_bytes = make_stacks_transfer(&contract_sk, 5, 99700, &other_addr, 1000);
                let tx =
                    StacksTransaction::consensus_deserialize(&mut tx_bytes.as_slice()).unwrap();
                let e = chain_state
                    .will_admit_mempool_tx(consensus_hash, block_hash, &tx, tx_bytes.len() as u64)
                    .unwrap_err();
                eprintln!("Err: {:?}", e);
                assert!(if let MemPoolRejection::NotEnoughFunds(100700, 99500) = e {
                    true
                } else {
                    false
                });

                let tx_bytes = make_contract_call(
                    &contract_sk,
                    5,
                    200,
                    &contract_addr,
                    "bar_contract",
                    "bar",
                    &[Value::UInt(1)],
                );
                let tx =
                    StacksTransaction::consensus_deserialize(&mut tx_bytes.as_slice()).unwrap();
                let e = chain_state
                    .will_admit_mempool_tx(consensus_hash, block_hash, &tx, tx_bytes.len() as u64)
                    .unwrap_err();
                eprintln!("Err: {:?}", e);
                assert!(if let MemPoolRejection::NoSuchContract = e {
                    true
                } else {
                    false
                });

                let tx_bytes = make_contract_call(
                    &contract_sk,
                    5,
                    200,
                    &contract_addr,
                    "foo_contract",
                    "foobar",
                    &[Value::UInt(1)],
                );
                let tx =
                    StacksTransaction::consensus_deserialize(&mut tx_bytes.as_slice()).unwrap();
                let e = chain_state
                    .will_admit_mempool_tx(consensus_hash, block_hash, &tx, tx_bytes.len() as u64)
                    .unwrap_err();
                eprintln!("Err: {:?}", e);
                assert!(if let MemPoolRejection::NoSuchPublicFunction = e {
                    true
                } else {
                    false
                });

                let tx_bytes = make_contract_call(
                    &contract_sk,
                    5,
                    200,
                    &contract_addr,
                    "foo_contract",
                    "bar",
                    &[Value::UInt(1), Value::Int(2)],
                );
                let tx =
                    StacksTransaction::consensus_deserialize(&mut tx_bytes.as_slice()).unwrap();
                let e = chain_state
                    .will_admit_mempool_tx(consensus_hash, block_hash, &tx, tx_bytes.len() as u64)
                    .unwrap_err();
                eprintln!("Err: {:?}", e);
                assert!(if let MemPoolRejection::BadFunctionArgument(_) = e {
                    true
                } else {
                    false
                });

                let tx_bytes =
                    make_contract_publish(&contract_sk, 5, 1000, "foo_contract", FOO_CONTRACT);
                let tx =
                    StacksTransaction::consensus_deserialize(&mut tx_bytes.as_slice()).unwrap();
                let e = chain_state
                    .will_admit_mempool_tx(consensus_hash, block_hash, &tx, tx_bytes.len() as u64)
                    .unwrap_err();
                eprintln!("Err: {:?}", e);
                assert!(if let MemPoolRejection::ContractAlreadyExists(_) = e {
                    true
                } else {
                    false
                });

                let microblock_1 = StacksMicroblockHeader {
                    version: 0,
                    sequence: 0,
                    prev_block: BlockHeaderHash([0; 32]),
                    tx_merkle_root: Sha512Trunc256Sum::from_data(&[]),
                    signature: MessageSignature([1; 65]),
                };

                let microblock_2 = StacksMicroblockHeader {
                    version: 0,
                    sequence: 1,
                    prev_block: BlockHeaderHash([0; 32]),
                    tx_merkle_root: Sha512Trunc256Sum::from_data(&[]),
                    signature: MessageSignature([1; 65]),
                };

                let tx_bytes = make_poison(&contract_sk, 5, 1000, microblock_1, microblock_2);
                let tx =
                    StacksTransaction::consensus_deserialize(&mut tx_bytes.as_slice()).unwrap();
                let e = chain_state
                    .will_admit_mempool_tx(consensus_hash, block_hash, &tx, tx_bytes.len() as u64)
                    .unwrap_err();
                eprintln!("Err: {:?}", e);
                assert!(
                    if let MemPoolRejection::PoisonMicroblocksDoNotConflict = e {
                        true
                    } else {
                        false
                    }
                );

                let microblock_1 = StacksMicroblockHeader {
                    version: 0,
                    sequence: 0,
                    prev_block: block_hash.clone(),
                    tx_merkle_root: Sha512Trunc256Sum::from_data(&[]),
                    signature: MessageSignature([0; 65]),
                };

                let microblock_2 = StacksMicroblockHeader {
                    version: 0,
                    sequence: 0,
                    prev_block: block_hash.clone(),
                    tx_merkle_root: Sha512Trunc256Sum::from_data(&[1, 2, 3]),
                    signature: MessageSignature([0; 65]),
                };

                let tx_bytes = make_poison(&contract_sk, 5, 1000, microblock_1, microblock_2);
                let tx =
                    StacksTransaction::consensus_deserialize(&mut tx_bytes.as_slice()).unwrap();
                let e = chain_state
                    .will_admit_mempool_tx(consensus_hash, block_hash, &tx, tx_bytes.len() as u64)
                    .unwrap_err();
                eprintln!("Err: {:?}", e);
                assert!(if let MemPoolRejection::InvalidMicroblocks = e {
                    true
                } else {
                    false
                });

                let mut microblock_1 = StacksMicroblockHeader {
                    version: 0,
                    sequence: 0,
                    prev_block: BlockHeaderHash([0; 32]),
                    tx_merkle_root: Sha512Trunc256Sum::from_data(&[]),
                    signature: MessageSignature([0; 65]),
                };

                let mut microblock_2 = StacksMicroblockHeader {
                    version: 0,
                    sequence: 0,
                    prev_block: BlockHeaderHash([0; 32]),
                    tx_merkle_root: Sha512Trunc256Sum::from_data(&[1, 2, 3]),
                    signature: MessageSignature([0; 65]),
                };

                microblock_1.sign(&other_sk).unwrap();
                microblock_2.sign(&other_sk).unwrap();

                let tx_bytes = make_poison(&contract_sk, 5, 1000, microblock_1, microblock_2);
                let tx =
                    StacksTransaction::consensus_deserialize(&mut tx_bytes.as_slice()).unwrap();
                let e = chain_state
                    .will_admit_mempool_tx(consensus_hash, block_hash, &tx, tx_bytes.len() as u64)
                    .unwrap_err();
                eprintln!("Err: {:?}", e);
                assert!(
                    if let MemPoolRejection::NoAnchorBlockWithPubkeyHash(_) = e {
                        true
                    } else {
                        false
                    }
                );

                let tx_bytes = make_coinbase(&contract_sk, 5, 1000);
                let tx =
                    StacksTransaction::consensus_deserialize(&mut tx_bytes.as_slice()).unwrap();
                let e = chain_state
                    .will_admit_mempool_tx(consensus_hash, block_hash, &tx, tx_bytes.len() as u64)
                    .unwrap_err();
                eprintln!("Err: {:?}", e);
                assert!(if let MemPoolRejection::NoCoinbaseViaMempool = e {
                    true
                } else {
                    false
                });

                // find the correct priv-key
                let mut secret_key = None;
                let mut conf = super::new_test_conf();
                conf.node.seed = vec![0x00];

                let mut keychain = Keychain::default(conf.node.seed.clone());
                for i in 0..4 {
                    let microblock_secret_key = keychain.rotate_microblock_keypair(1 + i);
                    let mut microblock_pubkey =
                        Secp256k1PublicKey::from_private(&microblock_secret_key);
                    microblock_pubkey.set_compressed(true);
                    let pubkey_hash = StacksBlockHeader::pubkey_hash(&microblock_pubkey);
                    if pubkey_hash == *micro_pubkh {
                        secret_key = Some(microblock_secret_key);
                        break;
                    }
                }

                let secret_key = secret_key.expect("Failed to find the microblock secret key");

                let mut microblock_1 = StacksMicroblockHeader {
                    version: 0,
                    sequence: 0,
                    prev_block: BlockHeaderHash([0; 32]),
                    tx_merkle_root: Sha512Trunc256Sum::from_data(&[]),
                    signature: MessageSignature([0; 65]),
                };

                let mut microblock_2 = StacksMicroblockHeader {
                    version: 0,
                    sequence: 0,
                    prev_block: BlockHeaderHash([0; 32]),
                    tx_merkle_root: Sha512Trunc256Sum::from_data(&[1, 2, 3]),
                    signature: MessageSignature([0; 65]),
                };

                microblock_1.sign(&secret_key).unwrap();
                microblock_2.sign(&secret_key).unwrap();

                let tx_bytes = make_poison(&contract_sk, 5, 1000, microblock_1, microblock_2);
                let tx =
                    StacksTransaction::consensus_deserialize(&mut tx_bytes.as_slice()).unwrap();
                chain_state
                    .will_admit_mempool_tx(consensus_hash, block_hash, &tx, tx_bytes.len() as u64)
                    .unwrap();

                let contract_id = QualifiedContractIdentifier::new(
                    StandardPrincipalData::from(contract_addr.clone()),
                    ContractName::try_from("implement-trait-contract").unwrap(),
                );
                let contract_principal = PrincipalData::Contract(contract_id.clone());

                let tx_bytes = make_contract_call(
                    &contract_sk,
                    5,
                    250,
                    &contract_addr,
                    "use-trait-contract",
                    "baz",
                    &[Value::Principal(contract_principal)],
                );
                let tx =
                    StacksTransaction::consensus_deserialize(&mut tx_bytes.as_slice()).unwrap();
                chain_state
                    .will_admit_mempool_tx(consensus_hash, block_hash, &tx, tx_bytes.len() as u64)
                    .unwrap();

                let contract_id = QualifiedContractIdentifier::new(
                    StandardPrincipalData::from(contract_addr.clone()),
                    ContractName::try_from("bad-trait-contract").unwrap(),
                );
                let contract_principal = PrincipalData::Contract(contract_id.clone());

                let tx_bytes = make_contract_call(
                    &contract_sk,
                    5,
                    250,
                    &contract_addr,
                    "use-trait-contract",
                    "baz",
                    &[Value::Principal(contract_principal)],
                );
                let tx =
                    StacksTransaction::consensus_deserialize(&mut tx_bytes.as_slice()).unwrap();
                let e = chain_state
                    .will_admit_mempool_tx(consensus_hash, block_hash, &tx, tx_bytes.len() as u64)
                    .unwrap_err();
                assert!(if let MemPoolRejection::BadFunctionArgument(_) = e {
                    true
                } else {
                    false
                });
            }
        },
    );

    run_loop.start(num_rounds).unwrap();
}
