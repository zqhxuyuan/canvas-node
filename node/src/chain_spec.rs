use cumulus_primitives_core::ParaId;
use sc_chain_spec::{ChainSpecExtension, ChainSpecGroup};
use sc_service::ChainType;
use serde::{Deserialize, Serialize};
use sp_core::{sr25519, Pair, Public};
use sp_runtime::traits::{IdentifyAccount, Verify, Zero};
use sp_consensus_aura::sr25519::AuthorityId as AuraId;

use canvas_runtime::{AccountId, BalancesConfig, GenesisConfig, SudoConfig, SystemConfig, Signature, CollatorSelectionConfig, SessionConfig, Balance};

/// Specialized `ChainSpec` for the normal parachain runtime.
pub type ChainSpec = sc_service::GenericChainSpec<canvas_runtime::GenesisConfig, Extensions>;

/// Helper function to generate a crypto pair from seed
pub fn get_from_seed<TPublic: Public>(seed: &str) -> <TPublic::Pair as Pair>::Public {
	TPublic::Pair::from_string(&format!("//{}", seed), None)
		.expect("static values are valid; qed")
		.public()
}

pub const EXISTENTIAL_DEPOSIT: Balance = 10 * CENTS;
pub const UNITS: Balance = 10_000_000_000;
pub const DOLLARS: Balance = UNITS;
pub const CENTS: Balance = UNITS / 100;        // 100_000_000
pub const MILLICENTS: Balance = CENTS / 1_000; // 100_000

/// The extensions for the [`ChainSpec`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ChainSpecGroup, ChainSpecExtension)]
#[serde(deny_unknown_fields)]
pub struct Extensions {
	/// The relay chain of the Parachain.
	pub relay_chain: String,
	/// The id of the Parachain.
	pub para_id: u32,
}

impl Extensions {
	/// Try to get the extension from the given `ChainSpec`.
	pub fn try_get(chain_spec: &dyn sc_service::ChainSpec) -> Option<&Self> {
		sc_chain_spec::get_extension(chain_spec.extensions())
	}
}

type AccountPublic = <Signature as Verify>::Signer;

/// Helper function to generate an account ID from seed
pub fn get_account_id_from_seed<TPublic: Public>(seed: &str) -> AccountId
	where
		AccountPublic: From<<TPublic::Pair as Pair>::Public>,
{
	AccountPublic::from(get_from_seed::<TPublic>(seed)).into_account()
}

pub fn get_pair_from_seed<TPublic: Public>(seed: &str) -> <TPublic::Pair as Pair>::Public {
	TPublic::Pair::from_string(&format!("//{}", seed), None)
		.expect("static values are valid; qed")
		.public()
}

pub fn get_collator_keys_from_seed(seed: &str) -> AuraId {
	get_pair_from_seed::<AuraId>(seed)
}

pub fn development_config(id: ParaId, relay: &str) -> Result<ChainSpec, String> {
	Ok(ChainSpec::from_genesis(
		"Development",
		"dev",
		ChainType::Development,
		move || testnet_genesis(
			get_account_id_from_seed::<sr25519::Public>("Alice"),
			vec![
				get_from_seed::<AuraId>("Alice"),
			],
			vec![(
					 get_account_id_from_seed::<sr25519::Public>("Alice"),
					 get_collator_keys_from_seed("Alice")
				 )
			],
			vec![
				get_account_id_from_seed::<sr25519::Public>("Alice"),
				get_account_id_from_seed::<sr25519::Public>("Bob"),
				get_account_id_from_seed::<sr25519::Public>("Alice//stash"),
				get_account_id_from_seed::<sr25519::Public>("Bob//stash"),
			],
			id,
			true,
		),
		vec![],
		None,
		None,
		None,
		Extensions {
			relay_chain: relay.into(),
			para_id: id.into(),
		},
	))
}

pub fn local_testnet_config(id: ParaId, relay_chain: &str) -> ChainSpec {
	ChainSpec::from_genesis(
		// Name
		"Local Testnet",
		// ID
		"local_testnet",
		ChainType::Local,
		move || {
			testnet_genesis(
				get_account_id_from_seed::<sr25519::Public>("Alice"),
				vec![
					get_from_seed::<AuraId>("Alice"),
					get_from_seed::<AuraId>("Bob"),
				],
				vec![(
						 get_account_id_from_seed::<sr25519::Public>("Alice"),
						 get_collator_keys_from_seed("Alice")
					 ),
					 (
						 get_account_id_from_seed::<sr25519::Public>("Bob"),
						 get_collator_keys_from_seed("Bob")
					 ),
				],
				vec![
					get_account_id_from_seed::<sr25519::Public>("Alice"),
					get_account_id_from_seed::<sr25519::Public>("Bob"),
					get_account_id_from_seed::<sr25519::Public>("Charlie"),
					get_account_id_from_seed::<sr25519::Public>("Dave"),
					get_account_id_from_seed::<sr25519::Public>("Eve"),
					get_account_id_from_seed::<sr25519::Public>("Ferdie"),
					get_account_id_from_seed::<sr25519::Public>("Alice//stash"),
					get_account_id_from_seed::<sr25519::Public>("Bob//stash"),
					get_account_id_from_seed::<sr25519::Public>("Charlie//stash"),
					get_account_id_from_seed::<sr25519::Public>("Dave//stash"),
					get_account_id_from_seed::<sr25519::Public>("Eve//stash"),
					get_account_id_from_seed::<sr25519::Public>("Ferdie//stash"),
				],
				id,
				true,
			)
		},
		vec![],
		None,
		None,
		None,
		Extensions {
			relay_chain: relay_chain.into(),
			para_id: id.into(),
		},
	)
}

fn testnet_genesis(
	root_key: AccountId,
	initial_authorities: Vec<AuraId>,
	invulnerables: Vec<(AccountId, AuraId)>,
	endowed_accounts: Vec<AccountId>,
	parachain_id: ParaId,
	enable_println: bool
) -> GenesisConfig {

	GenesisConfig {
		system: SystemConfig {
			// Add Wasm runtime to storage.
			code: canvas_runtime::WASM_BINARY
				.expect("WASM binary was not build, please build it!")
				.to_vec(),
			changes_trie_config: Default::default(),
		},
		balances: BalancesConfig {
			// Configure endowed accounts with initial balance of 1 << 60.
			balances: endowed_accounts
				.iter()
				.cloned()
				.map(|k|(k, 1 << 60))
				.collect(),
		},
		parachain_info: canvas_runtime::ParachainInfoConfig { parachain_id },
		sudo: SudoConfig {
			// Assign network admin rights.
			key: root_key,
		},
		collator_selection: CollatorSelectionConfig {
			invulnerables: invulnerables.iter().cloned().map(|(acc, _)| acc).collect(),
			candidacy_bond: Zero::zero(),
			..Default::default()
		},
		session: SessionConfig {
			keys: invulnerables.iter().cloned().map(|(acc, aura)| (
				acc.clone(), // account id
				acc.clone(), // validator id
				statemint_session_keys(aura), // session keys
			)).collect()
		},
		// no need to pass anything to aura, in fact it will panic if we do. Session will take care of this.
		aura: Default::default(),
		// aura: AuraConfig {
		// 	authorities: initial_authorities,
		// },
		aura_ext: Default::default(),
		parachain_system: Default::default(),
	}
}

pub fn statemint_session_keys(keys: AuraId) -> canvas_runtime::opaque::SessionKeys {
	canvas_runtime::opaque::SessionKeys { aura: keys }
}