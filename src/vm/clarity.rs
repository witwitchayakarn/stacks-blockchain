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

use vm::analysis;
use vm::analysis::AnalysisDatabase;
use vm::analysis::{errors::CheckError, errors::CheckErrors, ContractAnalysis};
use vm::ast;
use vm::ast::{errors::ParseError, errors::ParseErrors, ContractAST};
use vm::contexts::{AssetMap, Environment, OwnedEnvironment};
use vm::costs::{CostTracker, ExecutionCost, LimitedCostTracker};
use vm::database::{
    marf::WritableMarfStore, BurnStateDB, ClarityDatabase, HeadersDB, MarfedKV, RollbackWrapper,
    RollbackWrapperPersistedLog, SqliteConnection, NULL_BURN_STATE_DB, NULL_HEADER_DB,
};
use vm::errors::Error as InterpreterError;
use vm::representations::SymbolicExpression;
use vm::types::{
    AssetIdentifier, PrincipalData, QualifiedContractIdentifier, TypeSignature, Value,
};

use chainstate::burn::BlockHeaderHash;
use chainstate::stacks::events::StacksTransactionEvent;
use chainstate::stacks::index::marf::MARF;
use chainstate::stacks::index::{MarfTrieId, TrieHash};
use chainstate::stacks::Error as ChainstateError;
use chainstate::stacks::StacksBlockId;
use chainstate::stacks::StacksMicroblockHeader;

use chainstate::stacks::boot::{
    BOOT_CODE_COSTS, BOOT_CODE_COST_VOTING, BOOT_CODE_POX_TESTNET, STACKS_BOOT_COST_CONTRACT,
    STACKS_BOOT_COST_VOTE_CONTRACT, STACKS_BOOT_POX_CONTRACT,
};

use std::error;
use std::fmt;

use super::database::marf::ReadOnlyMarfStore;

///
/// A high-level interface for interacting with the Clarity VM.
///
/// ClarityInstance takes ownership of a MARF + Sqlite store used for
///   it's data operations.
/// The ClarityInstance defines a `begin_block(bhh, bhh, bhh) -> ClarityBlockConnection`
///    function.
/// ClarityBlockConnections are used for executing transactions within the context of
///    a single block.
/// Only one ClarityBlockConnection may be open at a time (enforced by the borrow checker)
///   and ClarityBlockConnections must be `commit_block`ed or `rollback_block`ed before discarding
///   begining the next connection (enforced by runtime panics).
///
pub struct ClarityInstance {
    datastore: MarfedKV,
    block_limit: ExecutionCost,
}

///
/// A high-level interface for Clarity VM interactions within a single block.
///
pub struct ClarityBlockConnection<'a> {
    datastore: WritableMarfStore<'a>,
    header_db: &'a dyn HeadersDB,
    burn_state_db: &'a dyn BurnStateDB,
    cost_track: Option<LimitedCostTracker>,
}

///
/// Interface for Clarity VM interactions within a given transaction.
///
///   commit the transaction to the block with .commit()
///   rollback the transaction by dropping this struct.
pub struct ClarityTransactionConnection<'a, 'b> {
    log: Option<RollbackWrapperPersistedLog>,
    store: &'a mut WritableMarfStore<'b>,
    header_db: &'a dyn HeadersDB,
    burn_state_db: &'a dyn BurnStateDB,
    cost_track: &'a mut Option<LimitedCostTracker>,
}

pub struct ClarityReadOnlyConnection<'a> {
    datastore: ReadOnlyMarfStore<'a>,
    header_db: &'a dyn HeadersDB,
    burn_state_db: &'a dyn BurnStateDB,
}

#[derive(Debug)]
pub enum Error {
    Analysis(CheckError),
    Parse(ParseError),
    Interpreter(InterpreterError),
    BadTransaction(String),
    CostError(ExecutionCost, ExecutionCost),
    AbortedByCallback(Option<Value>, AssetMap, Vec<StacksTransactionEvent>),
}

impl From<CheckError> for Error {
    fn from(e: CheckError) -> Self {
        match e.err {
            CheckErrors::CostOverflow => {
                Error::CostError(ExecutionCost::max_value(), ExecutionCost::max_value())
            }
            CheckErrors::CostBalanceExceeded(a, b) => Error::CostError(a, b),
            CheckErrors::MemoryBalanceExceeded(_a, _b) => {
                Error::CostError(ExecutionCost::max_value(), ExecutionCost::max_value())
            }
            _ => Error::Analysis(e),
        }
    }
}

impl From<InterpreterError> for Error {
    fn from(e: InterpreterError) -> Self {
        match &e {
            InterpreterError::Unchecked(CheckErrors::CostBalanceExceeded(a, b)) => {
                Error::CostError(a.clone(), b.clone())
            }
            InterpreterError::Unchecked(CheckErrors::CostOverflow) => {
                Error::CostError(ExecutionCost::max_value(), ExecutionCost::max_value())
            }
            _ => Error::Interpreter(e),
        }
    }
}

impl From<ParseError> for Error {
    fn from(e: ParseError) -> Self {
        match e.err {
            ParseErrors::CostOverflow => {
                Error::CostError(ExecutionCost::max_value(), ExecutionCost::max_value())
            }
            ParseErrors::CostBalanceExceeded(a, b) => Error::CostError(a, b),
            ParseErrors::MemoryBalanceExceeded(_a, _b) => {
                Error::CostError(ExecutionCost::max_value(), ExecutionCost::max_value())
            }
            _ => Error::Parse(e),
        }
    }
}

impl From<ChainstateError> for Error {
    fn from(e: ChainstateError) -> Self {
        match e {
            ChainstateError::InvalidStacksTransaction(msg, _) => Error::BadTransaction(msg),
            ChainstateError::CostOverflowError(_, after, budget) => Error::CostError(after, budget),
            ChainstateError::ClarityError(x) => x,
            x => Error::BadTransaction(format!("{:?}", &x)),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            Error::CostError(ref a, ref b) => {
                write!(f, "Cost Error: {} cost exceeded budget of {} cost", a, b)
            }
            Error::Analysis(ref e) => fmt::Display::fmt(e, f),
            Error::Parse(ref e) => fmt::Display::fmt(e, f),
            Error::AbortedByCallback(..) => write!(f, "Post condition aborted transaction"),
            Error::Interpreter(ref e) => fmt::Display::fmt(e, f),
            Error::BadTransaction(ref s) => fmt::Display::fmt(s, f),
        }
    }
}

impl error::Error for Error {
    fn cause(&self) -> Option<&dyn error::Error> {
        match *self {
            Error::CostError(ref _a, ref _b) => None,
            Error::AbortedByCallback(..) => None,
            Error::Analysis(ref e) => Some(e),
            Error::Parse(ref e) => Some(e),
            Error::Interpreter(ref e) => Some(e),
            Error::BadTransaction(ref _s) => None,
        }
    }
}

/// A macro for doing take/replace on a closure.
///   macro is needed rather than a function definition because
///   otherwise, we end up breaking the borrow checker when
///   passing a mutable reference across a function boundary.
macro_rules! using {
    ($to_use: expr, $msg: expr, $exec: expr) => {{
        let object = $to_use.take().expect(&format!(
            "BUG: Transaction connection lost {} handle.",
            $msg
        ));
        let (object, result) = ($exec)(object);
        $to_use.replace(object);
        result
    }};
}

impl ClarityBlockConnection<'_> {
    /// Reset the block's total execution to the given cost, if there is a cost tracker at all.
    /// Used by the miner to "undo" applying a transaction that exceeded the budget.
    pub fn reset_block_cost(&mut self, cost: ExecutionCost) -> () {
        if let Some(ref mut cost_tracker) = self.cost_track {
            cost_tracker.set_total(cost);
        }
    }

    pub fn set_cost_tracker(&mut self, tracker: LimitedCostTracker) -> LimitedCostTracker {
        let old = self
            .cost_track
            .take()
            .expect("BUG: Clarity block connection lost cost tracker instance");
        self.cost_track.replace(tracker);
        old
    }

    /// Get the current cost so far
    pub fn cost_so_far(&self) -> ExecutionCost {
        match self.cost_track {
            Some(ref track) => track.get_total(),
            None => ExecutionCost::zero(),
        }
    }
}

impl ClarityInstance {
    pub fn new(datastore: MarfedKV, block_limit: ExecutionCost) -> ClarityInstance {
        ClarityInstance {
            datastore,
            block_limit,
        }
    }

    pub fn with_marf<F, R>(&mut self, f: F) -> R
    where
        F: FnOnce(&mut MARF<StacksBlockId>) -> R,
    {
        f(self.datastore.get_marf())
    }

    pub fn begin_block<'a>(
        &'a mut self,
        current: &StacksBlockId,
        next: &StacksBlockId,
        header_db: &'a dyn HeadersDB,
        burn_state_db: &'a dyn BurnStateDB,
    ) -> ClarityBlockConnection<'a> {
        let mut datastore = self.datastore.begin(current, next);

        let cost_track = {
            let mut clarity_db = datastore.as_clarity_db(&NULL_HEADER_DB, &NULL_BURN_STATE_DB);
            Some(
                LimitedCostTracker::new(self.block_limit.clone(), &mut clarity_db)
                    .expect("FAIL: problem instantiating cost tracking"),
            )
        };

        ClarityBlockConnection {
            datastore,
            header_db,
            burn_state_db,
            cost_track,
        }
    }

    pub fn begin_genesis_block<'a>(
        &'a mut self,
        current: &StacksBlockId,
        next: &StacksBlockId,
        header_db: &'a dyn HeadersDB,
        burn_state_db: &'a dyn BurnStateDB,
    ) -> ClarityBlockConnection<'a> {
        let datastore = self.datastore.begin(current, next);

        let cost_track = Some(LimitedCostTracker::new_free());

        ClarityBlockConnection {
            datastore,
            header_db,
            burn_state_db,
            cost_track,
        }
    }

    /// begin a genesis block with the default cost contract
    ///  used in testing + benchmarking
    pub fn begin_test_genesis_block<'a>(
        &'a mut self,
        current: &StacksBlockId,
        next: &StacksBlockId,
        header_db: &'a dyn HeadersDB,
        burn_state_db: &'a dyn BurnStateDB,
    ) -> ClarityBlockConnection<'a> {
        let writable = self.datastore.begin(current, next);

        let cost_track = Some(LimitedCostTracker::new_free());

        let mut conn = ClarityBlockConnection {
            datastore: writable,
            header_db,
            burn_state_db,
            cost_track,
        };

        conn.as_transaction(|clarity_db| {
            let (ast, _) = clarity_db
                .analyze_smart_contract(&*STACKS_BOOT_COST_CONTRACT, BOOT_CODE_COSTS)
                .unwrap();
            clarity_db
                .initialize_smart_contract(
                    &*STACKS_BOOT_COST_CONTRACT,
                    &ast,
                    BOOT_CODE_COSTS,
                    |_, _| false,
                )
                .unwrap();
        });

        conn.as_transaction(|clarity_db| {
            let (ast, _) = clarity_db
                .analyze_smart_contract(&*STACKS_BOOT_COST_VOTE_CONTRACT, BOOT_CODE_COST_VOTING)
                .unwrap();
            clarity_db
                .initialize_smart_contract(
                    &*STACKS_BOOT_COST_VOTE_CONTRACT,
                    &ast,
                    BOOT_CODE_COST_VOTING,
                    |_, _| false,
                )
                .unwrap();
        });

        conn.as_transaction(|clarity_db| {
            let (ast, _) = clarity_db
                .analyze_smart_contract(&*STACKS_BOOT_POX_CONTRACT, &*BOOT_CODE_POX_TESTNET)
                .unwrap();
            clarity_db
                .initialize_smart_contract(
                    &*STACKS_BOOT_POX_CONTRACT,
                    &ast,
                    &*BOOT_CODE_POX_TESTNET,
                    |_, _| false,
                )
                .unwrap();
        });

        conn
    }

    pub fn begin_unconfirmed<'a>(
        &'a mut self,
        current: &StacksBlockId,
        header_db: &'a dyn HeadersDB,
        burn_state_db: &'a dyn BurnStateDB,
    ) -> ClarityBlockConnection<'a> {
        let mut datastore = self.datastore.begin_unconfirmed(current);

        let cost_track = {
            let mut clarity_db = datastore.as_clarity_db(&NULL_HEADER_DB, &NULL_BURN_STATE_DB);
            Some(
                LimitedCostTracker::new(self.block_limit.clone(), &mut clarity_db)
                    .expect("FAIL: problem instantiating cost tracking"),
            )
        };

        ClarityBlockConnection {
            datastore,
            header_db,
            burn_state_db,
            cost_track,
        }
    }

    pub fn read_only_connection<'a>(
        &'a mut self,
        at_block: &StacksBlockId,
        header_db: &'a dyn HeadersDB,
        burn_state_db: &'a dyn BurnStateDB,
    ) -> ClarityReadOnlyConnection<'a> {
        self.read_only_connection_checked(at_block, header_db, burn_state_db)
            .expect(&format!("BUG: failed to open block {}", at_block))
    }

    pub fn read_only_connection_checked<'a>(
        &'a mut self,
        at_block: &StacksBlockId,
        header_db: &'a dyn HeadersDB,
        burn_state_db: &'a dyn BurnStateDB,
    ) -> Result<ClarityReadOnlyConnection<'a>, Error> {
        let datastore = self.datastore.begin_read_only_checked(Some(at_block))?;

        Ok(ClarityReadOnlyConnection {
            datastore,
            header_db,
            burn_state_db,
        })
    }

    pub fn eval_read_only(
        &mut self,
        at_block: &StacksBlockId,
        header_db: &dyn HeadersDB,
        burn_state_db: &dyn BurnStateDB,
        contract: &QualifiedContractIdentifier,
        program: &str,
    ) -> Result<Value, Error> {
        let mut read_only_conn = self.datastore.begin_read_only(Some(at_block));
        let clarity_db = read_only_conn.as_clarity_db(header_db, burn_state_db);
        let mut env = OwnedEnvironment::new_free(clarity_db);
        env.eval_read_only(contract, program)
            .map(|(x, _, _)| x)
            .map_err(Error::from)
    }

    pub fn destroy(self) -> MarfedKV {
        self.datastore
    }
}

pub trait ClarityConnection {
    /// Do something to the underlying DB that involves only reading.
    fn with_clarity_db_readonly_owned<F, R>(&mut self, to_do: F) -> R
    where
        F: FnOnce(ClarityDatabase) -> (R, ClarityDatabase);
    fn with_analysis_db_readonly<F, R>(&mut self, to_do: F) -> R
    where
        F: FnOnce(&mut AnalysisDatabase) -> R;

    fn with_clarity_db_readonly<F, R>(&mut self, to_do: F) -> R
    where
        F: FnOnce(&mut ClarityDatabase) -> R,
    {
        self.with_clarity_db_readonly_owned(|mut db| (to_do(&mut db), db))
    }

    fn with_readonly_clarity_env<F, R>(
        &mut self,
        sender: PrincipalData,
        cost_track: LimitedCostTracker,
        to_do: F,
    ) -> Result<R, InterpreterError>
    where
        F: FnOnce(&mut Environment) -> Result<R, InterpreterError>,
    {
        self.with_clarity_db_readonly_owned(|clarity_db| {
            let mut vm_env = OwnedEnvironment::new_cost_limited(clarity_db, cost_track);
            let result = vm_env
                .execute_in_env(sender.into(), to_do)
                .map(|(result, _, _)| result);
            let (db, _) = vm_env
                .destruct()
                .expect("Failed to recover database reference after executing transaction");
            (result, db)
        })
    }
}

impl ClarityConnection for ClarityBlockConnection<'_> {
    /// Do something with ownership of the underlying DB that involves only reading.
    fn with_clarity_db_readonly_owned<F, R>(&mut self, to_do: F) -> R
    where
        F: FnOnce(ClarityDatabase) -> (R, ClarityDatabase),
    {
        let mut db =
            ClarityDatabase::new(&mut self.datastore, &self.header_db, &self.burn_state_db);
        db.begin();
        let (result, mut db) = to_do(db);
        db.roll_back();
        result
    }

    fn with_analysis_db_readonly<F, R>(&mut self, to_do: F) -> R
    where
        F: FnOnce(&mut AnalysisDatabase) -> R,
    {
        let mut db = AnalysisDatabase::new(&mut self.datastore);
        db.begin();
        let result = to_do(&mut db);
        db.roll_back();
        result
    }
}

impl ClarityConnection for ClarityReadOnlyConnection<'_> {
    /// Do something with ownership of the underlying DB that involves only reading.
    fn with_clarity_db_readonly_owned<F, R>(&mut self, to_do: F) -> R
    where
        F: FnOnce(ClarityDatabase) -> (R, ClarityDatabase),
    {
        let mut db = self
            .datastore
            .as_clarity_db(&self.header_db, &self.burn_state_db);
        db.begin();
        let (result, mut db) = to_do(db);
        db.roll_back();
        result
    }

    fn with_analysis_db_readonly<F, R>(&mut self, to_do: F) -> R
    where
        F: FnOnce(&mut AnalysisDatabase) -> R,
    {
        let mut db = self.datastore.as_analysis_db();
        db.begin();
        let result = to_do(&mut db);
        db.roll_back();
        result
    }
}

impl<'a> ClarityBlockConnection<'a> {
    /// Rolls back all changes in the current block by
    /// (1) dropping all writes from the current MARF tip,
    /// (2) rolling back side-storage
    pub fn rollback_block(self) {
        // this is a "lower-level" rollback than the roll backs performed in
        //   ClarityDatabase or AnalysisDatabase -- this is done at the backing store level.
        debug!("Rollback Clarity datastore");
        self.datastore.rollback_block();
    }

    /// Rolls back all unconfirmed state in the current block by
    /// (1) dropping all writes from the current MARF tip,
    /// (2) rolling back side-storage
    pub fn rollback_unconfirmed(self) {
        // this is a "lower-level" rollback than the roll backs performed in
        //   ClarityDatabase or AnalysisDatabase -- this is done at the backing store level.
        debug!("Rollback unconfirmed Clarity datastore");
        self.datastore.rollback_unconfirmed();
    }

    /// Commits all changes in the current block by
    /// (1) committing the current MARF tip to storage,
    /// (2) committing side-storage.
    #[cfg(test)]
    pub fn commit_block(self) -> LimitedCostTracker {
        debug!("Commit Clarity datastore");
        self.datastore.test_commit();

        self.cost_track.unwrap()
    }

    /// Commits all changes in the current block by
    /// (1) committing the current MARF tip to storage,
    /// (2) committing side-storage.  Commits to a different
    /// block hash than the one opened (i.e. since the caller
    /// may not have known the "real" block hash at the
    /// time of opening).
    pub fn commit_to_block(self, final_bhh: &StacksBlockId) -> LimitedCostTracker {
        debug!("Commit Clarity datastore to {}", final_bhh);
        self.datastore.commit_to(final_bhh);

        self.cost_track.unwrap()
    }

    /// Commits all changes in the current block by
    /// (1) committing the current MARF tip to storage,
    /// (2) committing side-storage.
    ///    before this saves, it updates the metadata headers in
    ///    the sidestore so that they don't get stepped on after
    ///    a miner re-executes a constructed block.
    pub fn commit_mined_block(self, bhh: &StacksBlockId) -> LimitedCostTracker {
        debug!("Commit mined Clarity datastore to {}", bhh);
        self.datastore.commit_mined_block(bhh);

        self.cost_track.unwrap()
    }

    /// Save all unconfirmed state by
    /// (1) committing the current unconfirmed MARF to storage,
    /// (2) committing side-storage
    /// Unconfirmed data has globally-unique block hashes that are cryptographically derived from a
    /// confirmed block hash, so they're exceedingly unlikely to conflict with existing blocks.
    pub fn commit_unconfirmed(self) -> LimitedCostTracker {
        debug!("Save unconfirmed Clarity datastore");
        self.datastore.commit_unconfirmed();

        self.cost_track.unwrap()
    }

    pub fn start_transaction_processing<'b>(&'b mut self) -> ClarityTransactionConnection<'b, 'a> {
        let store = &mut self.datastore;
        let cost_track = &mut self.cost_track;
        let header_db = &self.header_db;
        let burn_state_db = &self.burn_state_db;
        let mut log = RollbackWrapperPersistedLog::new();
        log.nest();
        ClarityTransactionConnection {
            store,
            cost_track,
            header_db,
            burn_state_db,
            log: Some(log),
        }
    }

    pub fn as_transaction<F, R>(&mut self, todo: F) -> R
    where
        F: FnOnce(&mut ClarityTransactionConnection) -> R,
    {
        let mut tx = self.start_transaction_processing();
        let r = todo(&mut tx);
        tx.commit();
        r
    }

    /// Get the MARF root hash
    pub fn get_root_hash(&mut self) -> TrieHash {
        self.datastore.get_root_hash()
    }
}

impl<'a, 'b> ClarityConnection for ClarityTransactionConnection<'a, 'b> {
    /// Do something with ownership of the underlying DB that involves only reading.
    fn with_clarity_db_readonly_owned<F, R>(&mut self, to_do: F) -> R
    where
        F: FnOnce(ClarityDatabase) -> (R, ClarityDatabase),
    {
        using!(self.log, "log", |log| {
            let rollback_wrapper = RollbackWrapper::from_persisted_log(self.store, log);
            let mut db = ClarityDatabase::new_with_rollback_wrapper(
                rollback_wrapper,
                &self.header_db,
                &self.burn_state_db,
            );
            db.begin();
            let (r, mut db) = to_do(db);
            db.roll_back();
            (db.destroy().into(), r)
        })
    }

    fn with_analysis_db_readonly<F, R>(&mut self, to_do: F) -> R
    where
        F: FnOnce(&mut AnalysisDatabase) -> R,
    {
        self.inner_with_analysis_db(|mut db| {
            db.begin();
            let result = to_do(&mut db);
            db.roll_back();
            result
        })
    }
}

impl<'a, 'b> Drop for ClarityTransactionConnection<'a, 'b> {
    fn drop(&mut self) {
        self.cost_track
            .as_mut()
            .expect("BUG: Transaction connection lost cost_tracker handle.")
            .reset_memory();
    }
}

impl<'a, 'b> ClarityTransactionConnection<'a, 'b> {
    fn inner_with_analysis_db<F, R>(&mut self, to_do: F) -> R
    where
        F: FnOnce(&mut AnalysisDatabase) -> R,
    {
        using!(self.log, "log", |log| {
            let rollback_wrapper = RollbackWrapper::from_persisted_log(self.store, log);
            let mut db = AnalysisDatabase::new_with_rollback_wrapper(rollback_wrapper);
            let r = to_do(&mut db);
            (db.destroy().into(), r)
        })
    }

    /// Do something to the underlying DB that involves writing.
    pub fn with_clarity_db<F, R>(&mut self, to_do: F) -> Result<R, Error>
    where
        F: FnOnce(&mut ClarityDatabase) -> Result<R, Error>,
    {
        using!(self.log, "log", |log| {
            let rollback_wrapper = RollbackWrapper::from_persisted_log(self.store, log);
            let mut db = ClarityDatabase::new_with_rollback_wrapper(
                rollback_wrapper,
                &self.header_db,
                &self.burn_state_db,
            );

            db.begin();
            let result = to_do(&mut db);
            if result.is_ok() {
                db.commit();
            } else {
                db.roll_back();
            }

            (db.destroy().into(), result)
        })
    }

    /// What's our total (block-wide) resource use so far?
    pub fn cost_so_far(&self) -> ExecutionCost {
        match self.cost_track {
            Some(ref track) => track.get_total(),
            None => ExecutionCost::zero(),
        }
    }

    /// Analyze a provided smart contract, but do not write the analysis to the AnalysisDatabase
    pub fn analyze_smart_contract(
        &mut self,
        identifier: &QualifiedContractIdentifier,
        contract_content: &str,
    ) -> Result<(ContractAST, ContractAnalysis), Error> {
        using!(self.cost_track, "cost tracker", |mut cost_track| {
            self.inner_with_analysis_db(|db| {
                let ast_result = ast::build_ast(identifier, contract_content, &mut cost_track);

                let mut contract_ast = match ast_result {
                    Ok(x) => x,
                    Err(e) => return (cost_track, Err(e.into())),
                };

                let result = analysis::run_analysis(
                    identifier,
                    &mut contract_ast.expressions,
                    db,
                    false,
                    cost_track,
                );

                match result {
                    Ok(mut contract_analysis) => {
                        let cost_track = contract_analysis.take_contract_cost_tracker();
                        (cost_track, Ok((contract_ast, contract_analysis)))
                    }
                    Err((e, cost_track)) => (cost_track, Err(e.into())),
                }
            })
        })
    }

    fn with_abort_callback<F, A, R>(
        &mut self,
        to_do: F,
        abort_call_back: A,
    ) -> Result<(R, AssetMap, Vec<StacksTransactionEvent>, bool), Error>
    where
        A: FnOnce(&AssetMap, &mut ClarityDatabase) -> bool,
        F: FnOnce(
            &mut OwnedEnvironment,
        ) -> Result<(R, AssetMap, Vec<StacksTransactionEvent>), Error>,
    {
        using!(self.log, "log", |log| {
            using!(self.cost_track, "cost tracker", |cost_track| {
                let rollback_wrapper = RollbackWrapper::from_persisted_log(self.store, log);
                let mut db = ClarityDatabase::new_with_rollback_wrapper(
                    rollback_wrapper,
                    &self.header_db,
                    &self.burn_state_db,
                );

                // wrap the whole contract-call in a claritydb transaction,
                //   so we can abort on call_back's boolean retun
                db.begin();
                let mut vm_env = OwnedEnvironment::new_cost_limited(db, cost_track);
                let result = to_do(&mut vm_env);
                let (mut db, cost_track) = vm_env
                    .destruct()
                    .expect("Failed to recover database reference after executing transaction");
                // DO NOT reset memory usage yet -- that should happen only when the TX commits.

                let result = match result {
                    Ok((value, asset_map, events)) => {
                        let aborted = abort_call_back(&asset_map, &mut db);
                        if aborted {
                            db.roll_back();
                        } else {
                            db.commit();
                        }
                        Ok((value, asset_map, events, aborted))
                    }
                    Err(e) => {
                        db.roll_back();
                        Err(e)
                    }
                };

                (cost_track, (db.destroy().into(), result))
            })
        })
    }

    /// Save a contract analysis output to the AnalysisDatabase
    /// An error here would indicate that something has gone terribly wrong in the processing of a contract insert.
    ///   the caller should likely abort the whole block or panic
    pub fn save_analysis(
        &mut self,
        identifier: &QualifiedContractIdentifier,
        contract_analysis: &ContractAnalysis,
    ) -> Result<(), CheckError> {
        self.inner_with_analysis_db(|db| {
            db.begin();
            let result = db.insert_contract(identifier, contract_analysis);
            match result {
                Ok(_) => {
                    db.commit();
                    Ok(())
                }
                Err(e) => {
                    db.roll_back();
                    Err(e)
                }
            }
        })
    }

    /// Execute a STX transfer in the current block.
    /// Will throw an error if it tries to spend STX that the 'from' principal doesn't have.
    pub fn run_stx_transfer(
        &mut self,
        from: &PrincipalData,
        to: &PrincipalData,
        amount: u128,
    ) -> Result<(Value, AssetMap, Vec<StacksTransactionEvent>), Error> {
        self.with_abort_callback(
            |vm_env| vm_env.stx_transfer(from, to, amount).map_err(Error::from),
            |_, _| false,
        )
        .and_then(|(value, assets, events, _)| Ok((value, assets, events)))
    }

    /// Execute a contract call in the current block.
    ///  If an error occurs while processing the transaction, it's modifications will be rolled back.
    /// abort_call_back is called with an AssetMap and a ClarityDatabase reference,
    ///   if abort_call_back returns true, all modifications from this transaction will be rolled back.
    ///      otherwise, they will be committed (though they may later be rolled back if the block itself is rolled back).
    pub fn run_contract_call<F>(
        &mut self,
        sender: &PrincipalData,
        contract: &QualifiedContractIdentifier,
        public_function: &str,
        args: &[Value],
        abort_call_back: F,
    ) -> Result<(Value, AssetMap, Vec<StacksTransactionEvent>), Error>
    where
        F: FnOnce(&AssetMap, &mut ClarityDatabase) -> bool,
    {
        let expr_args: Vec<_> = args
            .iter()
            .map(|x| SymbolicExpression::atom_value(x.clone()))
            .collect();

        self.with_abort_callback(
            |vm_env| {
                vm_env
                    .execute_transaction(
                        Value::Principal(sender.clone()),
                        contract.clone(),
                        public_function,
                        &expr_args,
                    )
                    .map_err(Error::from)
            },
            abort_call_back,
        )
        .and_then(|(value, assets, events, aborted)| {
            if aborted {
                Err(Error::AbortedByCallback(Some(value), assets, events))
            } else {
                Ok((value, assets, events))
            }
        })
    }

    /// Initialize a contract in the current block.
    ///  If an error occurs while processing the initialization, it's modifications will be rolled back.
    /// abort_call_back is called with an AssetMap and a ClarityDatabase reference,
    ///   if abort_call_back returns true, all modifications from this transaction will be rolled back.
    ///      otherwise, they will be committed (though they may later be rolled back if the block itself is rolled back).
    pub fn initialize_smart_contract<F>(
        &mut self,
        identifier: &QualifiedContractIdentifier,
        contract_ast: &ContractAST,
        contract_str: &str,
        abort_call_back: F,
    ) -> Result<(AssetMap, Vec<StacksTransactionEvent>), Error>
    where
        F: FnOnce(&AssetMap, &mut ClarityDatabase) -> bool,
    {
        let (_, asset_map, events, aborted) = self.with_abort_callback(
            |vm_env| {
                vm_env
                    .initialize_contract_from_ast(identifier.clone(), contract_ast, contract_str)
                    .map_err(Error::from)
            },
            abort_call_back,
        )?;
        if aborted {
            Err(Error::AbortedByCallback(None, asset_map, events))
        } else {
            Ok((asset_map, events))
        }
    }

    /// Evaluate a poison-microblock transaction
    pub fn run_poison_microblock(
        &mut self,
        sender: &PrincipalData,
        mblock_header_1: &StacksMicroblockHeader,
        mblock_header_2: &StacksMicroblockHeader,
    ) -> Result<Value, Error> {
        self.with_abort_callback(
            |vm_env| {
                vm_env
                    .handle_poison_microblock(sender, mblock_header_1, mblock_header_2)
                    .map_err(Error::from)
            },
            |_, _| false,
        )
        .and_then(|(value, ..)| Ok(value))
    }

    /// Commit the changes from the edit log.
    /// panics if there is more than one open savepoint
    pub fn commit(mut self) {
        let log = self
            .log
            .take()
            .expect("BUG: Transaction Connection lost db log connection.");
        let mut rollback_wrapper = RollbackWrapper::from_persisted_log(self.store, log);
        if rollback_wrapper.depth() != 1 {
            panic!(
                "Attempted to commit transaction with {} != 1 rollbacks",
                rollback_wrapper.depth()
            );
        }
        rollback_wrapper.commit();
        // now we can reset the memory usage for the edit-log
        self.cost_track
            .as_mut()
            .expect("BUG: Transaction connection lost cost tracker connection.")
            .reset_memory();
    }

    /// Evaluate a raw Clarity snippit
    #[cfg(test)]
    pub fn clarity_eval_raw(&mut self, code: &str) -> Result<Value, Error> {
        let (result, _, _, _) = self.with_abort_callback(
            |vm_env| vm_env.eval_raw(code).map_err(Error::from),
            |_, _| false,
        )?;
        Ok(result)
    }

    #[cfg(test)]
    pub fn eval_read_only(
        &mut self,
        contract: &QualifiedContractIdentifier,
        code: &str,
    ) -> Result<Value, Error> {
        let (result, _, _, _) = self.with_abort_callback(
            |vm_env| vm_env.eval_read_only(contract, code).map_err(Error::from),
            |_, _| false,
        )?;
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chainstate::stacks::index::storage::TrieFileStorage;
    use rusqlite::NO_PARAMS;
    use std::fs;
    use vm::analysis::errors::CheckErrors;
    use vm::database::{
        ClarityBackingStore, MarfedKV, STXBalance, NULL_BURN_STATE_DB, NULL_HEADER_DB,
    };
    use vm::types::{StandardPrincipalData, Value};

    #[test]
    pub fn bad_syntax_test() {
        let marf = MarfedKV::temporary();
        let mut clarity_instance = ClarityInstance::new(marf, ExecutionCost::max_value());

        let contract_identifier = QualifiedContractIdentifier::local("foo").unwrap();

        clarity_instance
            .begin_test_genesis_block(
                &StacksBlockId::sentinel(),
                &StacksBlockId([0 as u8; 32]),
                &NULL_HEADER_DB,
                &NULL_BURN_STATE_DB,
            )
            .commit_block();

        {
            let mut conn = clarity_instance.begin_block(
                &StacksBlockId([0 as u8; 32]),
                &StacksBlockId([1 as u8; 32]),
                &NULL_HEADER_DB,
                &NULL_BURN_STATE_DB,
            );

            let contract = "(define-public (foo (x int) (y uint)) (ok (+ x y)))";

            let _e = conn
                .as_transaction(|tx| tx.analyze_smart_contract(&contract_identifier, &contract))
                .unwrap_err();

            // okay, let's try it again:

            let _e = conn
                .as_transaction(|tx| tx.analyze_smart_contract(&contract_identifier, &contract))
                .unwrap_err();

            conn.commit_block();
        }
    }

    #[test]
    pub fn test_initialize_contract_tx_sender_contract_caller() {
        let marf = MarfedKV::temporary();
        let mut clarity_instance = ClarityInstance::new(marf, ExecutionCost::max_value());
        let contract_identifier = QualifiedContractIdentifier::local("foo").unwrap();

        clarity_instance
            .begin_test_genesis_block(
                &StacksBlockId::sentinel(),
                &StacksBlockId([0 as u8; 32]),
                &NULL_HEADER_DB,
                &NULL_BURN_STATE_DB,
            )
            .commit_block();

        {
            let mut conn = clarity_instance.begin_block(
                &StacksBlockId([0 as u8; 32]),
                &StacksBlockId([1 as u8; 32]),
                &NULL_HEADER_DB,
                &NULL_BURN_STATE_DB,
            );

            // S1G2081040G2081040G2081040G208105NK8PE5 is the transient address
            let contract = "
                (begin 
                    (asserts! (is-eq tx-sender 'S1G2081040G2081040G2081040G208105NK8PE5)
                        (err tx-sender))

                    (asserts! (is-eq contract-caller 'S1G2081040G2081040G2081040G208105NK8PE5)
                        (err contract-caller))
                )";

            conn.as_transaction(|conn| {
                let (ct_ast, ct_analysis) = conn
                    .analyze_smart_contract(&contract_identifier, &contract)
                    .unwrap();
                conn.initialize_smart_contract(&contract_identifier, &ct_ast, &contract, |_, _| {
                    false
                })
                .unwrap();
                conn.save_analysis(&contract_identifier, &ct_analysis)
                    .unwrap();
            });

            conn.commit_block();
        }
    }

    #[test]
    pub fn tx_rollback() {
        let marf = MarfedKV::temporary();
        let mut clarity_instance = ClarityInstance::new(marf, ExecutionCost::max_value());

        let contract_identifier = QualifiedContractIdentifier::local("foo").unwrap();
        let contract = "(define-public (foo (x int) (y int)) (ok (+ x y)))";

        clarity_instance
            .begin_test_genesis_block(
                &StacksBlockId::sentinel(),
                &StacksBlockId([0 as u8; 32]),
                &NULL_HEADER_DB,
                &NULL_BURN_STATE_DB,
            )
            .commit_block();

        {
            let mut conn = clarity_instance.begin_block(
                &StacksBlockId([0 as u8; 32]),
                &StacksBlockId([1 as u8; 32]),
                &NULL_HEADER_DB,
                &NULL_BURN_STATE_DB,
            );

            {
                let mut tx = conn.start_transaction_processing();

                let (ct_ast, ct_analysis) = tx
                    .analyze_smart_contract(&contract_identifier, &contract)
                    .unwrap();
                tx.initialize_smart_contract(&contract_identifier, &ct_ast, &contract, |_, _| {
                    false
                })
                .unwrap();
                tx.save_analysis(&contract_identifier, &ct_analysis)
                    .unwrap();
            }

            // okay, let's try it again -- should pass since the prior contract
            //   publish was unwound
            {
                let mut tx = conn.start_transaction_processing();

                let contract = "(define-public (foo (x int) (y int)) (ok (+ x y)))";

                let (ct_ast, ct_analysis) = tx
                    .analyze_smart_contract(&contract_identifier, &contract)
                    .unwrap();
                tx.initialize_smart_contract(&contract_identifier, &ct_ast, &contract, |_, _| {
                    false
                })
                .unwrap();
                tx.save_analysis(&contract_identifier, &ct_analysis)
                    .unwrap();

                tx.commit();
            }

            // should fail since the prior contract
            //   publish committed to the block
            {
                let mut tx = conn.start_transaction_processing();

                let contract = "(define-public (foo (x int) (y int)) (ok (+ x y)))";

                let (ct_ast, _ct_analysis) = tx
                    .analyze_smart_contract(&contract_identifier, &contract)
                    .unwrap();
                assert!(format!(
                    "{}",
                    tx.initialize_smart_contract(
                        &contract_identifier,
                        &ct_ast,
                        &contract,
                        |_, _| false
                    )
                    .unwrap_err()
                )
                .contains("ContractAlreadyExists"));

                tx.commit();
            }
        }
    }

    #[test]
    pub fn simple_test() {
        let marf = MarfedKV::temporary();
        let mut clarity_instance = ClarityInstance::new(marf, ExecutionCost::max_value());

        let contract_identifier = QualifiedContractIdentifier::local("foo").unwrap();

        clarity_instance
            .begin_test_genesis_block(
                &StacksBlockId::sentinel(),
                &StacksBlockId([0 as u8; 32]),
                &NULL_HEADER_DB,
                &NULL_BURN_STATE_DB,
            )
            .commit_block();

        {
            let mut conn = clarity_instance.begin_block(
                &StacksBlockId([0 as u8; 32]),
                &StacksBlockId([1 as u8; 32]),
                &NULL_HEADER_DB,
                &NULL_BURN_STATE_DB,
            );

            let contract = "(define-public (foo (x int)) (ok (+ x x)))";

            conn.as_transaction(|conn| {
                let (ct_ast, ct_analysis) = conn
                    .analyze_smart_contract(&contract_identifier, &contract)
                    .unwrap();
                conn.initialize_smart_contract(&contract_identifier, &ct_ast, &contract, |_, _| {
                    false
                })
                .unwrap();
                conn.save_analysis(&contract_identifier, &ct_analysis)
                    .unwrap();
            });

            assert_eq!(
                conn.as_transaction(|tx| tx.run_contract_call(
                    &StandardPrincipalData::transient().into(),
                    &contract_identifier,
                    "foo",
                    &[Value::Int(1)],
                    |_, _| false
                ))
                .unwrap()
                .0,
                Value::okay(Value::Int(2)).unwrap()
            );

            conn.commit_block();
        }

        let mut marf = clarity_instance.destroy();
        let mut conn = marf.begin_read_only(Some(&StacksBlockId([1 as u8; 32])));
        assert!(conn.get_contract_hash(&contract_identifier).is_ok());
    }

    #[test]
    pub fn test_block_roll_back() {
        let marf = MarfedKV::temporary();
        let mut clarity_instance = ClarityInstance::new(marf, ExecutionCost::max_value());
        let contract_identifier = QualifiedContractIdentifier::local("foo").unwrap();

        {
            let mut conn = clarity_instance.begin_test_genesis_block(
                &StacksBlockId::sentinel(),
                &StacksBlockId([0 as u8; 32]),
                &NULL_HEADER_DB,
                &NULL_BURN_STATE_DB,
            );

            let contract = "(define-public (foo (x int)) (ok (+ x x)))";

            conn.as_transaction(|conn| {
                let (ct_ast, ct_analysis) = conn
                    .analyze_smart_contract(&contract_identifier, &contract)
                    .unwrap();
                conn.initialize_smart_contract(&contract_identifier, &ct_ast, &contract, |_, _| {
                    false
                })
                .unwrap();
                conn.save_analysis(&contract_identifier, &ct_analysis)
                    .unwrap();
            });

            conn.rollback_block();
        }

        let mut marf = clarity_instance.destroy();

        let mut conn = marf.begin(&StacksBlockId::sentinel(), &StacksBlockId([0 as u8; 32]));
        // should not be in the marf.
        assert_eq!(
            conn.get_contract_hash(&contract_identifier).unwrap_err(),
            CheckErrors::NoSuchContract(contract_identifier.to_string()).into()
        );
        let sql = conn.get_side_store();
        // sqlite only have entries
        assert_eq!(
            0,
            sql.query_row::<u32, _, _>("SELECT COUNT(value) FROM data_table", NO_PARAMS, |row| row
                .get(0))
                .unwrap()
        );
    }

    #[test]
    fn test_unconfirmed() {
        let test_name = "/tmp/clarity_test_unconfirmed";
        if fs::metadata(test_name).is_ok() {
            fs::remove_dir_all(test_name).unwrap();
        }

        let confirmed_marf = MarfedKV::open(test_name, None).unwrap();
        let mut confirmed_clarity_instance =
            ClarityInstance::new(confirmed_marf, ExecutionCost::max_value());
        let contract_identifier = QualifiedContractIdentifier::local("foo").unwrap();

        let contract = "
        (define-data-var bar int 0)
        (define-public (get-bar) (ok (var-get bar)))
        (define-public (set-bar (x int) (y int))
          (begin (var-set bar (/ x y)) (ok (var-get bar))))";

        // make an empty but confirmed block
        confirmed_clarity_instance
            .begin_test_genesis_block(
                &StacksBlockId::sentinel(),
                &StacksBlockId([0 as u8; 32]),
                &NULL_HEADER_DB,
                &NULL_BURN_STATE_DB,
            )
            .commit_block();

        let marf = MarfedKV::open_unconfirmed(test_name, None).unwrap();

        let genesis_metadata_entries = marf
            .sql_conn()
            .query_row::<u32, _, _>(
                "SELECT COUNT(value) FROM metadata_table",
                NO_PARAMS,
                |row| row.get(0),
            )
            .unwrap();

        let mut clarity_instance = ClarityInstance::new(marf, ExecutionCost::max_value());

        // make an unconfirmed block off of the confirmed block
        {
            let mut conn = clarity_instance.begin_unconfirmed(
                &StacksBlockId([0 as u8; 32]),
                &NULL_HEADER_DB,
                &NULL_BURN_STATE_DB,
            );

            conn.as_transaction(|conn| {
                let (ct_ast, ct_analysis) = conn
                    .analyze_smart_contract(&contract_identifier, &contract)
                    .unwrap();
                conn.initialize_smart_contract(&contract_identifier, &ct_ast, &contract, |_, _| {
                    false
                })
                .unwrap();
                conn.save_analysis(&contract_identifier, &ct_analysis)
                    .unwrap();
            });

            conn.commit_unconfirmed();
        }

        // contract is still there, in unconfirmed status
        {
            let mut conn = clarity_instance.begin_unconfirmed(
                &StacksBlockId([0 as u8; 32]),
                &NULL_HEADER_DB,
                &NULL_BURN_STATE_DB,
            );

            conn.as_transaction(|conn| {
                conn.with_clarity_db_readonly(|ref mut tx| {
                    let src = tx.get_contract_src(&contract_identifier).unwrap();
                    assert_eq!(src, contract);
                });
            });

            conn.rollback_block();
        }

        // contract is still there, in unconfirmed status, even though the conn got explicitly
        // rolled back (but that should only drop the current TrieRAM)
        {
            let mut conn = clarity_instance.begin_unconfirmed(
                &StacksBlockId([0 as u8; 32]),
                &NULL_HEADER_DB,
                &NULL_BURN_STATE_DB,
            );

            conn.as_transaction(|conn| {
                conn.with_clarity_db_readonly(|ref mut tx| {
                    let src = tx.get_contract_src(&contract_identifier).unwrap();
                    assert_eq!(src, contract);
                });
            });

            conn.rollback_unconfirmed();
        }

        // contract is now absent, now that we did a rollback of unconfirmed state
        {
            let mut conn = clarity_instance.begin_unconfirmed(
                &StacksBlockId([0 as u8; 32]),
                &NULL_HEADER_DB,
                &NULL_BURN_STATE_DB,
            );

            conn.as_transaction(|conn| {
                conn.with_clarity_db_readonly(|ref mut tx| {
                    assert!(tx.get_contract_src(&contract_identifier).is_none());
                });
            });

            conn.commit_unconfirmed();
        }

        let mut marf = clarity_instance.destroy();
        let mut conn = marf.begin_unconfirmed(&StacksBlockId([0 as u8; 32]));

        // should not be in the marf.
        assert_eq!(
            conn.get_contract_hash(&contract_identifier).unwrap_err(),
            CheckErrors::NoSuchContract(contract_identifier.to_string()).into()
        );

        let sql = conn.get_side_store();
        // sqlite only have any metadata entries from the genesis block
        assert_eq!(
            genesis_metadata_entries,
            sql.query_row::<u32, _, _>(
                "SELECT COUNT(value) FROM metadata_table",
                NO_PARAMS,
                |row| row.get(0)
            )
            .unwrap()
        );
    }

    #[test]
    pub fn test_tx_roll_backs() {
        let marf = MarfedKV::temporary();
        let mut clarity_instance = ClarityInstance::new(marf, ExecutionCost::max_value());
        let contract_identifier = QualifiedContractIdentifier::local("foo").unwrap();
        let sender = StandardPrincipalData::transient().into();

        clarity_instance
            .begin_test_genesis_block(
                &StacksBlockId::sentinel(),
                &StacksBlockId([0 as u8; 32]),
                &NULL_HEADER_DB,
                &NULL_BURN_STATE_DB,
            )
            .commit_block();

        {
            let mut conn = clarity_instance.begin_block(
                &StacksBlockId([0 as u8; 32]),
                &StacksBlockId([1 as u8; 32]),
                &NULL_HEADER_DB,
                &NULL_BURN_STATE_DB,
            );

            let contract = "
            (define-data-var bar int 0)
            (define-public (get-bar) (ok (var-get bar)))
            (define-public (set-bar (x int) (y int))
              (begin (var-set bar (/ x y)) (ok (var-get bar))))";

            conn.as_transaction(|conn| {
                let (ct_ast, ct_analysis) = conn
                    .analyze_smart_contract(&contract_identifier, &contract)
                    .unwrap();
                conn.initialize_smart_contract(&contract_identifier, &ct_ast, &contract, |_, _| {
                    false
                })
                .unwrap();
                conn.save_analysis(&contract_identifier, &ct_analysis)
                    .unwrap();
            });

            assert_eq!(
                conn.as_transaction(|tx| tx.run_contract_call(
                    &sender,
                    &contract_identifier,
                    "get-bar",
                    &[],
                    |_, _| false
                ))
                .unwrap()
                .0,
                Value::okay(Value::Int(0)).unwrap()
            );

            assert_eq!(
                conn.as_transaction(|tx| tx.run_contract_call(
                    &sender,
                    &contract_identifier,
                    "set-bar",
                    &[Value::Int(1), Value::Int(1)],
                    |_, _| false
                ))
                .unwrap()
                .0,
                Value::okay(Value::Int(1)).unwrap()
            );

            let e = conn
                .as_transaction(|tx| {
                    tx.run_contract_call(
                        &sender,
                        &contract_identifier,
                        "set-bar",
                        &[Value::Int(10), Value::Int(1)],
                        |_, _| true,
                    )
                })
                .unwrap_err();
            let result_value = if let Error::AbortedByCallback(v, ..) = e {
                v.unwrap()
            } else {
                panic!("Expects a AbortedByCallback error")
            };

            assert_eq!(result_value, Value::okay(Value::Int(10)).unwrap());

            // prior transaction should have rolled back due to abort call back!
            assert_eq!(
                conn.as_transaction(|tx| tx.run_contract_call(
                    &sender,
                    &contract_identifier,
                    "get-bar",
                    &[],
                    |_, _| false
                ))
                .unwrap()
                .0,
                Value::okay(Value::Int(1)).unwrap()
            );

            assert!(format!(
                "{:?}",
                conn.as_transaction(|tx| tx.run_contract_call(
                    &sender,
                    &contract_identifier,
                    "set-bar",
                    &[Value::Int(10), Value::Int(0)],
                    |_, _| true
                ))
                .unwrap_err()
            )
            .contains("DivisionByZero"));

            // prior transaction should have rolled back due to runtime error
            assert_eq!(
                conn.as_transaction(|tx| tx.run_contract_call(
                    &StandardPrincipalData::transient().into(),
                    &contract_identifier,
                    "get-bar",
                    &[],
                    |_, _| false
                ))
                .unwrap()
                .0,
                Value::okay(Value::Int(1)).unwrap()
            );

            conn.commit_block();
        }
    }

    #[test]
    pub fn test_post_condition_failure_contract_publish() {
        use chainstate::stacks::db::*;
        use chainstate::stacks::*;
        use util::hash::Hash160;
        use util::secp256k1::MessageSignature;
        use util::strings::StacksString;

        let marf = MarfedKV::temporary();
        let mut clarity_instance = ClarityInstance::new(marf, ExecutionCost::max_value());
        let sender = StandardPrincipalData::transient().into();

        let spending_cond = TransactionSpendingCondition::Singlesig(SinglesigSpendingCondition {
            signer: Hash160([0x11u8; 20]),
            hash_mode: SinglesigHashMode::P2PKH,
            key_encoding: TransactionPublicKeyEncoding::Compressed,
            nonce: 0,
            tx_fee: 1,
            signature: MessageSignature::from_raw(&vec![0xfe; 65]),
        });

        let contract = "(define-public (foo) (ok 1))";

        let mut tx1 = StacksTransaction::new(
            TransactionVersion::Mainnet,
            TransactionAuth::Standard(spending_cond.clone()),
            TransactionPayload::SmartContract(TransactionSmartContract {
                name: "hello-world".into(),
                code_body: StacksString::from_str(contract).unwrap(),
            })
            .into(),
        );

        let tx2 = StacksTransaction::new(
            TransactionVersion::Mainnet,
            TransactionAuth::Standard(spending_cond.clone()),
            TransactionPayload::SmartContract(TransactionSmartContract {
                name: "hello-world".into(),
                code_body: StacksString::from_str(contract).unwrap(),
            })
            .into(),
        );

        tx1.post_conditions.push(TransactionPostCondition::STX(
            PostConditionPrincipal::Origin,
            FungibleConditionCode::SentEq,
            100,
        ));

        let mut tx3 = StacksTransaction::new(
            TransactionVersion::Mainnet,
            TransactionAuth::Standard(spending_cond.clone()),
            TransactionPayload::ContractCall(TransactionContractCall {
                address: sender,
                contract_name: "hello-world".into(),
                function_name: "foo".into(),
                function_args: vec![],
            }),
        );

        tx3.post_conditions.push(TransactionPostCondition::STX(
            PostConditionPrincipal::Origin,
            FungibleConditionCode::SentEq,
            100,
        ));
        let stx_balance = STXBalance::initial(5000);
        let account = StacksAccount {
            principal: sender.into(),
            nonce: 0,
            stx_balance,
        };

        clarity_instance
            .begin_test_genesis_block(
                &StacksBlockId::sentinel(),
                &StacksBlockId([0 as u8; 32]),
                &NULL_HEADER_DB,
                &NULL_BURN_STATE_DB,
            )
            .commit_block();

        {
            let mut conn = clarity_instance.begin_block(
                &StacksBlockId([0 as u8; 32]),
                &StacksBlockId([1 as u8; 32]),
                &NULL_HEADER_DB,
                &NULL_BURN_STATE_DB,
            );

            conn.as_transaction(|clarity_tx| {
                let receipt =
                    StacksChainState::process_transaction_payload(clarity_tx, &tx1, &account)
                        .unwrap();
                assert_eq!(receipt.post_condition_aborted, true);
            });
            conn.as_transaction(|clarity_tx| {
                StacksChainState::process_transaction_payload(clarity_tx, &tx2, &account).unwrap();
            });

            conn.as_transaction(|clarity_tx| {
                let receipt =
                    StacksChainState::process_transaction_payload(clarity_tx, &tx3, &account)
                        .unwrap();

                assert_eq!(receipt.post_condition_aborted, true);
            });

            conn.commit_block();
        }
    }

    #[test]
    pub fn test_block_limit() {
        let marf = MarfedKV::temporary();
        let mut clarity_instance = ClarityInstance::new(marf, ExecutionCost::max_value());
        let contract_identifier = QualifiedContractIdentifier::local("foo").unwrap();
        let sender = StandardPrincipalData::transient().into();

        clarity_instance
            .begin_test_genesis_block(
                &StacksBlockId::sentinel(),
                &StacksBlockId([0 as u8; 32]),
                &NULL_HEADER_DB,
                &NULL_BURN_STATE_DB,
            )
            .commit_block();

        {
            let mut conn = clarity_instance.begin_block(
                &StacksBlockId([0 as u8; 32]),
                &StacksBlockId([1 as u8; 32]),
                &NULL_HEADER_DB,
                &NULL_BURN_STATE_DB,
            );

            let contract = "
            (define-public (do-expand)
              (let ((list1 (list 1 2 3 4 5 6 7 8 9 10)))
                (let ((list2 (concat list1 list1)))
                  (let ((list3 (concat list2 list2)))
                    (let ((list4 (concat list3 list3)))
                      (ok (concat list4 list4)))))))
            ";

            conn.as_transaction(|conn| {
                let (ct_ast, ct_analysis) = conn
                    .analyze_smart_contract(&contract_identifier, &contract)
                    .unwrap();
                conn.initialize_smart_contract(&contract_identifier, &ct_ast, &contract, |_, _| {
                    false
                })
                .unwrap();
                conn.save_analysis(&contract_identifier, &ct_analysis)
                    .unwrap();
            });

            conn.commit_block();
        }

        clarity_instance.block_limit = ExecutionCost {
            write_length: u64::max_value(),
            write_count: u64::max_value(),
            read_count: u64::max_value(),
            read_length: u64::max_value(),
            runtime: 100,
        };

        {
            let mut conn = clarity_instance.begin_block(
                &StacksBlockId([1 as u8; 32]),
                &StacksBlockId([2 as u8; 32]),
                &NULL_HEADER_DB,
                &NULL_BURN_STATE_DB,
            );
            assert!(match conn
                .as_transaction(|tx| tx.run_contract_call(
                    &sender,
                    &contract_identifier,
                    "do-expand",
                    &[],
                    |_, _| false
                ))
                .unwrap_err()
            {
                Error::CostError(total, limit) => {
                    eprintln!("{}, {}", total, limit);
                    limit.runtime == 100 && total.runtime > 100
                }
                x => {
                    eprintln!("{}", x);
                    false
                }
            });

            conn.commit_block();
        }
    }
}
