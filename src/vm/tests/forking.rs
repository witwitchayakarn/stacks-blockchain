// Copyright (C) 2013-2020 Blockstack PBC, a public benefit corporation
// Copyright (C) 2020 Stacks Open Internet Foundation
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

use vm::analysis::errors::CheckErrors;
use vm::contexts::OwnedEnvironment;
use vm::database::{ClarityDatabase, MarfedKV, NULL_BURN_STATE_DB, NULL_HEADER_DB};
use vm::errors::{Error, InterpreterResult as Result, RuntimeErrorType};
use vm::representations::SymbolicExpression;
use vm::types::Value;
use vm::types::{PrincipalData, QualifiedContractIdentifier};

use vm::tests::{execute, is_committed, is_err_code, symbols_from_values};

use chainstate::burn::BlockHeaderHash;
use chainstate::stacks::index::storage::TrieFileStorage;
use chainstate::stacks::index::MarfTrieId;
use chainstate::stacks::StacksBlockId;

const p1_str: &str = "'SZ2J6ZY48GV1EZ5V2V5RB9MP66SW86PYKKQ9H6DPR";

#[test]
fn test_forking_simple() {
    with_separate_forks_environment(
        initialize_contract,
        |x| {
            branched_execution(x, true);
        },
        |x| {
            branched_execution(x, true);
        },
        |x| {
            branched_execution(x, false);
        },
    );
}

#[test]
fn test_at_block_mutations() {
    // test how at-block works when a mutation has occurred
    fn initialize(owned_env: &mut OwnedEnvironment) {
        let c = QualifiedContractIdentifier::local("contract").unwrap();
        let contract =
            "(define-data-var datum int 1)
             (define-public (working)
               (ok (at-block 0x0101010101010101010101010101010101010101010101010101010101010101 (var-get datum))))
             (define-public (broken)
               (begin
                 (var-set datum 10)
                 ;; this should return 1, not 10!
                 (ok (at-block 0x0101010101010101010101010101010101010101010101010101010101010101 (var-get datum)))))";

        eprintln!("Initializing contract...");
        owned_env.initialize_contract(c.clone(), &contract).unwrap();
    }

    fn branch(
        owned_env: &mut OwnedEnvironment,
        expected_value: i128,
        to_exec: &str,
    ) -> Result<Value> {
        let c = QualifiedContractIdentifier::local("contract").unwrap();
        let p1 = execute(p1_str);
        eprintln!("Branched execution...");

        {
            let mut env = owned_env.get_exec_environment(None);
            let command = format!("(var-get datum)");
            let value = env.eval_read_only(&c, &command).unwrap();
            assert_eq!(value, Value::Int(expected_value));
        }

        owned_env
            .execute_transaction(p1, c, to_exec, &vec![])
            .map(|(x, _, _)| x)
    }

    with_separate_forks_environment(
        initialize,
        |x| {
            assert_eq!(
                branch(x, 1, "working").unwrap(),
                Value::okay(Value::Int(1)).unwrap()
            );
            assert_eq!(
                branch(x, 1, "broken").unwrap(),
                Value::okay(Value::Int(1)).unwrap()
            );
            assert_eq!(
                branch(x, 10, "working").unwrap(),
                Value::okay(Value::Int(1)).unwrap()
            );
            // make this test fail: this assertion _should_ be
            //  true, but at-block is broken. when a context
            //  switches to an at-block context, _any_ of the db
            //  wrapping that the Clarity VM does needs to be
            //  ignored.
            assert_eq!(
                branch(x, 10, "broken").unwrap(),
                Value::okay(Value::Int(1)).unwrap()
            );
        },
        |_x| {},
        |_x| {},
    );
}

#[test]
fn test_at_block_good() {
    fn initialize(owned_env: &mut OwnedEnvironment) {
        let c = QualifiedContractIdentifier::local("contract").unwrap();
        let contract =
            "(define-data-var datum int 1)
             (define-public (reset)
               (begin
                 (var-set datum (+
                   (at-block 0x0202020202020202020202020202020202020202020202020202020202020202 (var-get datum))
                   (at-block 0x0101010101010101010101010101010101010101010101010101010101010101 (var-get datum))))
                 (ok (var-get datum))))
             (define-public (set-val)
               (begin
                 (var-set datum 10)
                 (ok (var-get datum))))";

        eprintln!("Initializing contract...");
        owned_env.initialize_contract(c.clone(), &contract).unwrap();
    }

    fn branch(
        owned_env: &mut OwnedEnvironment,
        expected_value: i128,
        to_exec: &str,
    ) -> Result<Value> {
        let c = QualifiedContractIdentifier::local("contract").unwrap();
        let p1 = execute(p1_str);
        eprintln!("Branched execution...");

        {
            let mut env = owned_env.get_exec_environment(None);
            let command = format!("(var-get datum)");
            let value = env.eval_read_only(&c, &command).unwrap();
            assert_eq!(value, Value::Int(expected_value));
        }

        owned_env
            .execute_transaction(p1, c, to_exec, &vec![])
            .map(|(x, _, _)| x)
    }

    with_separate_forks_environment(
        initialize,
        |x| {
            assert_eq!(
                branch(x, 1, "set-val").unwrap(),
                Value::okay(Value::Int(10)).unwrap()
            );
        },
        |x| {
            let resp = branch(x, 1, "reset").unwrap_err();
            eprintln!("{}", resp);
            match resp {
                Error::Runtime(x, _) => assert_eq!(
                    x,
                    RuntimeErrorType::UnknownBlockHeaderHash(BlockHeaderHash::from(
                        vec![2 as u8; 32].as_slice()
                    ))
                ),
                _ => panic!("Unexpected error"),
            }
        },
        |x| {
            assert_eq!(
                branch(x, 10, "reset").unwrap(),
                Value::okay(Value::Int(11)).unwrap()
            );
        },
    );
}

#[test]
fn test_at_block_missing_defines() {
    fn initialize_1(owned_env: &mut OwnedEnvironment) {
        let c_a = QualifiedContractIdentifier::local("contract-a").unwrap();

        let contract = "(define-map datum { id: bool } { value: int })

             (define-public (flip)
               (let ((current (default-to (get value (map-get?! datum {id: true})) 0)))
                 (map-set datum {id: true} (if (is-eq 1 current) 0 1))
                 (ok current)))";

        eprintln!("Initializing contract...");
        owned_env
            .initialize_contract(c_a.clone(), &contract)
            .unwrap();
    }

    fn initialize_2(owned_env: &mut OwnedEnvironment) -> Error {
        let c_b = QualifiedContractIdentifier::local("contract-b").unwrap();

        let contract = "(define-private (problematic-cc)
               (at-block 0x0101010101010101010101010101010101010101010101010101010101010101
                 (contract-call? .contract-a flip)))
             (problematic-cc)
            ";

        eprintln!("Initializing contract...");
        let e = owned_env
            .initialize_contract(c_b.clone(), &contract)
            .unwrap_err();
        e
    }

    with_separate_forks_environment(
        |_| {},
        initialize_1,
        |_| {},
        |env| {
            let err = initialize_2(env);
            assert_eq!(
                err,
                CheckErrors::NoSuchContract(
                    "S1G2081040G2081040G2081040G208105NK8PE5.contract-a".into()
                )
                .into()
            );
        },
    );
}

// execute:
// f -> a -> z
//    \--> b
// with f @ block 1;32
// with a @ block 2;32
// with b @ block 3;32
// with z @ block 4;32

fn with_separate_forks_environment<F0, F1, F2, F3>(f: F0, a: F1, b: F2, z: F3)
where
    F0: FnOnce(&mut OwnedEnvironment),
    F1: FnOnce(&mut OwnedEnvironment),
    F2: FnOnce(&mut OwnedEnvironment),
    F3: FnOnce(&mut OwnedEnvironment),
{
    let mut marf_kv = MarfedKV::temporary();

    {
        let mut store = marf_kv.begin(&StacksBlockId::sentinel(), &StacksBlockId([0 as u8; 32]));
        store
            .as_clarity_db(&NULL_HEADER_DB, &NULL_BURN_STATE_DB)
            .initialize();
        store.test_commit();
    }

    {
        let mut store = marf_kv.begin(&StacksBlockId([0 as u8; 32]), &StacksBlockId([1 as u8; 32]));
        let mut owned_env =
            OwnedEnvironment::new(store.as_clarity_db(&NULL_HEADER_DB, &NULL_BURN_STATE_DB));
        f(&mut owned_env);
        store.test_commit();
    }

    // Now, we can do our forking.

    {
        let mut store = marf_kv.begin(&StacksBlockId([1 as u8; 32]), &StacksBlockId([2 as u8; 32]));
        let mut owned_env =
            OwnedEnvironment::new(store.as_clarity_db(&NULL_HEADER_DB, &NULL_BURN_STATE_DB));
        a(&mut owned_env);
        store.test_commit();
    }

    {
        let mut store = marf_kv.begin(&StacksBlockId([1 as u8; 32]), &StacksBlockId([3 as u8; 32]));
        let mut owned_env =
            OwnedEnvironment::new(store.as_clarity_db(&NULL_HEADER_DB, &NULL_BURN_STATE_DB));
        b(&mut owned_env);
        store.test_commit();
    }

    {
        let mut store = marf_kv.begin(&StacksBlockId([2 as u8; 32]), &StacksBlockId([4 as u8; 32]));
        let mut owned_env =
            OwnedEnvironment::new(store.as_clarity_db(&NULL_HEADER_DB, &NULL_BURN_STATE_DB));
        z(&mut owned_env);
        store.test_commit();
    }
}

fn initialize_contract(owned_env: &mut OwnedEnvironment) {
    let p1_address = {
        if let Value::Principal(PrincipalData::Standard(address)) = execute(p1_str) {
            address
        } else {
            panic!();
        }
    };
    let contract = format!(
        "(define-constant burn-address 'SP000000000000000000002Q6VF78)
         (define-fungible-token stackaroos)
         (define-read-only (get-balance (p principal))
           (ft-get-balance stackaroos p))
         (define-public (destroy (x uint))
           (if (< (ft-get-balance stackaroos tx-sender) x)
               (err u30)
               (ft-transfer? stackaroos x tx-sender burn-address)))
         (ft-mint? stackaroos u10 {})",
        p1_str
    );

    eprintln!("Initializing contract...");

    let contract_identifier = QualifiedContractIdentifier::new(p1_address, "tokens".into());
    owned_env
        .initialize_contract(contract_identifier, &contract)
        .unwrap();
}

fn branched_execution(owned_env: &mut OwnedEnvironment, expect_success: bool) {
    let p1_address = {
        if let Value::Principal(PrincipalData::Standard(address)) = execute(p1_str) {
            address
        } else {
            panic!();
        }
    };
    let contract_identifier = QualifiedContractIdentifier::new(p1_address.clone(), "tokens".into());

    eprintln!("Branched execution...");

    {
        let mut env = owned_env.get_exec_environment(None);
        let command = format!("(get-balance {})", p1_str);
        let balance = env.eval_read_only(&contract_identifier, &command).unwrap();
        let expected = if expect_success { 10 } else { 0 };
        assert_eq!(balance, Value::UInt(expected));
    }

    let (result, _, _) = owned_env
        .execute_transaction(
            Value::Principal(PrincipalData::Standard(p1_address)),
            contract_identifier,
            "destroy",
            &symbols_from_values(vec![Value::UInt(10)]),
        )
        .unwrap();

    if expect_success {
        assert!(is_committed(&result))
    } else {
        assert!(is_err_code(&result, 30))
    }
}
