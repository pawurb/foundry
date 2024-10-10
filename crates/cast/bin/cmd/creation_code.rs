use alloy_primitives::{Address, Bytes};
use alloy_provider::{ext::TraceApi, Provider};
use alloy_rpc_types::trace::parity::{Action, CreateAction, CreateOutput, TraceOutput};
use cast::SimpleCast;
use clap::{command, Parser};
use eyre::Result;
use foundry_block_explorers::Client;
use foundry_cli::{
    opts::{EtherscanOpts, RpcOpts},
    utils,
};
use foundry_common::provider::RetryProvider;
use foundry_config::Config;

use super::interface::fetch_abi_from_etherscan;

/// CLI arguments for `cast creation-code`.
#[derive(Parser)]
pub struct CreationCodeArgs {
    /// An Ethereum address, for which the bytecode will be fetched.
    contract: Address,

    /// Disassemble bytecodes into individual opcodes.
    #[arg(long)]
    disassemble: bool,

    /// Return creation bytecode without constructor arguments appended.
    #[arg(long)]
    without_args: bool,

    /// Return only constructor arguments.
    #[arg(long)]
    only_args: bool,

    #[command(flatten)]
    etherscan: EtherscanOpts,
    #[command(flatten)]
    rpc: RpcOpts,
}

impl CreationCodeArgs {
    pub async fn run(self) -> Result<()> {
        let Self { contract, etherscan, rpc, disassemble, without_args, only_args } = self;

        if without_args && only_args {
            return Err(eyre::eyre!("--without-args and --only-args are mutually exclusive."));
        }

        let config = Config::from(&etherscan);
        let chain = config.chain.unwrap_or_default();
        let api_key = config.get_etherscan_api_key(Some(chain)).unwrap_or_default();
        let client = Client::new(chain, api_key)?;

        let config = Config::from(&rpc);
        let provider = utils::get_provider(&config)?;

        let bytecode = fetch_creation_code(contract, client, provider).await?;

        let bytecode =
            parse_code_output(bytecode, contract, &etherscan, without_args, only_args).await?;

        if disassemble {
            println!("{}", SimpleCast::disassemble(&bytecode)?);
        } else {
            print!("{bytecode}");
        }

        Ok(())
    }
}

/// Parses the creation bytecode to return either the bytecode, or bytecoe without constructor
/// arguments or only the constructor arguments.
async fn parse_code_output(
    bytecode: Bytes,
    contract: Address,
    etherscan: &EtherscanOpts,
    without_args: bool,
    only_args: bool,
) -> Result<Bytes> {
    if !without_args && !only_args {
        return Ok(bytecode);
    }

    let abi = fetch_abi_from_etherscan(contract, etherscan).await?;
    let abi = abi.into_iter().next().ok_or_else(|| eyre::eyre!("No ABI found."))?;
    let (abi, _) = abi;

    if abi.constructor.is_none() {
        if only_args {
            return Err(eyre::eyre!("No constructor found."));
        }
        return Ok(bytecode);
    }

    let constructor = abi.constructor.unwrap();
    if constructor.inputs.is_empty() {
        if only_args {
            return Err(eyre::eyre!("No constructor arguments found."));
        }
        return Ok(bytecode);
    }

    let args_size = constructor.inputs.len() * 32;

    let bytecode = if without_args {
        Bytes::from(bytecode[..bytecode.len() - args_size].to_vec())
    } else if only_args {
        Bytes::from(bytecode[bytecode.len() - args_size..].to_vec())
    } else {
        panic!("Unreachable.")
    };

    Ok(bytecode)
}

/// Fetches the creation code of a contract from Etherscan and RPC.
pub async fn fetch_creation_code(
    contract: Address,
    client: Client,
    provider: RetryProvider,
) -> Result<Bytes> {
    let creation_data = client.contract_creation_data(contract).await?;
    let creation_tx_hash = creation_data.transaction_hash;
    let tx_data = provider.get_transaction_by_hash(creation_tx_hash).await?;
    let tx_data = tx_data.ok_or_else(|| eyre::eyre!("Could not find creation tx data."))?;

    let bytecode = if tx_data.inner.to.is_none() {
        // Contract was created using a standard transaction
        tx_data.inner.input
    } else {
        // Contract was created using a factory pattern or create2
        // Extract creation code from tx traces
        let mut creation_bytecode = None;

        let traces = provider.trace_transaction(creation_tx_hash).await.map_err(|e| {
            eyre::eyre!("Could not fetch traces for transaction {}: {}", creation_tx_hash, e)
        })?;

        for trace in traces {
            if let Some(TraceOutput::Create(CreateOutput { address, code: _, gas_used: _ })) =
                trace.trace.result
            {
                if address == contract {
                    creation_bytecode = match trace.trace.action {
                        Action::Create(CreateAction { init, value: _, from: _, gas: _ }) => {
                            Some(init)
                        }
                        _ => None,
                    };
                }
            }
        }

        creation_bytecode.ok_or_else(|| eyre::eyre!("Could not find contract creation trace."))?
    };

    Ok(bytecode)
}
