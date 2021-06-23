// Copyright 2019 Parity Technologies (UK) Ltd.
// This file is part of Cumulus.

// Cumulus is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Cumulus is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Cumulus.  If not, see <http://www.gnu.org/licenses/>.

use cumulus_client_consensus_aura::{build_aura_consensus, BuildAuraConsensusParams, SlotProportion};
use cumulus_client_consensus_common::{
	ParachainConsensus, ParachainCandidate, ParachainBlockImport,
};
use cumulus_client_consensus_relay_chain::{
	build_relay_chain_consensus,
	BuildRelayChainConsensusParams,
	Verifier as RelayChainVerifier
};
use cumulus_client_network::build_block_announce_validator;
use cumulus_client_service::{
	prepare_node_config, start_collator, start_full_node, StartCollatorParams, StartFullNodeParams,
};
use canvas_runtime::{opaque::Block, RuntimeApi, Header};
use polkadot_primitives::v0::CollatorPair;
use sc_executor::native_executor_instance;
pub use sc_executor::NativeExecutor;
use sc_service::{Configuration, PartialComponents, Role, TFullBackend, TFullClient, TaskManager};
use sc_telemetry::{Telemetry, TelemetryWorker, TelemetryWorkerHandle};
use sp_runtime::traits::{BlakeTwo256, Header as HeaderT};
use sp_trie::PrefixedMemoryDB;
use std::sync::{Arc};
use futures::lock::Mutex;
use sp_consensus::{
	BlockImportParams, BlockOrigin, SlotData,
	import_queue::{BasicQueue, CacheKeyId, Verifier as VerifierT},
};
use sc_client_api::ExecutorProvider;
use cumulus_primitives_core::{
	ParaId, relay_chain::v1::{Hash as PHash, PersistedValidationData},
};
use sp_runtime::generic::{BlockId};
use sp_api::ApiExt;
use sp_consensus_aura::{sr25519::AuthorityId as AuraId, AuraApi, sr25519::AuthorityPair as AuraPair};
use sc_consensus_aura::ImportQueueParams;

// Native executor instance.
native_executor_instance!(
	pub Executor,
	canvas_runtime::api::dispatch,
	canvas_runtime::native_version,
);

enum BuildOnAccess<R> {
	Uninitialized(Option<Box<dyn FnOnce() -> R + Send + Sync>>),
	Initialized(R),
}

impl<R> BuildOnAccess<R> {
	fn get_mut(&mut self) -> &mut R {
		loop {
			match self {
				Self::Uninitialized(f) => {
					*self = Self::Initialized((f.take().unwrap())());
				}
				Self::Initialized(ref mut r) => return r,
			}
		}
	}
}

/// Special [`ParachainConsensus`] implementation that waits for the upgrade from
/// shell to a parachain runtime that implements Aura.
struct WaitForAuraConsensus<Client> {
	client: Arc<Client>,
	aura_consensus: Arc<Mutex<BuildOnAccess<Box<dyn ParachainConsensus<Block>>>>>,
	relay_chain_consensus: Arc<Mutex<Box<dyn ParachainConsensus<Block>>>>,
}

impl<Client> Clone for WaitForAuraConsensus<Client> {
	fn clone(&self) -> Self {
		Self {
			client: self.client.clone(),
			aura_consensus: self.aura_consensus.clone(),
			relay_chain_consensus: self.relay_chain_consensus.clone(),
		}
	}
}

#[async_trait::async_trait]
impl<Client> ParachainConsensus<Block> for WaitForAuraConsensus<Client>
	where
		Client: sp_api::ProvideRuntimeApi<Block> + Send + Sync,
		Client::Api: AuraApi<Block, AuraId>,
{
	async fn produce_candidate(
		&mut self,
		parent: &Header,
		relay_parent: PHash,
		validation_data: &PersistedValidationData,
	) -> Option<ParachainCandidate<Block>> {
		let block_id = BlockId::hash(parent.hash());
		if self
			.client
			.runtime_api()
			.has_api::<dyn AuraApi<Block, AuraId>>(&block_id)
			.unwrap_or(false)
		{
			self.aura_consensus
				.lock()
				.await
				.get_mut()
				.produce_candidate(parent, relay_parent, validation_data)
				.await
		} else {
			self.relay_chain_consensus
				.lock()
				.await
				.produce_candidate(parent, relay_parent, validation_data)
				.await
		}
	}
}

struct Verifier<Client> {
	client: Arc<Client>,
	aura_verifier: BuildOnAccess<Box<dyn VerifierT<Block>>>,
	relay_chain_verifier: Box<dyn VerifierT<Block>>,
}

#[async_trait::async_trait]
impl<Client> VerifierT<Block> for Verifier<Client>
	where
		Client: sp_api::ProvideRuntimeApi<Block> + Send + Sync,
		Client::Api: AuraApi<Block, AuraId>,
{
	async fn verify(
		&mut self,
		origin: BlockOrigin,
		header: Header,
		justifications: Option<sp_runtime::Justifications>,
		body: Option<Vec<<Block as sp_runtime::traits::Block>::Extrinsic>>,
	) -> Result<
		(
			BlockImportParams<Block, ()>,
			Option<Vec<(CacheKeyId, Vec<u8>)>>,
		),
		String,
	> {
		let block_id = BlockId::hash(*header.parent_hash());

		if self
			.client
			.runtime_api()
			.has_api::<dyn AuraApi<Block, AuraId>>(&block_id)
			.unwrap_or(false)
		{
			self.aura_verifier
				.get_mut()
				.verify(origin, header, justifications, body)
				.await
		} else {
			self.relay_chain_verifier
				.verify(origin, header, justifications, body)
				.await
		}
	}
}

/// Starts a `ServiceBuilder` for a full service.
///
/// Use this macro if you don't actually need the full service, but just the builder in order to
/// be able to perform chain operations.
pub fn new_partial(
	config: &Configuration,
) -> Result<
	PartialComponents<
		TFullClient<Block, RuntimeApi, Executor>,
		TFullBackend<Block>,
		(),
		sp_consensus::import_queue::BasicQueue<Block, PrefixedMemoryDB<BlakeTwo256>>,
		sc_transaction_pool::FullPool<Block, TFullClient<Block, RuntimeApi, Executor>>,
		(Option<Telemetry>, Option<TelemetryWorkerHandle>),
	>,
	sc_service::Error,
> {
	let telemetry = config.telemetry_endpoints.clone()
		.filter(|x| !x.is_empty())
		.map(|endpoints| -> Result<_, sc_telemetry::Error> {
			let worker = TelemetryWorker::new(16)?;
			let telemetry = worker.handle().new_telemetry(endpoints);
			Ok((worker, telemetry))
		})
		.transpose()?;

	let (client, backend, keystore_container, task_manager) =
		sc_service::new_full_parts::<Block, RuntimeApi, Executor>(
			&config,
			telemetry.as_ref().map(|(_, telemetry)| telemetry.handle()),
		)?;
	let client = Arc::new(client);

	let telemetry_worker_handle = telemetry
		.as_ref()
		.map(|(worker, _)| worker.handle());

	let telemetry = telemetry
		.map(|(worker, telemetry)| {
			task_manager.spawn_handle().spawn("telemetry", worker.run());
			telemetry
		});

	let registry = config.prometheus_registry();

	let transaction_pool = sc_transaction_pool::BasicPool::new_full(
		config.transaction_pool.clone(),
		config.role.is_authority().into(),
		config.prometheus_registry(),
		task_manager.spawn_essential_handle(),
		client.clone(),
	);

	// cumulus relay chain import queue
	// let import_queue = cumulus_client_consensus_relay_chain::import_queue(
	// 	client.clone(),
	// 	client.clone(),
	// 	|_, _| async { Ok(sp_timestamp::InherentDataProvider::from_system_time()) },
	// 	&task_manager.spawn_essential_handle(),
	// 	registry.clone(),
	// )?;

	// with verifier begin.
	// let telemetry_handle = telemetry.as_ref().map(|telemetry| telemetry.handle());
	// let client2 = client.clone();
	//
	// let aura_verifier = move || {
	// 	let slot_duration = cumulus_client_consensus_aura::slot_duration(&*client2).unwrap();
	//
	// 	Box::new(cumulus_client_consensus_aura::build_verifier::<
	// 		sp_consensus_aura::sr25519::AuthorityPair,
	// 		_,
	// 		_,
	// 		_,
	// 	>(cumulus_client_consensus_aura::BuildVerifierParams {
	// 		client: client2.clone(),
	// 		create_inherent_data_providers: move |_, _| async move {
	// 			let time = sp_timestamp::InherentDataProvider::from_system_time();
	//
	// 			let slot =
	// 				sp_consensus_aura::inherents::InherentDataProvider::from_timestamp_and_duration(
	// 					*time,
	// 					slot_duration.slot_duration(),
	// 				);
	//
	// 			Ok((time, slot))
	// 		},
	// 		can_author_with: sp_consensus::CanAuthorWithNativeVersion::new(
    //                 client2.executor().clone(),
	// 		),
	// 		telemetry: telemetry_handle,
	// 	})) as Box<_>
	// };
	//
	// let relay_chain_verifier = Box::new(RelayChainVerifier::new(client.clone(), |_, _| async {
	// 	Ok(())
	// })) as Box<_>;
	//
	// let verifier = Verifier {
	// 	client: client.clone(),
	// 	relay_chain_verifier,
	// 	aura_verifier: BuildOnAccess::Uninitialized(Some(Box::new(aura_verifier))),
	// };
	//
	// let spawner = task_manager.spawn_essential_handle();
	// let registry = config.prometheus_registry().clone();
	//
	// let import_queue = BasicQueue::new(
	// 	verifier,
	// 	Box::new(ParachainBlockImport::new(client.clone())),
	// 	None,
	// 	&spawner,
	// 	registry,
	// );
	// with verifier end.

	// aura import queue
	// let slot_duration = sc_consensus_aura::slot_duration(&*client)?.slot_duration();
	// let import_queue = sc_consensus_aura::import_queue::<AuraPair, _, _, _, _, _, _>(ImportQueueParams {
	// 	block_import: client.clone(),
	// 	justification_import: None,
	// 	client: client.clone(),
	// 	create_inherent_data_providers: move |_, ()| async move {
	// 		let timestamp = sp_timestamp::InherentDataProvider::from_system_time();
	//
	// 		let slot = sp_consensus_aura::inherents::InherentDataProvider::from_timestamp_and_duration(
	// 			*timestamp,
	// 			slot_duration,
	// 		);
	//
	// 		Ok((timestamp, slot))
	// 	},
	// 	spawner: &task_manager.spawn_essential_handle(),
	// 	registry,
	// 	can_author_with: sp_consensus::CanAuthorWithNativeVersion::new(client.executor().clone()),
	// 	check_for_equivocation: Default::default(),
	// 	telemetry: telemetry.as_ref().map(|x| x.handle()),
	// })?;

	// cumulus aura import queue
	let slot_duration = cumulus_client_consensus_aura::slot_duration(&*client)?;
	let import_queue = cumulus_client_consensus_aura::import_queue::<sp_consensus_aura::sr25519::AuthorityPair, _, _, _, _, _, _>(
		cumulus_client_consensus_aura::ImportQueueParams {
			block_import: client.clone(),
			client: client.clone(),
			create_inherent_data_providers: move |_, _| async move {
				let time = sp_timestamp::InherentDataProvider::from_system_time();
				let slot = sp_consensus_aura::inherents::InherentDataProvider::from_timestamp_and_duration(
					*time,
					slot_duration.slot_duration(),
				);
				Ok((time, slot))
			},
			registry,
			can_author_with: sp_consensus::CanAuthorWithNativeVersion::new(client.executor().clone()),
			spawner: &task_manager.spawn_essential_handle(),
			telemetry: telemetry.as_ref().map(|telemetry| telemetry.handle()),
		},
	)?;

	let params = PartialComponents {
		backend,
		client,
		import_queue,
		keystore_container,
		task_manager,
		transaction_pool,
		// inherent_data_providers,
		select_chain: (),
		other: (telemetry, telemetry_worker_handle),
	};

	Ok(params)
}

/// Start a node with the given parachain `Configuration` and relay chain `Configuration`.
///
/// This is the actual implementation that is abstract over the executor and the runtime api.
#[sc_tracing::logging::prefix_logs_with("Parachain")]
async fn start_node_impl(
	parachain_config: Configuration,
	collator_key: CollatorPair,
	polkadot_config: Configuration,
	id: ParaId,
	validator: bool,
) -> sc_service::error::Result<(TaskManager, Arc<TFullClient<Block, RuntimeApi, Executor>>)> {
	if matches!(parachain_config.role, Role::Light) {
		return Err("Light client not supported!".into());
	}

	let parachain_config = prepare_node_config(parachain_config);

	let params = new_partial(&parachain_config)?;
	let (mut telemetry, telemetry_worker_handle) = params.other;

	let polkadot_full_node =
		cumulus_client_service::build_polkadot_full_node(
			polkadot_config,
			telemetry_worker_handle,
		)
			.map_err(|e| match e {
				polkadot_service::Error::Sub(x) => x,
				s => format!("{}", s).into(),
			})?;

	let client = params.client.clone();
	let backend = params.backend.clone();
	let block_announce_validator = build_block_announce_validator(
		polkadot_full_node.client.clone(),
		id,
		Box::new(polkadot_full_node.network.clone()),
		polkadot_full_node.backend.clone(),
	);

	let force_authoring = parachain_config.force_authoring;
	let prometheus_registry = parachain_config.prometheus_registry().cloned();
	let transaction_pool = params.transaction_pool.clone();
	let mut task_manager = params.task_manager;
	// let import_queue = params.import_queue;
	let import_queue = cumulus_client_service::SharedImportQueue::new(params.import_queue);
	let (network, system_rpc_tx, start_network) =
		sc_service::build_network(sc_service::BuildNetworkParams {
			config: &parachain_config,
			client: client.clone(),
			transaction_pool: transaction_pool.clone(),
			spawn_handle: task_manager.spawn_handle(),
			import_queue: import_queue.clone(),
			on_demand: None,
			block_announce_validator_builder: Some(Box::new(|_| block_announce_validator)),
		})?;

	let rpc_extensions_builder = {
		let client = client.clone();
		let pool = transaction_pool.clone();

		Box::new(move |deny_unsafe, _| {
			let deps = crate::rpc::FullDeps {
				client: client.clone(),
				pool: pool.clone(),
				deny_unsafe,
			};

			crate::rpc::create_full(deps)
		})
	};

	sc_service::spawn_tasks(sc_service::SpawnTasksParams {
		on_demand: None,
		remote_blockchain: None,
		rpc_extensions_builder,
		client: client.clone(),
		transaction_pool: transaction_pool.clone(),
		task_manager: &mut task_manager,
		config: parachain_config,
		keystore: params.keystore_container.sync_keystore(),
		backend: backend.clone(),
		network: network.clone(),
		system_rpc_tx,
		telemetry: telemetry.as_mut(),
	})?;

	let announce_block = {
		let network = network.clone();
		Arc::new(move |hash, data| network.announce_block(hash, data))
	};

	let keystore = params.keystore_container.sync_keystore();
	let wait_for_aura = false;

	if validator {
		// https://github.com/paritytech/cumulus/blob/polkadot-v0.9.5/polkadot-parachains/src/service.rs#L313
		// build_consensus start.
		// let parachain_consensus: Box<dyn ParachainConsensus<Block>> = if wait_for_aura {
		// 	let client2 = client.clone();
		// 	let relay_chain_backend = polkadot_full_node.backend.clone();
		// 	let relay_chain_client = polkadot_full_node.client.clone();
		// 	let spawn_handle = task_manager.spawn_handle();
		// 	let transaction_pool2 = transaction_pool.clone();
		// 	let prometheus_registry2 = prometheus_registry.as_ref().map(|r| (*r).clone());
		// 	let telemetry = telemetry.as_ref().map(|t| t.handle());
		// 	let telemetry2 = telemetry.clone();
		//
		// 	let aura_consensus = BuildOnAccess::Uninitialized(Some(
		// 		Box::new(move || {
		// 			let slot_duration =
		// 				cumulus_client_consensus_aura::slot_duration(&*client2).unwrap();
		//
		// 			let proposer_factory =
		// 				sc_basic_authorship::ProposerFactory::with_proof_recording(
		// 					spawn_handle,
		// 					client2.clone(),
		// 					transaction_pool2,
		// 					prometheus_registry2.as_ref(),
		// 					telemetry2.clone(),
		// 				);
		//
		// 			let relay_chain_backend2 = relay_chain_backend.clone();
		// 			let relay_chain_client2 = relay_chain_client.clone();
		//
		// 			build_aura_consensus::<sp_consensus_aura::sr25519::AuthorityPair, _, _, _, _, _, _, _, _, _, >(BuildAuraConsensusParams {
		// 				proposer_factory,
		// 				create_inherent_data_providers:
		// 				move |_, (relay_parent, validation_data)| {
		// 					let parachain_inherent =
		// 						cumulus_primitives_parachain_inherent::ParachainInherentData::create_at_with_client(
		// 							relay_parent,
		// 							&relay_chain_client,
		// 							&*relay_chain_backend,
		// 							&validation_data,
		// 							id,
		// 						);
		// 					async move {
		// 						let time = sp_timestamp::InherentDataProvider::from_system_time();
		// 						let slot =
		// 							sp_consensus_aura::inherents::InherentDataProvider::from_timestamp_and_duration(
		// 								*time,
		// 								slot_duration.slot_duration(),
		// 							);
		// 						let parachain_inherent =
		// 							parachain_inherent.ok_or_else(|| {
		// 								Box::<dyn std::error::Error + Send + Sync>::from(
		// 									"Failed to create parachain inherent",
		// 								)
		// 							})?;
		// 						Ok((time, slot, parachain_inherent))
		// 					}
		// 				},
		// 				block_import: client2.clone(),
		// 				relay_chain_client: relay_chain_client2,
		// 				relay_chain_backend: relay_chain_backend2,
		// 				para_client: client2.clone(),
		// 				backoff_authoring_blocks: Option::<()>::None,
		// 				sync_oracle: network.clone(),
		// 				keystore,
		// 				force_authoring,
		// 				slot_duration,
		// 				// We got around 500ms for proposing
		// 				block_proposal_slot_portion: SlotProportion::new(1f32 / 24f32),
		// 				telemetry: telemetry2,
		// 			})
		// 		}),
		// 	));
		//
		// 	let proposer_factory = sc_basic_authorship::ProposerFactory::with_proof_recording(
		// 		task_manager.spawn_handle(),
		// 		client.clone(),
		// 		transaction_pool.clone(),
		// 		prometheus_registry.as_ref(),
		// 		telemetry.clone(),
		// 	);
		//
		// 	let relay_chain_backend = polkadot_full_node.backend.clone();
		// 	let relay_chain_client = polkadot_full_node.client.clone();
		//
		// 	let relay_chain_consensus =
		// 		cumulus_client_consensus_relay_chain::build_relay_chain_consensus(
		// 			cumulus_client_consensus_relay_chain::BuildRelayChainConsensusParams {
		// 				para_id: id,
		// 				proposer_factory,
		// 				block_import: client.clone(),
		// 				relay_chain_client: polkadot_full_node.client.clone(),
		// 				relay_chain_backend: polkadot_full_node.backend.clone(),
		// 				create_inherent_data_providers:
		// 				move |_, (relay_parent, validation_data)| {
		// 					let parachain_inherent =
		// 						cumulus_primitives_parachain_inherent::ParachainInherentData::create_at_with_client(
		// 							relay_parent,
		// 							&relay_chain_client,
		// 							&*relay_chain_backend,
		// 							&validation_data,
		// 							id,
		// 						);
		// 					async move {
		// 						let parachain_inherent =
		// 							parachain_inherent.ok_or_else(|| {
		// 								Box::<dyn std::error::Error + Send + Sync>::from(
		// 									"Failed to create parachain inherent",
		// 								)
		// 							})?;
		// 						Ok(parachain_inherent)
		// 					}
		// 				},
		// 			},
		// 		);
		//
		// 	let parachain_consensus = Box::new(WaitForAuraConsensus {
		// 		client: client.clone(),
		// 		aura_consensus: Arc::new(Mutex::new(aura_consensus)),
		// 		relay_chain_consensus: Arc::new(Mutex::new(relay_chain_consensus)),
		// 	});
		// 	parachain_consensus
		// } else {
			let client2 = client.clone();
			let slot_duration = cumulus_client_consensus_aura::slot_duration(&*client2)?;
			let telemetry2 = telemetry.as_ref().map(|t| t.handle());

			let proposer_factory = sc_basic_authorship::ProposerFactory::with_proof_recording(
				task_manager.spawn_handle(),
				client2.clone(),
				transaction_pool,
				prometheus_registry.as_ref(),
				telemetry2.clone(),
			);

			let relay_chain_backend = polkadot_full_node.backend.clone();
			let relay_chain_client = polkadot_full_node.client.clone();
			let parachain_consensus = build_aura_consensus::<AuraPair, _, _, _, _, _, _, _, _, _>(
				BuildAuraConsensusParams {
					proposer_factory,
					create_inherent_data_providers: move |_, (relay_parent, validation_data)| {
						let parachain_inherent =
							cumulus_primitives_parachain_inherent::ParachainInherentData::create_at_with_client(
								relay_parent,
								&relay_chain_client,
								&*relay_chain_backend,
								&validation_data,
								id,
							);
						async move {
							let time = sp_timestamp::InherentDataProvider::from_system_time();

							let slot = sp_consensus_aura::inherents::InherentDataProvider::from_timestamp_and_duration(
								*time,
								slot_duration.slot_duration(),
							);

							let parachain_inherent = parachain_inherent.ok_or_else(|| {
								Box::<dyn std::error::Error + Send + Sync>::from("Failed to create parachain inherent")
							})?;
							Ok((time, slot, parachain_inherent))
						}
					},
					block_import: client2.clone(),
					relay_chain_client: polkadot_full_node.client.clone(),
					relay_chain_backend: polkadot_full_node.backend.clone(),
					para_client: client2,
					backoff_authoring_blocks: Option::<()>::None,
					sync_oracle: network.clone(),
					keystore,
					force_authoring,
					slot_duration,
					// We got around 500ms for proposing
					block_proposal_slot_portion: SlotProportion::new(1f32 / 24f32),
					telemetry: telemetry2,
				},
			);
			// parachain_consensus
		// };
		// build_consensus end.

		let spawner = task_manager.spawn_handle();

		let params = StartCollatorParams {
			para_id: id,
			block_status: client.clone(),
			announce_block,
			client: client.clone(),
			task_manager: &mut task_manager,
			relay_chain_full_node: polkadot_full_node,
			spawner,
			parachain_consensus,
			import_queue
		};

		start_collator(params).await?;
	} else {
		let params = StartFullNodeParams {
			client: client.clone(),
			announce_block,
			task_manager: &mut task_manager,
			para_id: id,
			relay_chain_full_node: polkadot_full_node,
		};

		start_full_node(params)?;
	}

	start_network.start_network();


	Ok((task_manager, client))
}

/// Start a normal parachain node.
pub async fn start_node(
	parachain_config: Configuration,
	collator_key: CollatorPair,
	polkadot_config: Configuration,
	id: ParaId,
	validator: bool,
) -> sc_service::error::Result<(TaskManager, Arc<TFullClient<Block, RuntimeApi, Executor>>)> {
	start_node_impl(
		parachain_config,
		collator_key,
		polkadot_config,
		id,
		validator,
	)
		.await
}
