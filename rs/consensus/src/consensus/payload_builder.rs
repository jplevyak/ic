//! Payload creation/validation subcomponent

use crate::consensus::metrics::PayloadBuilderMetrics;
use ic_interfaces::{
    consensus::{
        PayloadBuilderError, PayloadPermanentError, PayloadTransientError, PayloadValidationError,
    },
    ingress_manager::{IngressSelector, IngressSetQuery},
    ingress_pool::IngressPoolSelect,
    messaging::XNetPayloadBuilder,
    registry::RegistryClient,
    self_validating_payload::SelfValidatingPayloadBuilder,
    validation::{ValidationError, ValidationResult},
};
use ic_logger::{warn, ReplicaLogger};
use ic_metrics::MetricsRegistry;
use ic_registry_client::helper::subnet::SubnetRegistry;
use ic_types::{
    artifact::IngressMessageId,
    batch::{BatchPayload, SelfValidatingPayload, ValidationContext, XNetPayload},
    consensus::{BlockPayload, Payload},
    crypto::CryptoHashOf,
    messages::MAX_XNET_PAYLOAD_IN_BYTES,
    CountBytes, Height, NumBytes, SubnetId, Time,
};
use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, RwLock};

/// The PayloadBuilder is responsible for creating and validating payload that
/// is included in consensus blocks.
pub trait PayloadBuilder: Send + Sync {
    /// Produces a payload that is valid given `past_payloads` and `context`.
    ///
    /// `past_payloads` contains the `Payloads` from all blocks above the
    /// certified height provided in `context`, in descending block height
    /// order.
    fn get_payload(
        &self,
        height: Height,
        ingress_pool: &dyn IngressPoolSelect,
        past_payloads: &[(Height, Time, Payload)],
        context: &ValidationContext,
    ) -> Result<BatchPayload, PayloadBuilderError>;

    /// Checks whether the provided `payload` is valid given `past_payloads` and
    /// `context`.
    ///
    /// `past_payloads` contains the `Payloads` from all blocks above the
    /// certified height provided in `context`, in descending block height
    /// order.
    fn validate_payload(
        &self,
        payload: &Payload,
        past_payloads: &[(Height, Time, Payload)],
        context: &ValidationContext,
    ) -> ValidationResult<PayloadValidationError>;
}

/// Cache of sets of message ids for past payloads. The index used here is a
/// tuple (Height, HashOfBatchPayload) for two reasons:
/// 1. We want to purge this cache by height, for those below certified height.
/// 2. There could be more than one payloads at a given height due to blockchain
/// branching.
type IngressPayloadCache =
    BTreeMap<(Height, CryptoHashOf<BlockPayload>), Arc<HashSet<IngressMessageId>>>;

/// A list of hashsets that implements IngressSetQuery.
struct IngressSets {
    hash_sets: Vec<Arc<HashSet<IngressMessageId>>>,
    min_block_time: Time,
}

impl IngressSets {
    fn new(hash_sets: Vec<Arc<HashSet<IngressMessageId>>>, min_block_time: Time) -> Self {
        IngressSets {
            hash_sets,
            min_block_time,
        }
    }
}

impl IngressSetQuery for IngressSets {
    fn contains(&self, msg_id: &IngressMessageId) -> bool {
        self.hash_sets.iter().any(|set| set.contains(msg_id))
    }

    fn get_expiry_lower_bound(&self) -> Time {
        self.min_block_time
    }
}

/// Implementation of PayloadBuilder.
pub struct PayloadBuilderImpl {
    subnet_id: SubnetId,
    registry_client: Arc<dyn RegistryClient>,
    ingress_selector: Arc<dyn IngressSelector>,
    xnet_payload_builder: Arc<dyn XNetPayloadBuilder>,
    self_validating_payload_builder: Arc<dyn SelfValidatingPayloadBuilder>,
    metrics: PayloadBuilderMetrics,
    ingress_payload_cache: RwLock<IngressPayloadCache>,
    logger: ReplicaLogger,
}

impl PayloadBuilderImpl {
    /// Helper to create PayloadBuilder
    pub fn new(
        subnet_id: SubnetId,
        registry_client: Arc<dyn RegistryClient>,
        ingress_selector: Arc<dyn IngressSelector>,
        xnet_payload_builder: Arc<dyn XNetPayloadBuilder>,
        self_validating_payload_builder: Arc<dyn SelfValidatingPayloadBuilder>,
        metrics: MetricsRegistry,
        logger: ReplicaLogger,
    ) -> Self {
        Self {
            subnet_id,
            registry_client,
            ingress_selector,
            xnet_payload_builder,
            self_validating_payload_builder,
            metrics: PayloadBuilderMetrics::new(metrics),
            ingress_payload_cache: RwLock::new(BTreeMap::new()),
            logger,
        }
    }
}

impl PayloadBuilder for PayloadBuilderImpl {
    fn get_payload(
        &self,
        height: Height,
        ingress_pool: &dyn IngressPoolSelect,
        past_payloads: &[(Height, Time, Payload)],
        context: &ValidationContext,
    ) -> Result<BatchPayload, PayloadBuilderError> {
        let _timer = self.metrics.get_payload_duration.start_timer();
        self.metrics
            .past_payloads_length
            .observe(past_payloads.len() as f64);

        let mut ingress_payload_cache = self.ingress_payload_cache.write().unwrap();
        self.metrics
            .ingress_payload_cache_size
            .set(ingress_payload_cache.len() as i64);

        let min_block_time = match past_payloads.last() {
            None => context.time,
            Some((_, time, _)) => *time,
        };
        let (past_ingress, past_xnet, past_self_validating) =
            split_past_payloads(&mut ingress_payload_cache, past_payloads);
        self.metrics
            .past_payloads_length
            .observe(past_payloads.len() as f64);

        let ingress_query = IngressSets::new(past_ingress, min_block_time);

        // We enforce the block_payload limit in the following way:
        // On a block with even height, we fill up the block with xnet messages.
        // If there is space left, we fill it is ingress messages.
        // On odd blocks, we prioritize ingress over xnet.
        let max_block_payload_size = self.get_max_block_payload_size_bytes(context)?;
        let get_ingress_payload = |byte_limit| {
            self.ingress_selector.get_ingress_payload(
                ingress_pool,
                &ingress_query,
                context,
                byte_limit,
            )
        };
        let get_xnet_payload = |byte_limit| {
            self.xnet_payload_builder
                .get_xnet_payload(context, &past_xnet, byte_limit)
        };

        let (ingress, xnet) = if height.get() % 2 == 0 {
            let xnet = get_xnet_payload(max_block_payload_size);
            let ingress = get_ingress_payload(NumBytes::new(
                max_block_payload_size
                    .get()
                    .saturating_sub(xnet.count_bytes() as u64),
            ));
            (ingress, xnet)
        } else {
            let ingress = get_ingress_payload(max_block_payload_size);
            let xnet = get_xnet_payload(NumBytes::new(
                max_block_payload_size
                    .get()
                    .saturating_sub(ingress.count_bytes() as u64),
            ));
            (ingress, xnet)
        };

        let self_validating = self
            .self_validating_payload_builder
            .get_self_validating_payload(context, &past_self_validating, MAX_XNET_PAYLOAD_IN_BYTES);

        Ok(BatchPayload {
            ingress,
            xnet,
            self_validating,
        })
    }

    fn validate_payload(
        &self,
        payload: &Payload,
        past_payloads: &[(Height, Time, Payload)],
        context: &ValidationContext,
    ) -> ValidationResult<PayloadValidationError> {
        let _timer = self.metrics.validate_payload_duration.start_timer();
        if payload.is_summary() {
            return Ok(());
        }
        let batch_payload = &payload.as_ref().as_data().batch;
        let mut ingress_payload_cache = self.ingress_payload_cache.write().unwrap();
        let min_block_time = match past_payloads.last() {
            None => context.time,
            Some((_, time, _)) => *time,
        };
        let (past_ingress, past_xnet, past_self_validating) =
            split_past_payloads(&mut ingress_payload_cache, past_payloads);
        self.metrics
            .ingress_payload_cache_size
            .set(ingress_payload_cache.len() as i64);

        let ingress_query = IngressSets::new(past_ingress, min_block_time);
        let max_block_payload_size = self
            .get_max_block_payload_size_bytes(context)
            .map_err(|_| ValidationError::Transient(PayloadTransientError::RegistryUnavailable))?;

        // If ingress valiation is not valid, return it early.
        self.ingress_selector.validate_ingress_payload(
            &batch_payload.ingress,
            &ingress_query,
            context,
        )?;

        let xnet_size = self.xnet_payload_builder.validate_xnet_payload(
            &batch_payload.xnet,
            context,
            &past_xnet,
        )?;

        // The size of both payloads together must not exceed the block payload size.
        // NOTE: We MUST NOT use xnet.count_bytes() here, as it may not be
        // deterministic and could lead to divergence.
        if xnet_size + NumBytes::from(batch_payload.ingress.count_bytes() as u64)
            > max_block_payload_size
        {
            return Err(ValidationError::Permanent(
                PayloadPermanentError::PayloadTooBig {
                    expected: max_block_payload_size,
                    received: xnet_size
                        + NumBytes::from(batch_payload.ingress.count_bytes() as u64),
                },
            ));
        }
        self.self_validating_payload_builder
            .validate_self_validating_payload(
                &batch_payload.self_validating,
                context,
                &past_self_validating,
            )?;

        Ok(())
    }
}

impl PayloadBuilderImpl {
    /// Returns the valid maximum block payload length from the registry and
    /// checks the invariants. Emits a warning in case the invariants are not
    /// met.
    fn get_max_block_payload_size_bytes(
        &self,
        context: &ValidationContext,
    ) -> Result<NumBytes, PayloadBuilderError> {
        // Retrieve value from subnet
        let subnet_record = match self
            .registry_client
            .get_subnet_record(self.subnet_id, context.registry_version)
        {
            Err(_) | Ok(None) => {
                warn!(self.logger, "Failed to get subnet record in block_maker");
                return Err(PayloadBuilderError::RegistryUnavailable);
            }
            Ok(Some(subnet_record)) => subnet_record,
        };
        let required_min_size = MAX_XNET_PAYLOAD_IN_BYTES
            .get()
            .max(subnet_record.max_ingress_bytes_per_message);

        let mut max_block_payload_size = subnet_record.max_block_payload_size;
        // In any case, ensure the value is bigger than inter canister payload and
        // message size
        if max_block_payload_size < required_min_size {
            warn!(every_n_seconds => 300, self.logger,
                "max_block_payload_size too small. current value: {}, required minimum: {}! \
                max_block_payload_size must be larger than max_ingress_bytes_per_message \
                and MAX_XNET_PAYLOAD_IN_BYTES. Update registry!",
                max_block_payload_size, required_min_size);
            max_block_payload_size = required_min_size;
        }

        Ok(NumBytes::new(max_block_payload_size))
    }
}

/// Split past_payloads into past_ingress and past_xnet payloads. The
/// past_ingress is actually a list of HashSet of MessageIds taken from the
/// ingress_payload_cache.
#[allow(clippy::type_complexity)]
fn split_past_payloads<'a, 'b>(
    ingress_payload_cache: &'a mut IngressPayloadCache,
    past_payloads: &'b [(Height, Time, Payload)],
) -> (
    Vec<Arc<HashSet<IngressMessageId>>>,
    Vec<&'b XNetPayload>,
    Vec<&'b SelfValidatingPayload>,
) {
    let past_xnet: Vec<_> = past_payloads
        .iter()
        .filter_map(|(_, _, payload)| {
            if payload.is_summary() {
                None
            } else {
                Some(&payload.as_ref().as_data().batch.xnet)
            }
        })
        .collect();
    let past_ingress: Vec<_> = past_payloads
        .iter()
        .filter_map(|(height, _, payload)| {
            if payload.is_summary() {
                None
            } else {
                let payload_hash = payload.get_hash();
                let batch = &payload.as_ref().as_data().batch;
                let ingress = ingress_payload_cache
                    .entry((*height, payload_hash.clone()))
                    .or_insert_with(|| Arc::new(batch.ingress.message_ids().into_iter().collect()));
                Some(ingress.clone())
            }
        })
        .collect();
    let past_self_validating: Vec<_> = past_payloads
        .iter()
        .filter_map(|(_, _, payload)| {
            if payload.is_summary() {
                None
            } else {
                Some(&payload.as_ref().as_data().batch.self_validating)
            }
        })
        .collect();
    // We assume that 'past_payloads' comes in descending heights, following the
    // block parent traversal order.
    if let Some((min_height, _, _)) = past_payloads.last() {
        // The step below is to garbage collect no longer used past ingress payload
        // cache. It assumes the sequence of calls to payload selection/validation
        // leads to a monotonic sequence of lower-bound (min_height).
        //
        // Usually this is true, but even when it is not true (e.g. in tests) it is
        // always safe to remove entries from ingress_payload_cache at the expense
        // of having to re-compute them.
        let keys: Vec<_> = ingress_payload_cache.keys().cloned().collect();
        for key in keys {
            if key.0 < *min_height {
                ingress_payload_cache.remove(&key);
            }
        }
    }
    (past_ingress, past_xnet, past_self_validating)
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::consensus::mocks::{dependencies, dependencies_with_subnet_params, Dependencies};
    use ic_interfaces::self_validating_payload::NoOpSelfValidatingPayloadBuilder;
    use ic_logger::replica_logger::no_op_logger;
    use ic_test_artifact_pool::ingress_pool::TestIngressPool;
    use ic_test_utilities::{
        consensus::fake::Fake,
        ingress_selector::FakeIngressSelector,
        mock_time,
        registry::SubnetRecordBuilder,
        types::ids::{node_test_id, subnet_test_id},
        types::messages::SignedIngressBuilder,
        xnet_payload_builder::FakeXNetPayloadBuilder,
    };
    use ic_types::{
        consensus::{
            certification::{Certification, CertificationContent},
            dkg::Dealings,
            DataPayload, Payload, ThresholdSignature,
        },
        crypto::{CryptoHash, Signed},
        messages::SignedIngress,
        xnet::CertifiedStreamSlice,
        CryptoHashOfPartialState, RegistryVersion,
    };
    use std::collections::BTreeMap;
    /// Builds a `PayloadBuilderImpl` wrapping fake ingress and XNet payload
    /// builders that return the supplied ingress and XNet data.
    fn make_test_payload_impl(
        registry: Arc<dyn RegistryClient>,
        mut ingress_messages: Vec<Vec<SignedIngress>>,
        mut certified_streams: Vec<BTreeMap<SubnetId, CertifiedStreamSlice>>,
    ) -> PayloadBuilderImpl {
        let ingress_selector = FakeIngressSelector::new();
        ingress_messages
            .drain(..)
            .for_each(|im| ingress_selector.enqueue(im));
        let xnet_payload_builder =
            FakeXNetPayloadBuilder::make(certified_streams.drain(..).collect());
        let self_validating_payload_builder = NoOpSelfValidatingPayloadBuilder {};

        PayloadBuilderImpl::new(
            subnet_test_id(0),
            registry,
            Arc::new(ingress_selector),
            Arc::new(xnet_payload_builder),
            Arc::new(self_validating_payload_builder),
            MetricsRegistry::new(),
            no_op_logger(),
        )
    }

    /// Builds a `CertifiedStreamSlice` from the supplied `payload` and
    /// `merkle_proof` bytes, without a valid certification.
    fn make_certified_stream_slice(
        height: u64,
        payload: Vec<u8>,
        merkle_proof: Vec<u8>,
    ) -> CertifiedStreamSlice {
        CertifiedStreamSlice {
            payload,
            merkle_proof,
            certification: Certification {
                height: Height::from(height),
                signed: Signed {
                    signature: ThresholdSignature::fake(),
                    content: CertificationContent::new(CryptoHashOfPartialState::from(CryptoHash(
                        vec![],
                    ))),
                },
            },
        }
    }

    /// Wraps a `BatchPayload` into the full `Payload` structure.
    fn batch_payload_to_payload(height: u64, payload: BatchPayload) -> Payload {
        Payload::new(
            ic_crypto::crypto_hash,
            BlockPayload::Data(DataPayload {
                batch: payload,
                dealings: Dealings::new_empty(Height::from(height)),
                ecdsa: None,
            }),
        )
    }

    // Test that confirms that the output of messaging.get_messages aligns with the
    // messages acquired from the application layer.
    fn test_get_messages(
        provided_ingress_messages: Vec<SignedIngress>,
        provided_certified_streams: BTreeMap<SubnetId, CertifiedStreamSlice>,
    ) {
        ic_test_utilities::artifact_pool_config::with_test_pool_config(|pool_config| {
            let Dependencies { registry, .. } = dependencies(pool_config.clone(), 1);
            let payload_builder = make_test_payload_impl(
                registry,
                vec![provided_ingress_messages.clone()],
                vec![provided_certified_streams.clone()],
            );
            let ingress_pool = TestIngressPool::new(pool_config);

            let prev_payloads = Vec::new();
            let context = ValidationContext {
                certified_height: Height::from(0),
                registry_version: RegistryVersion::from(1),
                time: mock_time(),
            };

            let (ingress_msgs, stream_msgs) = payload_builder
                .get_payload(Height::from(1), &ingress_pool, &prev_payloads, &context)
                .unwrap()
                .into_messages()
                .unwrap();

            assert_eq!(ingress_msgs.len(), provided_ingress_messages.len());
            provided_ingress_messages
                .into_iter()
                .zip(ingress_msgs.into_iter())
                .for_each(|(a, b)| assert_eq!(a, b));

            assert_eq!(stream_msgs.len(), provided_certified_streams.len());
            provided_certified_streams
                .iter()
                .zip(stream_msgs.iter())
                .for_each(|(a, b)| assert_eq!(a, b));
        })
    }

    // Engine for changing the number of Ingress and RequestOrResponse messages
    // provided by the application.
    fn param_msgs_test(in_count: u64, stream_count: u64) {
        let ingress = |i| SignedIngressBuilder::new().nonce(i).build();
        let inputs = (0..in_count).map(ingress).collect();
        let certified_streams = (0..stream_count)
            .map(|x| {
                (
                    subnet_test_id(x),
                    make_certified_stream_slice(1, vec![], vec![]),
                )
            })
            .collect();

        test_get_messages(inputs, certified_streams)
    }

    #[test]
    fn test_get_messages_interface() {
        for i in 0..3 {
            for j in 0..3 {
                param_msgs_test(i, j);
            }
        }
    }

    /// This test executes the `get_payload` and `validate_payload` functions
    /// in `PayloadBuilderImpl`.
    /// It first builds and validated a `BatchPayload` that consists of 3/4
    /// `XNetPayload` and 1/4 `IngressPayload`.
    /// Then, it builds and validates a `BatchPayload` that consists of 3/4
    /// `XNetPayload` and 1/4 `IngressPayload`.
    /// In the last step, the mocks are setup to return 3/4 `XNetPayload` and
    /// 3/4 `IngressPayload`. This is too large and makes the `get_payload`
    /// function fail.
    #[test]
    fn test_payload_size_validation() {
        const MAX_SIZE: u64 = 2 * 1024 * 1024;
        // NOTE: Since the messages will also contain headers, the payload needs to be a
        // little bit smaller than the overall size
        const ONE_QUARTER: usize = 512 * 1024 - 1000;
        const THREE_QUARTER: usize = 3 * 512 * 1024 - 1000;

        ic_test_utilities::artifact_pool_config::with_test_pool_config(|pool_config| {
            let mut subnet_record = SubnetRecordBuilder::from(&[node_test_id(0)]).build();
            // NOTE: We can't set smaller values
            subnet_record.max_block_payload_size = MAX_SIZE;
            subnet_record.max_ingress_bytes_per_message = MAX_SIZE;
            let Dependencies { registry, .. } = dependencies_with_subnet_params(
                pool_config.clone(),
                subnet_test_id(0),
                vec![(1, subnet_record)],
            );
            let ingress_pool = TestIngressPool::new(pool_config);
            let context = ValidationContext {
                certified_height: Height::from(0),
                registry_version: RegistryVersion::from(1),
                time: mock_time(),
            };

            // Prepare the messages in the mock
            let make_ingress = |nonce, size| {
                vec![SignedIngressBuilder::new()
                    .method_payload(vec![0; size])
                    .nonce(nonce)
                    .build()]
            };
            let make_slice = |height, size| {
                let mut map = BTreeMap::new();
                map.insert(
                    subnet_test_id(1),
                    make_certified_stream_slice(height, vec![0; size], vec![]),
                );
                map
            };
            let certified_streams: Vec<BTreeMap<SubnetId, CertifiedStreamSlice>> = vec![
                make_slice(0, THREE_QUARTER),
                make_slice(1, ONE_QUARTER),
                make_slice(2, THREE_QUARTER),
            ];
            let ingress = vec![
                make_ingress(0, ONE_QUARTER),
                make_ingress(1, THREE_QUARTER),
                make_ingress(2, THREE_QUARTER),
            ];
            let payload_builder = make_test_payload_impl(registry, ingress, certified_streams);

            // Build first payload and then validate it
            let payload0 = payload_builder
                .get_payload(Height::from(0), &ingress_pool, &[], &context)
                .unwrap();
            let wrapped_payload0 = batch_payload_to_payload(0, payload0);
            payload_builder
                .validate_payload(&wrapped_payload0, &[], &context)
                .unwrap();

            // Build second payload and validate it
            let past_payload0 = [(Height::from(0), mock_time(), wrapped_payload0)];
            let payload1 = payload_builder
                .get_payload(Height::from(1), &ingress_pool, &past_payload0, &context)
                .unwrap();
            let wrapped_payload1 = batch_payload_to_payload(0, payload1);
            payload_builder
                .validate_payload(&wrapped_payload1, &past_payload0, &context)
                .unwrap();

            // Build third payload and validate it
            // This payload is oversized, therefore we expect the validator to fail
            let past_payload1 = [(Height::from(1), mock_time(), wrapped_payload1)];
            let payload2 = payload_builder
                .get_payload(Height::from(2), &ingress_pool, &past_payload1, &context)
                .unwrap();

            let pb_result = payload_builder.validate_payload(
                &batch_payload_to_payload(1, payload2),
                &past_payload1,
                &context,
            );

            match pb_result {
                Err(
                    ValidationError::<PayloadPermanentError, PayloadTransientError>::Permanent(
                        PayloadPermanentError::PayloadTooBig { .. },
                    ),
                ) => (),
                _ => panic!("Expected PayloadTooBig error"),
            }
        });
    }
}
