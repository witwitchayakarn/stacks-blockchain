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

use std::fmt;
use std::io;
use std::io::prelude::*;
use std::io::{Read, Write};

use std::borrow::Borrow;
use std::convert::TryFrom;
use std::ops::Deref;
use std::ops::DerefMut;

use net::codec::{read_next, read_next_at_most, write_next};
use net::Error as net_error;
use net::StacksMessageCodec;
use net::MAX_MESSAGE_LEN;

use regex::Regex;

use vm::ast::parser::{lex, LexItem, CONTRACT_MAX_NAME_LENGTH, CONTRACT_MIN_NAME_LENGTH};
use vm::representations::{
    ClarityName, ContractName, SymbolicExpression, MAX_STRING_LEN as CLARITY_MAX_STRING_LENGTH,
};

pub use vm::representations::UrlString;

use url;

use vm::types::{PrincipalData, QualifiedContractIdentifier, StandardPrincipalData, Value};

use util::retry::BoundReader;

/// printable-ASCII-only string, but encodable.
/// Note that it cannot be longer than ARRAY_MAX_LEN (4.1 billion bytes)
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct StacksString(Vec<u8>);

pub struct VecDisplay<'a, T: fmt::Display>(pub &'a [T]);

impl<'a, T: fmt::Display> fmt::Display for VecDisplay<'a, T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "[")?;
        for (ix, val) in self.0.iter().enumerate() {
            if ix == 0 {
                write!(f, "{}", val)?;
            } else {
                write!(f, ", {}", val)?;
            }
        }
        write!(f, "]")
    }
}

impl fmt::Display for StacksString {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(String::from_utf8_lossy(&self).into_owned().as_str())
    }
}

impl fmt::Debug for StacksString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(String::from_utf8_lossy(&self).into_owned().as_str())
    }
}

impl Deref for StacksString {
    type Target = Vec<u8>;
    fn deref(&self) -> &Vec<u8> {
        &self.0
    }
}

impl DerefMut for StacksString {
    fn deref_mut(&mut self) -> &mut Vec<u8> {
        &mut self.0
    }
}

impl StacksMessageCodec for StacksString {
    fn consensus_serialize<W: Write>(&self, fd: &mut W) -> Result<(), net_error> {
        write_next(fd, &self.0)
    }

    fn consensus_deserialize<R: Read>(fd: &mut R) -> Result<StacksString, net_error> {
        let bytes: Vec<u8> = {
            let mut bound_read = BoundReader::from_reader(fd, MAX_MESSAGE_LEN as u64);
            read_next(&mut bound_read)
        }?;

        // must encode a valid string
        let s = String::from_utf8(bytes.clone()).map_err(|_e| {
            warn!("Invalid StacksString -- could not build from utf8");
            net_error::DeserializeError(
                "Invalid Stacks string: could not build from utf8".to_string(),
            )
        })?;

        if !StacksString::is_valid_string(&s) {
            // non-printable ASCII or not ASCII
            warn!("Invalid StacksString -- non-printable ASCII or non-ASCII");
            return Err(net_error::DeserializeError(
                "Invalid Stacks string: non-printable or non-ASCII string".to_string(),
            ));
        }

        Ok(StacksString(bytes))
    }
}

impl StacksMessageCodec for ClarityName {
    fn consensus_serialize<W: Write>(&self, fd: &mut W) -> Result<(), net_error> {
        // ClarityName can't be longer than vm::representations::MAX_STRING_LEN, which itself is
        // a u8, so we should be good here.
        if self.as_bytes().len() > CLARITY_MAX_STRING_LENGTH as usize {
            return Err(net_error::SerializeError(
                "Failed to serialize clarity name: too long".to_string(),
            ));
        }
        write_next(fd, &(self.as_bytes().len() as u8))?;
        fd.write_all(self.as_bytes())
            .map_err(net_error::WriteError)?;
        Ok(())
    }

    fn consensus_deserialize<R: Read>(fd: &mut R) -> Result<ClarityName, net_error> {
        let len_byte: u8 = read_next(fd)?;
        if len_byte > CLARITY_MAX_STRING_LENGTH {
            return Err(net_error::DeserializeError(
                "Failed to deserialize clarity name: too long".to_string(),
            ));
        }
        let mut bytes = vec![0u8; len_byte as usize];
        fd.read_exact(&mut bytes).map_err(net_error::ReadError)?;

        // must encode a valid string
        let s = String::from_utf8(bytes).map_err(|_e| {
            net_error::DeserializeError(
                "Failed to parse Clarity name: could not contruct from utf8".to_string(),
            )
        })?;

        // must decode to a clarity name
        let name = ClarityName::try_from(s).map_err(|e| {
            net_error::DeserializeError(format!("Failed to parse Clarity name: {:?}", e))
        })?;
        Ok(name)
    }
}

impl StacksMessageCodec for ContractName {
    fn consensus_serialize<W: Write>(&self, fd: &mut W) -> Result<(), net_error> {
        if self.as_bytes().len() < CONTRACT_MIN_NAME_LENGTH as usize
            || self.as_bytes().len() > CONTRACT_MAX_NAME_LENGTH as usize
        {
            return Err(net_error::SerializeError(format!(
                "Failed to serialize contract name: too short or too long: {}",
                self.as_bytes().len()
            )));
        }
        write_next(fd, &(self.as_bytes().len() as u8))?;
        fd.write_all(self.as_bytes())
            .map_err(net_error::WriteError)?;
        Ok(())
    }

    fn consensus_deserialize<R: Read>(fd: &mut R) -> Result<ContractName, net_error> {
        let len_byte: u8 = read_next(fd)?;
        if (len_byte as usize) < CONTRACT_MIN_NAME_LENGTH
            || (len_byte as usize) > CONTRACT_MAX_NAME_LENGTH
        {
            return Err(net_error::DeserializeError(format!(
                "Failed to deserialize contract name: too short or too long: {}",
                len_byte
            )));
        }
        let mut bytes = vec![0u8; len_byte as usize];
        fd.read_exact(&mut bytes).map_err(net_error::ReadError)?;

        // must encode a valid string
        let s = String::from_utf8(bytes).map_err(|_e| {
            net_error::DeserializeError(
                "Failed to parse Contract name: could not construct from utf8".to_string(),
            )
        })?;

        let name = ContractName::try_from(s).map_err(|e| {
            net_error::DeserializeError(format!("Failed to parse Contract name: {:?}", e))
        })?;
        Ok(name)
    }
}

impl StacksMessageCodec for UrlString {
    fn consensus_serialize<W: Write>(&self, fd: &mut W) -> Result<(), net_error> {
        // UrlString can't be longer than vm::representations::MAX_STRING_LEN, which itself is
        // a u8, so we should be good here.
        if self.as_bytes().len() > CLARITY_MAX_STRING_LENGTH as usize {
            return Err(net_error::SerializeError(
                "Failed to serialize URL string: too long".to_string(),
            ));
        }

        // must be a valid block URL, or empty string
        if self.as_bytes().len() > 0 {
            let _ = self.parse_to_block_url()?;
        }

        write_next(fd, &(self.as_bytes().len() as u8))?;
        fd.write_all(self.as_bytes())
            .map_err(net_error::WriteError)?;
        Ok(())
    }

    fn consensus_deserialize<R: Read>(fd: &mut R) -> Result<UrlString, net_error> {
        let len_byte: u8 = read_next(fd)?;
        if len_byte > CLARITY_MAX_STRING_LENGTH {
            return Err(net_error::DeserializeError(
                "Failed to deserialize URL string: too long".to_string(),
            ));
        }
        let mut bytes = vec![0u8; len_byte as usize];
        fd.read_exact(&mut bytes).map_err(net_error::ReadError)?;

        // must encode a valid string
        let s = String::from_utf8(bytes).map_err(|_e| {
            net_error::DeserializeError(
                "Failed to parse URL string: could not contruct from utf8".to_string(),
            )
        })?;

        // must decode to a URL
        let url = UrlString::try_from(s).map_err(|e| {
            net_error::DeserializeError(format!("Failed to parse URL string: {:?}", e))
        })?;

        // must be a valid block URL, or empty string
        if url.len() > 0 {
            let _ = url.parse_to_block_url()?;
        }
        Ok(url)
    }
}

impl From<ClarityName> for StacksString {
    fn from(clarity_name: ClarityName) -> StacksString {
        // .unwrap() is safe since StacksString is less strict
        StacksString::from_str(&clarity_name).unwrap()
    }
}

impl From<ContractName> for StacksString {
    fn from(contract_name: ContractName) -> StacksString {
        // .unwrap() is safe since StacksString is less strict
        StacksString::from_str(&contract_name).unwrap()
    }
}

impl StacksString {
    /// Is the given string a valid Clarity string?
    pub fn is_valid_string(s: &String) -> bool {
        s.is_ascii() && StacksString::is_printable(s)
    }

    pub fn is_printable(s: &String) -> bool {
        if !s.is_ascii() {
            return false;
        }
        // all characters must be ASCII "printable" characters, excluding "delete".
        // This is 0x20 through 0x7e, inclusive, as well as '\t' and '\n'
        // TODO: DRY up with vm::representations
        for c in s.as_bytes().iter() {
            if (*c < 0x20 && *c != ('\t' as u8) && *c != ('\n' as u8)) || (*c > 0x7e) {
                return false;
            }
        }
        true
    }

    pub fn is_clarity_variable(&self) -> bool {
        // must parse to a single Clarity variable
        match lex(&self.to_string()) {
            Ok(lexed) => {
                if lexed.len() != 1 {
                    return false;
                }
                match lexed[0].0 {
                    LexItem::Variable(_) => true,
                    _ => false,
                }
            }
            Err(_) => false,
        }
    }

    pub fn from_string(s: &String) -> Option<StacksString> {
        if !StacksString::is_valid_string(s) {
            return None;
        }
        Some(StacksString(s.as_bytes().to_vec()))
    }

    pub fn from_str(s: &str) -> Option<StacksString> {
        if !StacksString::is_valid_string(&String::from(s)) {
            return None;
        }
        Some(StacksString(s.as_bytes().to_vec()))
    }

    pub fn to_string(&self) -> String {
        // guaranteed to always succeed because the string is ASCII
        String::from_utf8(self.0.clone()).unwrap()
    }
}

impl UrlString {
    /// Determine that the UrlString parses to something that can be used to fetch blocks via HTTP(S).
    /// A block URL must be an HTTP(S) URL without a query or fragment, and without a login.
    pub fn parse_to_block_url(&self) -> Result<url::Url, net_error> {
        // even though this code uses from_utf8_unchecked() internally, we've already verified that
        // the bytes in this string are all ASCII.
        let url = url::Url::parse(&self.to_string())
            .map_err(|e| net_error::DeserializeError(format!("Invalid URL: {:?}", &e)))?;

        if url.scheme() != "http" && url.scheme() != "https" {
            return Err(net_error::DeserializeError(format!(
                "Invalid URL: invalid scheme '{}'",
                url.scheme()
            )));
        }

        if url.username().len() > 0 || url.password().is_some() {
            return Err(net_error::DeserializeError(
                "Invalid URL: must not contain a username/password".to_string(),
            ));
        }

        if url.host_str().is_none() {
            return Err(net_error::DeserializeError(
                "Invalid URL: no host string".to_string(),
            ));
        }

        if url.query().is_some() {
            return Err(net_error::DeserializeError(
                "Invalid URL: query strings not supported for block URLs".to_string(),
            ));
        }

        if url.fragment().is_some() {
            return Err(net_error::DeserializeError(
                "Invalid URL: fragments are not supported for block URLs".to_string(),
            ));
        }

        Ok(url)
    }

    /// Is this URL routable?
    /// i.e. is the host _not_ 0.0.0.0 or ::?
    pub fn has_routable_host(&self) -> bool {
        let url = match url::Url::parse(&self.to_string()) {
            Ok(x) => x,
            Err(_) => {
                // should be unreachable
                return false;
            }
        };
        match url.host_str() {
            Some(host_str) => {
                if host_str == "0.0.0.0" || host_str == "[::]" || host_str == "::" {
                    return false;
                } else {
                    return true;
                }
            }
            None => {
                return false;
            }
        }
    }

    /// Get the port. Returns 0 for unknown
    pub fn get_port(&self) -> Option<u16> {
        let url = match url::Url::parse(&self.to_string()) {
            Ok(x) => x,
            Err(_) => {
                // unknown, but should be unreachable anyway
                return None;
            }
        };
        url.port_or_known_default()
    }
}

#[cfg(test)]
mod test {
    use super::*;

    use net::codec::test::check_codec_and_corruption;
    use net::codec::*;
    use net::*;

    use std::error::Error;

    #[test]
    fn tx_stacks_strings_codec() {
        let s = "hello-world";
        let stacks_str = StacksString::from_str(&s).unwrap();
        let clarity_str = ClarityName::try_from(s.clone()).unwrap();
        let contract_str = ContractName::try_from(s.clone()).unwrap();

        assert_eq!(stacks_str[..], s.as_bytes().to_vec()[..]);
        let s2 = stacks_str.to_string();
        assert_eq!(s2.to_string(), s.to_string());

        // stacks strings have a 4-byte length prefix
        let mut b = vec![];
        stacks_str.consensus_serialize(&mut b).unwrap();
        let mut bytes = vec![0x00, 0x00, 0x00, s.len() as u8];
        bytes.extend_from_slice(s.as_bytes());

        check_codec_and_corruption::<StacksString>(&stacks_str, &bytes);

        // clarity names and contract names have a 1-byte length prefix
        let mut clarity_bytes = vec![s.len() as u8];
        clarity_bytes.extend_from_slice(clarity_str.as_bytes());
        check_codec_and_corruption::<ClarityName>(&clarity_str, &clarity_bytes);

        let mut contract_bytes = vec![s.len() as u8];
        contract_bytes.extend_from_slice(contract_str.as_bytes());
        check_codec_and_corruption::<ContractName>(&contract_str, &clarity_bytes);
    }

    #[test]
    fn tx_stacks_string_invalid() {
        let s = "hello\rworld";
        assert!(StacksString::from_str(&s).is_none());

        let s = "hello\x01world";
        assert!(StacksString::from_str(&s).is_none());
    }

    #[test]
    fn test_contract_name_invalid() {
        let s = vec![0u8];
        assert!(ContractName::consensus_deserialize(&mut &s[..]).is_err());

        let s = vec![5u8, 0x66, 0x6f, 0x6f, 0x6f, 0x6f]; // "foooo"
        assert!(ContractName::consensus_deserialize(&mut &s[..]).is_ok());

        let s_body = [0x6fu8; CONTRACT_MAX_NAME_LENGTH + 1];
        let mut s_payload = vec![s_body.len() as u8];
        s_payload.extend_from_slice(&s_body);

        assert!(ContractName::consensus_deserialize(&mut &s_payload[..]).is_err());
    }

    #[test]
    fn test_url_parse() {
        assert!(UrlString::try_from("asdfjkl;")
            .unwrap()
            .parse_to_block_url()
            .unwrap_err()
            .to_string()
            .find("Invalid URL")
            .is_some());
        assert!(UrlString::try_from("http://")
            .unwrap()
            .parse_to_block_url()
            .unwrap_err()
            .to_string()
            .find("Invalid URL")
            .is_some());
        assert!(UrlString::try_from("ftp://ftp.google.com")
            .unwrap()
            .parse_to_block_url()
            .unwrap_err()
            .to_string()
            .find("invalid scheme")
            .is_some());
        assert!(UrlString::try_from("http://jude@google.com")
            .unwrap()
            .parse_to_block_url()
            .unwrap_err()
            .to_string()
            .find("must not contain a username/password")
            .is_some());
        assert!(UrlString::try_from("http://jude:pw@google.com")
            .unwrap()
            .parse_to_block_url()
            .unwrap_err()
            .to_string()
            .find("must not contain a username/password")
            .is_some());
        assert!(UrlString::try_from("http://www.google.com/foo/bar?baz=goo")
            .unwrap()
            .parse_to_block_url()
            .unwrap_err()
            .to_string()
            .find("query strings not supported")
            .is_some());
        assert!(UrlString::try_from("http://www.google.com/foo/bar#baz")
            .unwrap()
            .parse_to_block_url()
            .unwrap_err()
            .to_string()
            .find("fragments are not supported")
            .is_some());

        // don't need to cover the happy path too much, since the rust-url package already tests it.
        let url = UrlString::try_from("http://127.0.0.1:1234/v2/info")
            .unwrap()
            .parse_to_block_url()
            .unwrap();
        assert_eq!(url.host_str(), Some("127.0.0.1"));
        assert_eq!(url.port(), Some(1234));
        assert_eq!(url.path(), "/v2/info");
        assert_eq!(url.scheme(), "http");
    }
}
