// Copyright (c) The Starcoin Core Contributors
// SPDX-License-Identifier: Apache-2.0

use crate::state::CliState;
use crate::StarcoinOpt;
use anyhow::{bail, Result};
use scmd::{CommandAction, ExecContext};
use starcoin_crypto::ed25519::Ed25519PrivateKey;
use starcoin_crypto::{PrivateKey, ValidKeyStringExt};
use starcoin_types::account_address::AccountAddress;
use std::path::PathBuf;
use structopt::StructOpt;

#[derive(Debug, StructOpt)]
#[structopt(name = "import")]
pub struct ImportOpt {
    #[structopt(short = "a")]
    account: Option<AccountAddress>,
    #[structopt(short = "p", default_value = "")]
    password: String,

    #[structopt(name = "input", short = "i", help = "input of private key")]
    from_input: Option<String>,

    #[structopt(
        short = "f",
        help = "file path of private key",
        parse(from_os_str),
        conflicts_with("input")
    )]
    from_file: Option<PathBuf>,
}

pub struct ImportCommand;

impl CommandAction for ImportCommand {
    type State = CliState;
    type GlobalOpt = StarcoinOpt;
    type Opt = ImportOpt;

    fn run(&self, ctx: &ExecContext<Self::State, Self::GlobalOpt, Self::Opt>) -> Result<()> {
        let client = ctx.state().client();
        let opt: &ImportOpt = ctx.opt();

        let private_key = match (opt.from_input.as_ref(), opt.from_file.as_ref()) {
            (Some(p), _) => Ed25519PrivateKey::from_encoded_string(p)?,
            (None, Some(p)) => {
                let data = std::fs::read_to_string(p)?;
                Ed25519PrivateKey::from_encoded_string(data.as_str())?
            }
            (None, None) => {
                bail!("private key should be specified, use one of <input>, <from-file>")
            }
        };

        let address = opt
            .account
            .unwrap_or_else(|| AccountAddress::from_public_key(&private_key.public_key()));
        let account = client.account_import(
            address,
            private_key.to_bytes().to_vec(),
            opt.password.clone(),
        )?;
        println!("account imported:");
        println!("address: {}", account.address);
        println!(
            "public key: {}",
            account.public_key.to_encoded_string().unwrap()
        );
        Ok(())
    }
}
