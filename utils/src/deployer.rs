use anyhow::{ensure, Context};
use async_std::sync::Arc;
use clap::{builder::OsStr, Parser};
use contract_bindings::{
    light_client::LIGHTCLIENT_ABI, light_client_mock::LIGHTCLIENTMOCK_ABI,
    light_client_state_update_vk::LightClientStateUpdateVK,
    light_client_state_update_vk_mock::LightClientStateUpdateVKMock, plonk_verifier::PlonkVerifier,
    shared_types::LightClientState,
};
use derive_more::Display;
use ethers::{prelude::*, solc::artifacts::BytecodeObject};
use futures::future::{BoxFuture, FutureExt};
use hotshot_contract_adapter::light_client::ParsedLightClientState;
use std::{collections::HashMap, io::Write, ops::Deref};

/// Set of predeployed contracts.
#[derive(Clone, Debug, Parser)]
pub struct DeployedContracts {
    /// Use an already-deployed HotShot.sol instead of deploying a new one.
    #[clap(long, env = Contract::HotShot)]
    hotshot: Option<Address>,

    /// Use an already-deployed PlonkVerifier.sol instead of deploying a new one.
    #[clap(long, env = Contract::PlonkVerifier)]
    plonk_verifier: Option<Address>,

    /// Use an already-deployed LightClientStateUpdateVK.sol instead of deploying a new one.
    #[clap(long, env = Contract::StateUpdateVK)]
    light_client_state_update_vk: Option<Address>,

    /// Use an already-deployed LightClient.sol instead of deploying a new one.
    #[clap(long, env = Contract::LightClient)]
    light_client: Option<Address>,

    /// Use an already-deployed LightClient.sol proxy instead of deploying a new one.
    #[clap(long, env = Contract::LightClientProxy)]
    light_client_proxy: Option<Address>,
}

/// An identifier for a particular contract.
#[derive(Clone, Copy, Debug, Display, PartialEq, Eq, Hash)]
pub enum Contract {
    #[display(fmt = "ESPRESSO_SEQUENCER_HOTSHOT_ADDRESS")]
    HotShot,
    #[display(fmt = "ESPRESSO_SEQUENCER_PLONK_VERIFIER_ADDRESS")]
    PlonkVerifier,
    #[display(fmt = "ESPRESSO_SEQUENCER_LIGHT_CLIENT_STATE_UPDATE_VK_ADDRESS")]
    StateUpdateVK,
    #[display(fmt = "ESPRESSO_SEQUENCER_LIGHT_CLIENT_ADDRESS")]
    LightClient,
    #[display(fmt = "ESPRESSO_SEQUENCER_LIGHT_CLIENT_PROXY_ADDRESS")]
    LightClientProxy,
}

impl From<Contract> for OsStr {
    fn from(c: Contract) -> OsStr {
        c.to_string().into()
    }
}

/// Cache of contracts predeployed or deployed during this current run.
#[derive(Debug, Clone, Default)]
pub struct Contracts(HashMap<Contract, Address>);

impl From<DeployedContracts> for Contracts {
    fn from(deployed: DeployedContracts) -> Self {
        let mut m = HashMap::new();
        if let Some(addr) = deployed.hotshot {
            m.insert(Contract::HotShot, addr);
        }
        if let Some(addr) = deployed.plonk_verifier {
            m.insert(Contract::PlonkVerifier, addr);
        }
        if let Some(addr) = deployed.light_client_state_update_vk {
            m.insert(Contract::StateUpdateVK, addr);
        }
        if let Some(addr) = deployed.light_client {
            m.insert(Contract::LightClient, addr);
        }
        if let Some(addr) = deployed.light_client_proxy {
            m.insert(Contract::LightClientProxy, addr);
        }
        Self(m)
    }
}

impl Contracts {
    /// Deploy a contract by calling a function.
    ///
    /// The `deploy` function will be called only if contract `name` is not already deployed;
    /// otherwise this function will just return the predeployed address. The `deploy` function may
    /// access this [`Contracts`] object, so this can be used to deploy contracts recursively in
    /// dependency order.
    pub async fn deploy_fn(
        &mut self,
        name: Contract,
        deploy: impl FnOnce(&mut Self) -> BoxFuture<'_, anyhow::Result<Address>>,
    ) -> anyhow::Result<Address> {
        if let Some(addr) = self.0.get(&name) {
            tracing::info!("skipping deployment of {name}, already deployed at {addr:#x}");
            return Ok(*addr);
        }
        tracing::info!("deploying {name}");
        let addr = deploy(self).await?;
        tracing::info!("deployed {name} at {addr:#x}");

        self.0.insert(name, addr);
        Ok(addr)
    }

    /// Deploy a contract by executing its deploy transaction.
    ///
    /// The transaction will only be broadcast if contract `name` is not already deployed.
    pub async fn deploy_tx<M, C>(
        &mut self,
        name: Contract,
        tx: ContractDeployer<M, C>,
    ) -> anyhow::Result<Address>
    where
        M: Middleware + 'static,
        C: Deref<Target = ethers::contract::Contract<M>>
            + From<ContractInstance<Arc<M>, M>>
            + Send
            + 'static,
    {
        self.deploy_fn(name, |_| {
            async {
                let contract = tx.send().await?;
                Ok(contract.address())
            }
            .boxed()
        })
        .await
    }

    /// Write a .env file.
    pub fn write(&self, mut w: impl Write) -> anyhow::Result<()> {
        for (contract, address) in &self.0 {
            writeln!(w, "{contract}={address:#x}")?;
        }
        Ok(())
    }
}

/// Default deployment function `LightClient.sol` in production
///
/// # NOTE:
/// currently, `LightClient.sol` follows upgradable contract, thus a follow-up
/// call to `.initialize()` with proper genesis block (and other constructor args)
/// are expected to be *delegatecall-ed through the proxy contract*.
pub async fn deploy_light_client_contract<M: Middleware + 'static>(
    l1: Arc<M>,
    contracts: &mut Contracts,
) -> anyhow::Result<Address> {
    // Deploy library contracts.
    let plonk_verifier = contracts
        .deploy_tx(
            Contract::PlonkVerifier,
            PlonkVerifier::deploy(l1.clone(), ())?,
        )
        .await?;
    let vk = contracts
        .deploy_tx(
            Contract::StateUpdateVK,
            LightClientStateUpdateVK::deploy(l1.clone(), ())?,
        )
        .await?;

    // Link with LightClient's bytecode artifacts. We include the unlinked bytecode for the contract
    // in this binary so that the contract artifacts do not have to be distributed with the binary.
    // This should be fine because if the bindings we are importing are up to date, so should be the
    // contract artifacts: this is no different than foundry inlining bytecode objects in generated
    // bindings, except that foundry doesn't provide the bytecode for contracts that link with
    // libraries, so we have to do it ourselves.
    let mut bytecode: BytecodeObject = serde_json::from_str(include_str!(
        "../../contract-bindings/artifacts/LightClient_bytecode.json",
    ))?;
    bytecode
        .link_fully_qualified(
            "contracts/src/libraries/PlonkVerifier.sol:PlonkVerifier",
            plonk_verifier,
        )
        .resolve()
        .context("error linking PlonkVerifier lib")?;
    bytecode
        .link_fully_qualified(
            "contracts/src/libraries/LightClientStateUpdateVK.sol:LightClientStateUpdateVK",
            vk,
        )
        .resolve()
        .context("error linking LightClientStateUpdateVK lib")?;
    ensure!(!bytecode.is_unlinked(), "failed to link LightClient.sol");

    // Deploy light client.
    let light_client_factory = ContractFactory::new(
        LIGHTCLIENT_ABI.clone(),
        bytecode
            .as_bytes()
            .context("error parsing bytecode for linked LightClient contract")?
            .clone(),
        l1,
    );
    let contract = light_client_factory.deploy(())?.send().await?;
    Ok(contract.address())
}

/// Default deployment function `LightClientMock.sol` for testing
///
/// # NOTE
/// unlike [`deploy_light_client_contract()`], the `LightClientMock` doesn't
/// use upgradable contract for simplicity, thus there's no follow-up `.initialize()`
/// necessary, as we have already call its un-disabled constructor.
pub async fn deploy_mock_light_client_contract<M: Middleware + 'static>(
    l1: Arc<M>,
    contracts: &mut Contracts,
    constructor_args: Option<(LightClientState, u32)>,
) -> anyhow::Result<Address> {
    // Deploy library contracts.
    let plonk_verifier = contracts
        .deploy_tx(
            Contract::PlonkVerifier,
            PlonkVerifier::deploy(l1.clone(), ())?,
        )
        .await?;
    let vk = contracts
        .deploy_tx(
            Contract::StateUpdateVK,
            LightClientStateUpdateVKMock::deploy(l1.clone(), ())?,
        )
        .await?;

    let mut bytecode: BytecodeObject = serde_json::from_str(include_str!(
        "../../contract-bindings/artifacts/LightClientMock_bytecode.json",
    ))?;
    bytecode
        .link_fully_qualified(
            "contracts/src/libraries/PlonkVerifier.sol:PlonkVerifier",
            plonk_verifier,
        )
        .resolve()
        .context("error linking PlonkVerifier lib")?;
    bytecode
        .link_fully_qualified(
            "contracts/tests/mocks/LightClientStateUpdateVKMock.sol:LightClientStateUpdateVKMock",
            vk,
        )
        .resolve()
        .context("error linking LightClientStateUpdateVKMock lib")?;
    ensure!(
        !bytecode.is_unlinked(),
        "failed to link LightClientMock.sol"
    );

    // Deploy light client.
    let light_client_factory = ContractFactory::new(
        LIGHTCLIENTMOCK_ABI.clone(),
        bytecode
            .as_bytes()
            .context("error parsing bytecode for linked LightClientMock contract")?
            .clone(),
        l1,
    );
    let constructor_args = match constructor_args {
        Some(args) => args,
        None => (ParsedLightClientState::dummy_genesis().into(), u32::MAX),
    };
    let contract = light_client_factory
        .deploy(constructor_args)?
        .send()
        .await?;
    Ok(contract.address())
}
