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

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs;
use std::io;
use std::io::prelude::*;

use chainstate::stacks::db::*;
use chainstate::stacks::Error;
use chainstate::stacks::*;

use std::path::{Path, PathBuf};

use util::db::Error as db_error;
use util::db::{query_count, query_rows, DBConn};

use util::strings::StacksString;

use util::hash::to_hex;

use chainstate::burn::db::sortdb::*;

use net::Error as net_error;

use vm::types::{PrincipalData, QualifiedContractIdentifier, StandardPrincipalData};

use vm::contexts::{AssetMap, OwnedEnvironment};

use vm::analysis::run_analysis;
use vm::ast::build_ast;
use vm::types::{AssetIdentifier, Value};

use vm::clarity::ClarityConnection;

pub use vm::analysis::errors::CheckErrors;
use vm::errors::Error as clarity_vm_error;

use vm::database::ClarityDatabase;

use vm::contracts::Contract;

impl StacksChainState {
    pub fn get_contract<T: ClarityConnection>(
        clarity_tx: &mut T,
        contract_id: &QualifiedContractIdentifier,
    ) -> Result<Option<Contract>, Error> {
        clarity_tx
            .with_clarity_db_readonly(|ref mut db| match db.get_contract(contract_id) {
                Ok(c) => Ok(Some(c)),
                Err(clarity_vm_error::Unchecked(CheckErrors::NoSuchContract(_))) => Ok(None),
                Err(e) => Err(clarity_error::Interpreter(e)),
            })
            .map_err(Error::ClarityError)
    }

    pub fn get_data_var<T: ClarityConnection>(
        clarity_tx: &mut T,
        contract_id: &QualifiedContractIdentifier,
        data_var: &str,
    ) -> Result<Option<Value>, Error> {
        clarity_tx
            .with_clarity_db_readonly(|ref mut db| {
                match db.lookup_variable_unknown_descriptor(contract_id, data_var) {
                    Ok(c) => Ok(Some(c)),
                    Err(clarity_vm_error::Unchecked(CheckErrors::NoSuchDataVariable(_))) => {
                        Ok(None)
                    }
                    Err(e) => Err(clarity_error::Interpreter(e)),
                }
            })
            .map_err(Error::ClarityError)
    }
}
