//! The pre signature process manager

use crate::consensus::{
    metrics::{timed_call, EcdsaPreSignerMetrics},
    utils::RoundRobin,
    ConsensusCrypto,
};

use ic_interfaces::consensus_pool::ConsensusPoolCache;
use ic_interfaces::crypto::{ErrorReplication, IDkgProtocol};
use ic_interfaces::ecdsa::{EcdsaChangeAction, EcdsaChangeSet, EcdsaPool};
use ic_logger::{debug, warn, ReplicaLogger};
use ic_metrics::MetricsRegistry;
use ic_types::artifact::EcdsaMessageId;
use ic_types::consensus::ecdsa::{
    EcdsaBlockReader, EcdsaBlockReaderImpl, EcdsaDealing, EcdsaDealingSupport, EcdsaMessage,
    EcdsaVerifiedDealing,
};
use ic_types::consensus::MultiSignature;
use ic_types::crypto::canister_threshold_sig::idkg::{
    IDkgMultiSignedDealing, IDkgTranscript, IDkgTranscriptId, IDkgTranscriptOperation,
    IDkgTranscriptParams,
};
use ic_types::{Height, NodeId};

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{self, Debug, Formatter};
use std::sync::Arc;

pub(crate) trait EcdsaPreSigner: Send {
    /// The on_state_change() called from the main ECDSA path.
    fn on_state_change(&self, ecdsa_pool: &dyn EcdsaPool) -> EcdsaChangeSet;
}

pub(crate) struct EcdsaPreSignerImpl {
    node_id: NodeId,
    consensus_cache: Arc<dyn ConsensusPoolCache>,
    crypto: Arc<dyn ConsensusCrypto>,
    schedule: RoundRobin,
    metrics: EcdsaPreSignerMetrics,
    log: ReplicaLogger,
}

impl EcdsaPreSignerImpl {
    pub(crate) fn new(
        node_id: NodeId,
        consensus_cache: Arc<dyn ConsensusPoolCache>,
        crypto: Arc<dyn ConsensusCrypto>,
        metrics_registry: MetricsRegistry,
        log: ReplicaLogger,
    ) -> Self {
        Self {
            node_id,
            consensus_cache,
            crypto,
            schedule: RoundRobin::default(),
            metrics: EcdsaPreSignerMetrics::new(metrics_registry),
            log,
        }
    }

    /// Starts the transcript generation sequence by issuing the
    /// dealing for the transcript. The requests for new transcripts
    /// come from the latest summary block
    fn send_dealings(
        &self,
        ecdsa_pool: &dyn EcdsaPool,
        block_reader: &dyn EcdsaBlockReader,
    ) -> EcdsaChangeSet {
        block_reader
            .requested_transcripts()
            .filter(|transcript_params| {
                // Issue a dealing if we are in the dealer list and we haven't
                //already issued a dealing for this transcript
                transcript_params.dealers().position(self.node_id).is_some()
                    && !self.has_dealer_issued_dealing(
                        ecdsa_pool,
                        &transcript_params.transcript_id(),
                        &self.node_id,
                    )
            })
            .map(|transcript_params| self.crypto_create_dealing(block_reader, transcript_params))
            .flatten()
            .collect()
    }

    /// Processes the dealings received from peer dealers
    fn validate_dealings(
        &self,
        ecdsa_pool: &dyn EcdsaPool,
        block_reader: &dyn EcdsaBlockReader,
    ) -> EcdsaChangeSet {
        // Pass 1: collection of <TranscriptId, DealerId>
        let mut dealing_keys = BTreeSet::new();
        let mut duplicate_keys = BTreeSet::new();
        for (_, dealing) in ecdsa_pool.unvalidated().dealings() {
            let key = (dealing.transcript_id, dealing.dealer_id);
            if !dealing_keys.insert(key) {
                duplicate_keys.insert(key);
            }
        }

        let mut ret = Vec::new();
        for (id, dealing) in ecdsa_pool.unvalidated().dealings() {
            // Remove the duplicate entries
            let key = (dealing.transcript_id, dealing.dealer_id);
            if duplicate_keys.contains(&key) {
                self.metrics
                    .pre_sign_errors_inc("duplicate_dealing_in_batch");
                ret.push(EcdsaChangeAction::HandleInvalid(
                    id,
                    format!(
                        "Duplicate dealing in unvalidated batch: dealer = {:?}, height = {:?},
                          transcript_id = {:?}",
                        dealing.dealer_id, dealing.requested_height, dealing.transcript_id
                    ),
                ));
                continue;
            }

            match Action::action(
                block_reader,
                dealing.requested_height,
                &dealing.transcript_id,
            ) {
                Action::Process(transcript_params) => {
                    if transcript_params
                        .dealers()
                        .position(dealing.dealer_id)
                        .is_none()
                    {
                        // The node is not in the dealer list for this transcript
                        self.metrics.pre_sign_errors_inc("unexpected_dealing");
                        ret.push(EcdsaChangeAction::HandleInvalid(
                            id,
                            format!(
                                "Dealing from unexpected node: dealer = {:?}, height = {:?},
                                  transcript_id = {:?}",
                                dealing.dealer_id, dealing.requested_height, dealing.transcript_id
                            ),
                        ))
                    } else if self.has_dealer_issued_dealing(
                        ecdsa_pool,
                        &dealing.transcript_id,
                        &dealing.dealer_id,
                    ) {
                        // The node already sent a valid dealing for this transcript
                        self.metrics.pre_sign_errors_inc("duplicate_dealing");
                        ret.push(EcdsaChangeAction::HandleInvalid(
                            id,
                            format!(
                                "Duplicate dealing: dealer = {:?}, height = {:?},
                                  transcript_id = {:?}",
                                dealing.dealer_id, dealing.requested_height, dealing.transcript_id
                            ),
                        ))
                    } else {
                        let mut changes =
                            self.crypto_verify_dealing(&id, transcript_params, dealing);
                        ret.append(&mut changes);
                    }
                }
                Action::Drop => ret.push(EcdsaChangeAction::RemoveUnvalidated(id)),
                Action::Defer => {}
            }
        }
        ret
    }

    /// Sends out the signature share for the dealings received from peer
    /// dealers
    fn send_dealing_support(
        &self,
        ecdsa_pool: &dyn EcdsaPool,
        block_reader: &dyn EcdsaBlockReader,
    ) -> EcdsaChangeSet {
        // TranscriptId -> TranscriptParams
        let mut trancript_param_map = BTreeMap::new();
        for transcript_params in block_reader.requested_transcripts() {
            trancript_param_map.insert(transcript_params.transcript_id(), transcript_params);
        }

        ecdsa_pool
            .validated()
            .dealings()
            .filter(|(_, dealing)| {
                !self.has_node_issued_dealing_support(
                    ecdsa_pool,
                    &dealing.transcript_id,
                    &dealing.dealer_id,
                    &self.node_id,
                )
            })
            .filter_map(|(id, dealing)| {
                // Look up the transcript params for the dealing, and check if we
                // are a receiver for this dealing
                if let Some(transcript_params) = trancript_param_map.get(&dealing.transcript_id) {
                    transcript_params
                        .receivers()
                        .position(self.node_id)
                        .map(|_| (id, transcript_params, dealing))
                } else {
                    self.metrics
                        .pre_sign_errors_inc("create_support_missing_transcript_params");
                    warn!(
                        self.log,
                        "Dealing support creation: transcript_param not found: dealer = {:?},
                          height = {:?}, transcript_id = {:?}",
                        dealing.dealer_id,
                        dealing.requested_height,
                        dealing.transcript_id,
                    );
                    None
                }
            })
            .map(|(id, transcript_params, dealing)| {
                self.crypto_create_dealing_support(&id, transcript_params, dealing)
            })
            .flatten()
            .collect()
    }

    /// Processes the received dealing support messages
    fn validate_dealing_support(
        &self,
        ecdsa_pool: &dyn EcdsaPool,
        block_reader: &dyn EcdsaBlockReader,
    ) -> EcdsaChangeSet {
        // Get the set of valid dealings <TranscriptId, DealerId>
        let mut valid_dealings = BTreeSet::new();
        for (_, dealing) in ecdsa_pool.validated().dealings() {
            let dealing_key = (dealing.transcript_id, dealing.dealer_id);
            valid_dealings.insert(dealing_key);
        }

        // Pass 1: collection of <TranscriptId, DealerId, SignerId>
        let mut supports = BTreeSet::new();
        let mut duplicate_supports = BTreeSet::new();
        for (_, support) in ecdsa_pool.unvalidated().dealing_support() {
            let dealing = &support.content;
            let support_key = (
                dealing.transcript_id,
                dealing.dealer_id,
                support.signature.signer,
            );
            if !supports.insert(support_key) {
                duplicate_supports.insert(support_key);
            }
        }

        let mut ret = Vec::new();
        for (id, support) in ecdsa_pool.unvalidated().dealing_support() {
            let dealing = &support.content;
            let dealing_key = (dealing.transcript_id, dealing.dealer_id);
            let support_key = (
                dealing.transcript_id,
                dealing.dealer_id,
                support.signature.signer,
            );

            // Remove the duplicate entries
            if duplicate_supports.contains(&support_key) {
                self.metrics
                    .pre_sign_errors_inc("duplicate_support_in_batch");
                ret.push(EcdsaChangeAction::HandleInvalid(
                    id,
                    format!(
                        "Duplicate support in unvalidated batch: dealer = {:?}, height = {:?},
                          transcript_id = {:?}, signer = {:?}",
                        dealing.dealer_id,
                        dealing.requested_height,
                        dealing.transcript_id,
                        support.signature.signer,
                    ),
                ));
                continue;
            }

            match Action::action(
                block_reader,
                dealing.requested_height,
                &dealing.transcript_id,
            ) {
                Action::Process(transcript_params) => {
                    if transcript_params
                        .receivers()
                        .position(support.signature.signer)
                        .is_none()
                    {
                        // The node is not in the receiver list for this transcript,
                        // support share is not expected from it
                        self.metrics.pre_sign_errors_inc("unexpected_support");
                        ret.push(EcdsaChangeAction::HandleInvalid(
                            id,
                            format!(
                                "Support from unexpected node: dealer = {:?}, height = {:?},
                                  transcript_id = {:?}, signer = support.signature.signer",
                                dealing.dealer_id, dealing.requested_height, dealing.transcript_id
                            ),
                        ))
                    } else if !valid_dealings.contains(&dealing_key) {
                        // Support for a dealing we don't have yet, defer it
                        continue;
                    } else if self.has_node_issued_dealing_support(
                        ecdsa_pool,
                        &dealing.transcript_id,
                        &dealing.dealer_id,
                        &support.signature.signer,
                    ) {
                        // The node already sent a valid support for this dealing
                        self.metrics.pre_sign_errors_inc("duplicate_support");
                        ret.push(EcdsaChangeAction::HandleInvalid(
                            id,
                            format!(
                                "Duplicate support: dealer = {:?}, height = {:?},
                                  transcript_id = {:?}, signer = {:?}",
                                dealing.dealer_id,
                                dealing.requested_height,
                                dealing.transcript_id,
                                support.signature.signer
                            ),
                        ))
                    } else {
                        let mut changes =
                            self.crypto_verify_dealing_support(&id, transcript_params, support);
                        ret.append(&mut changes);
                    }
                }
                Action::Drop => ret.push(EcdsaChangeAction::RemoveUnvalidated(id)),
                Action::Defer => {}
            }
        }

        ret
    }

    /// Purges the entries no longer needed from the artifact pool
    fn purge_artifacts(
        &self,
        ecdsa_pool: &dyn EcdsaPool,
        block_reader: &dyn EcdsaBlockReader,
    ) -> EcdsaChangeSet {
        let mut in_progress = BTreeSet::new();
        for transcript_params in block_reader.requested_transcripts() {
            in_progress.insert(transcript_params.transcript_id());
        }

        let mut ret = Vec::new();
        let current_height = block_reader.height();

        // Unvalidated dealings.
        let mut action = ecdsa_pool
            .unvalidated()
            .dealings()
            .filter(|(_, dealing)| self.should_purge(dealing, current_height, &in_progress))
            .map(|(id, _)| EcdsaChangeAction::RemoveUnvalidated(id))
            .collect();
        ret.append(&mut action);

        // Validated dealings.
        let mut action = ecdsa_pool
            .validated()
            .dealings()
            .filter(|(_, dealing)| self.should_purge(dealing, current_height, &in_progress))
            .map(|(id, _)| EcdsaChangeAction::RemoveValidated(id))
            .collect();
        ret.append(&mut action);

        // Unvalidated dealing support.
        let mut action = ecdsa_pool
            .unvalidated()
            .dealing_support()
            .filter(|(_, support)| {
                self.should_purge(&support.content, current_height, &in_progress)
            })
            .map(|(id, _)| EcdsaChangeAction::RemoveUnvalidated(id))
            .collect();
        ret.append(&mut action);

        // Validated dealing support.
        let mut action = ecdsa_pool
            .validated()
            .dealing_support()
            .filter(|(_, support)| {
                self.should_purge(&support.content, current_height, &in_progress)
            })
            .map(|(id, _)| EcdsaChangeAction::RemoveValidated(id))
            .collect();
        ret.append(&mut action);

        ret
    }

    /// Helper to create dealing
    fn crypto_create_dealing(
        &self,
        block_reader: &dyn EcdsaBlockReader,
        transcript_params: &IDkgTranscriptParams,
    ) -> EcdsaChangeSet {
        if !self.crypto_load_dependencies(transcript_params) {
            return Default::default();
        }

        IDkgProtocol::create_dealing(&*self.crypto, transcript_params).map_or_else(
            |error| {
                // TODO: currently, transcript creation will be retried the next time, which
                // will most likely fail again. This should be signaled up so that the bad
                // transcript params can be acted on
                warn!(
                    self.log,
                    "Failed to create dealing: transcript_id = {:?}, type = {:?}, error = {:?}",
                    transcript_params.transcript_id(),
                    transcript_params.operation_type(),
                    error
                );
                self.metrics.pre_sign_errors_inc("create_dealing");
                Default::default()
            },
            |dealing| {
                let dealing = EcdsaDealing {
                    requested_height: block_reader.height(),
                    transcript_id: transcript_params.transcript_id(),
                    dealer_id: self.node_id,
                    dealing,
                };
                self.metrics.pre_sign_metrics_inc("dealing_sent");
                vec![EcdsaChangeAction::AddToValidated(
                    EcdsaMessage::EcdsaDealing(dealing),
                )]
            },
        )
    }

    /// Helper to verify a dealing received for a transcript we are building
    fn crypto_verify_dealing(
        &self,
        id: &EcdsaMessageId,
        transcript_params: &IDkgTranscriptParams,
        dealing: &EcdsaDealing,
    ) -> EcdsaChangeSet {
        IDkgProtocol::verify_dealing_public(&*self.crypto, transcript_params, &dealing.dealing)
            .map_or_else(
                |error| {
                    if error.is_replicated() {
                        self.metrics.pre_sign_errors_inc("verify_dealing_permanent");
                        vec![EcdsaChangeAction::HandleInvalid(
                            id.clone(),
                            format!(
                                "Dealing validation(permanent error): dealer = {:?},
                              height = {:?}, transcript_id = {:?}, error = {:?}",
                                dealing.dealer_id,
                                dealing.requested_height,
                                dealing.transcript_id,
                                error
                            ),
                        )]
                    } else {
                        // Defer in case of transient errors
                        debug!(
                            self.log,
                            "Dealing validation(transient error): dealer = {:?},
                          height = {:?}, transcript_id = {:?}, error = {:?}",
                            dealing.dealer_id,
                            dealing.requested_height,
                            dealing.transcript_id,
                            error
                        );
                        self.metrics.pre_sign_errors_inc("verify_dealing_transient");
                        Default::default()
                    }
                },
                |()| {
                    self.metrics.pre_sign_metrics_inc("dealing_received");
                    vec![EcdsaChangeAction::MoveToValidated(id.clone())]
                },
            )
    }

    /// Helper to issue a support share for a dealing. Assumes we are a receiver
    /// for the dealing.
    fn crypto_create_dealing_support(
        &self,
        id: &EcdsaMessageId,
        transcript_params: &IDkgTranscriptParams,
        dealing: &EcdsaDealing,
    ) -> EcdsaChangeSet {
        if let Err(error) =
            IDkgProtocol::verify_dealing_private(&*self.crypto, transcript_params, &dealing.dealing)
        {
            if error.is_replicated() {
                self.metrics
                    .pre_sign_errors_inc("verify_dealing_private_permanent");
                return vec![EcdsaChangeAction::HandleInvalid(
                    id.clone(),
                    format!(
                        "Dealing private verification(permanent error): dealer = {:?},
                          height = {:?}, transcript_id = {:?}, error = {:?}",
                        dealing.dealer_id, dealing.requested_height, dealing.transcript_id, error
                    ),
                )];
            } else {
                self.metrics
                    .pre_sign_errors_inc("verify_dealing_private_transient");
                debug!(
                    self.log,
                    "Dealing private verification(transient error): dealer = {:?},
                          height = {:?}, transcript_id = {:?}, error = {:?}",
                    dealing.dealer_id,
                    dealing.requested_height,
                    dealing.transcript_id,
                    error
                );
                return Default::default();
            }
        }

        // Generate the multi sig share
        self.crypto
            .sign(dealing, self.node_id, transcript_params.registry_version())
            .map_or_else(
                |error| {
                    debug!(
                        self.log,
                        "Dealing multi sign failed: dealer = {:?},
                          height = {:?}, transcript_id = {:?}, error = {:?}",
                        dealing.dealer_id,
                        dealing.requested_height,
                        dealing.transcript_id,
                        error
                    );
                    self.metrics
                        .pre_sign_errors_inc("dealing_support_multi_sign");
                    Default::default()
                },
                |multi_sig_share| {
                    let dealing_support = EcdsaDealingSupport {
                        content: dealing.clone(),
                        signature: multi_sig_share,
                    };
                    self.metrics.pre_sign_metrics_inc("dealing_support_sent");
                    vec![EcdsaChangeAction::AddToValidated(
                        EcdsaMessage::EcdsaDealingSupport(dealing_support),
                    )]
                },
            )
    }

    /// Helper to verify a support share for a dealing
    fn crypto_verify_dealing_support(
        &self,
        id: &EcdsaMessageId,
        transcript_params: &IDkgTranscriptParams,
        support: &EcdsaDealingSupport,
    ) -> EcdsaChangeSet {
        let dealing = &support.content;
        self.crypto
            .verify(support, transcript_params.registry_version())
            .map_or_else(
                |error| {
                    self.metrics.pre_sign_errors_inc("verify_dealing_support");
                    vec![EcdsaChangeAction::HandleInvalid(
                        id.clone(),
                        format!(
                            "Support validation failed: dealer = {:?},
                          height = {:?}, transcript_id = {:?}, signer = {:?}, error = {:?}",
                            dealing.dealer_id,
                            dealing.requested_height,
                            dealing.transcript_id,
                            support.signature.signer,
                            error
                        ),
                    )]
                },
                |_| {
                    self.metrics
                        .pre_sign_metrics_inc("dealing_support_received");
                    vec![EcdsaChangeAction::MoveToValidated(id.clone())]
                },
            )
    }

    /// Helper to load the transcripts the given transcript is dependent on.
    /// Returns true if the dependencies were loaded successfully.
    fn crypto_load_dependencies(&self, transcript_params: &IDkgTranscriptParams) -> bool {
        match &transcript_params.operation_type() {
            IDkgTranscriptOperation::Random => true,
            IDkgTranscriptOperation::ReshareOfMasked(t) => self.crypto_load_transcript(t),
            IDkgTranscriptOperation::ReshareOfUnmasked(t) => self.crypto_load_transcript(t),
            IDkgTranscriptOperation::UnmaskedTimesMasked(t1, t2) => {
                self.crypto_load_transcript(t1) && self.crypto_load_transcript(t2)
            }
        }
    }

    /// Helper to load the given transcript.
    /// Returns true if the transcript was loaded successfully.
    fn crypto_load_transcript(&self, transcript: &IDkgTranscript) -> bool {
        if let Err(error) = IDkgProtocol::load_transcript(&*self.crypto, transcript) {
            warn!(
                self.log,
                "Failed to load transcript: transcript_id = {:?}, error = {:?}",
                transcript.transcript_id,
                error
            );
            self.metrics.pre_sign_errors_inc("load_transcript");
            false
        } else {
            self.metrics.pre_sign_metrics_inc("transcript_loaded");
            true
        }
    }

    /// Checks if the we have a valid dealing from the dealer for the given
    /// transcript
    fn has_dealer_issued_dealing(
        &self,
        ecdsa_pool: &dyn EcdsaPool,
        transcript_id: &IDkgTranscriptId,
        dealer_id: &NodeId,
    ) -> bool {
        ecdsa_pool.validated().dealings().any(|(_, dealing)| {
            dealing.dealer_id == *dealer_id && dealing.transcript_id == *transcript_id
        })
    }

    /// Checks if the we have a valid dealing support from the node for the
    /// given dealing
    fn has_node_issued_dealing_support(
        &self,
        ecdsa_pool: &dyn EcdsaPool,
        transcript_id: &IDkgTranscriptId,
        dealer_id: &NodeId,
        node_id: &NodeId,
    ) -> bool {
        ecdsa_pool
            .validated()
            .dealing_support()
            .any(|(_, support)| {
                support.content.dealer_id == *dealer_id
                    && support.content.transcript_id == *transcript_id
                    && support.signature.signer == *node_id
            })
    }

    /// Checks if the dealing should be purged
    fn should_purge(
        &self,
        dealing: &EcdsaDealing,
        current_height: Height,
        in_progress: &BTreeSet<IDkgTranscriptId>,
    ) -> bool {
        dealing.requested_height <= current_height && !in_progress.contains(&dealing.transcript_id)
    }
}

impl EcdsaPreSigner for EcdsaPreSignerImpl {
    fn on_state_change(&self, ecdsa_pool: &dyn EcdsaPool) -> EcdsaChangeSet {
        let block_reader = EcdsaBlockReaderImpl::new(self.consensus_cache.finalized_block());
        let metrics = self.metrics.clone();

        let send_dealings = || {
            timed_call(
                "send_dealings",
                || self.send_dealings(ecdsa_pool, &block_reader),
                &metrics.on_state_change_duration,
            )
        };
        let validate_dealings = || {
            timed_call(
                "validate_dealings",
                || self.validate_dealings(ecdsa_pool, &block_reader),
                &metrics.on_state_change_duration,
            )
        };
        let send_dealing_support = || {
            timed_call(
                "send_dealing_support",
                || self.send_dealing_support(ecdsa_pool, &block_reader),
                &metrics.on_state_change_duration,
            )
        };
        let validate_dealing_support = || {
            timed_call(
                "validate_dealing_support",
                || self.validate_dealing_support(ecdsa_pool, &block_reader),
                &metrics.on_state_change_duration,
            )
        };
        let purge_artifacts = || {
            timed_call(
                "purge_artifacts",
                || self.purge_artifacts(ecdsa_pool, &block_reader),
                &metrics.on_state_change_duration,
            )
        };

        let calls: [&'_ dyn Fn() -> EcdsaChangeSet; 5] = [
            &send_dealings,
            &validate_dealings,
            &send_dealing_support,
            &validate_dealing_support,
            &purge_artifacts,
        ];
        self.schedule.call_next(&calls)
    }
}

pub(crate) trait EcdsaTranscriptBuilder: Send {
    /// Returns the transcripts that can be successfully built from
    /// the current entries in the ECDSA pool
    fn get_completed_transcripts(&self, ecdsa_pool: &dyn EcdsaPool) -> Vec<IDkgTranscript>;
}

pub(crate) struct EcdsaTranscriptBuilderImpl<'a> {
    consensus_cache: &'a dyn ConsensusPoolCache,
    crypto: &'a dyn ConsensusCrypto,
    metrics: EcdsaPreSignerMetrics,
    log: ReplicaLogger,
}

impl<'a> EcdsaTranscriptBuilderImpl<'a> {
    pub(crate) fn new(
        consensus_cache: &'a dyn ConsensusPoolCache,
        crypto: &'a dyn ConsensusCrypto,
        metrics_registry: MetricsRegistry,
        log: ReplicaLogger,
    ) -> Self {
        Self {
            consensus_cache,
            crypto,
            metrics: EcdsaPreSignerMetrics::new(metrics_registry),
            log,
        }
    }

    /// Helper to combine the multi sig shares for a dealing
    fn crypto_aggregate_dealing_support(
        &self,
        transcript_params: &IDkgTranscriptParams,
        support_shares: &[&EcdsaDealingSupport],
    ) -> Option<MultiSignature<EcdsaDealing>> {
        // Check if we have enough shares for aggregation
        if support_shares.len() < (transcript_params.verification_threshold().get() as usize) {
            return None;
        }

        let mut signatures = Vec::new();
        for support_share in support_shares {
            signatures.push(&support_share.signature);
        }

        self.crypto
            .aggregate(signatures, transcript_params.registry_version())
            .map_or_else(
                |error| {
                    debug!(
                        self.log,
                        "Failed to aggregate: transcript_id = {:?}, error = {:?}",
                        transcript_params.transcript_id(),
                        error
                    );
                    self.metrics
                        .pre_sign_errors_inc("aggregate_dealing_support");
                    None
                },
                |multi_sig| {
                    self.metrics
                        .pre_sign_metrics_inc("dealing_support_aggregated");
                    Some(multi_sig)
                },
            )
    }

    /// Helper to create the transcript from the verified dealings
    fn crypto_create_transcript(
        &self,
        transcript_params: &IDkgTranscriptParams,
        verified_dealings: &BTreeMap<NodeId, IDkgMultiSignedDealing>,
    ) -> Option<IDkgTranscript> {
        // Check if we have enough dealings to create transcript
        if verified_dealings.len() < (transcript_params.collection_threshold().get() as usize) {
            return None;
        }

        IDkgProtocol::create_transcript(&*self.crypto, transcript_params, verified_dealings)
            .map_or_else(
                |error| {
                    warn!(
                        self.log,
                        "Failed to create transcript: transcript_id = {:?}, error = {:?}",
                        transcript_params.transcript_id(),
                        error
                    );
                    self.metrics.pre_sign_errors_inc("create_transcript");
                    None
                },
                |transcript| {
                    self.metrics.pre_sign_metrics_inc("transcript_created");
                    Some(transcript)
                },
            )
    }
}

impl<'a> EcdsaTranscriptBuilder for EcdsaTranscriptBuilderImpl<'a> {
    fn get_completed_transcripts(&self, ecdsa_pool: &dyn EcdsaPool) -> Vec<IDkgTranscript> {
        // TranscriptId -> TranscriptParams
        let block_reader = EcdsaBlockReaderImpl::new(self.consensus_cache.finalized_block());
        let mut trancript_state_map = BTreeMap::new();
        for transcript_params in block_reader.requested_transcripts() {
            trancript_state_map.insert(
                transcript_params.transcript_id(),
                TranscriptState::new(transcript_params),
            );
        }

        // Step 1: Build the verified dealings from the support shares
        for (_, dealing) in ecdsa_pool.validated().dealings() {
            let transcript_state = match trancript_state_map.get_mut(&dealing.transcript_id) {
                Some(state) => state,
                None => continue,
            };

            // Collect the shares for this dealing and aggregate the shares
            // TODO: do preprocessing to avoid repeated walking of the
            // support pool
            let support_shares: Vec<&EcdsaDealingSupport> = ecdsa_pool
                .validated()
                .dealing_support()
                .filter_map(|(_, support)| {
                    if support.content.transcript_id == dealing.transcript_id
                        && support.content.dealer_id == dealing.dealer_id
                    {
                        Some(support)
                    } else {
                        None
                    }
                })
                .collect();

            if let Some(multi_sig) = self.crypto_aggregate_dealing_support(
                transcript_state.transcript_params,
                &support_shares,
            ) {
                transcript_state.add_completed_dealing(dealing, multi_sig);
            }
        }

        // Step 2: Build the transcripts from the verified dealings
        let mut completed_transcripts = Vec::new();
        for transcript_state in trancript_state_map.values() {
            if let Some(transcript) = self.crypto_create_transcript(
                transcript_state.transcript_params,
                &transcript_state.completed_dealings,
            ) {
                completed_transcripts.push(transcript);
            }
        }
        completed_transcripts
    }
}

/// Specifies how to handle a received message
#[derive(Eq, PartialEq)]
enum Action<'a> {
    /// The message is relevant to our current state, process it
    /// immediately. The transcript params for this transcript
    /// (as specified by the finalized block) is the argument
    Process(&'a IDkgTranscriptParams),

    /// Keep it to be processed later (e.g) this is from a node
    /// ahead of us
    Defer,

    /// Don't need it
    Drop,
}

impl<'a> Action<'a> {
    /// Decides the action to take on a received message with the given
    /// height/transcriptId
    #[allow(clippy::self_named_constructors)]
    fn action(
        block_reader: &'a dyn EcdsaBlockReader,
        msg_height: Height,
        msg_transcript_id: &IDkgTranscriptId,
    ) -> Action<'a> {
        if msg_height > block_reader.height() {
            // Message is from a node ahead of us, keep it to be
            // processed later
            return Action::Defer;
        }

        for transcript_params in block_reader.requested_transcripts() {
            if *msg_transcript_id == transcript_params.transcript_id() {
                return Action::Process(transcript_params);
            }
        }

        // Its for a transcript that has not been requested, drop it
        Action::Drop
    }
}

/// Needed as IDKGTranscriptParams doesn't implement Debug
impl<'a> Debug for Action<'a> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match &self {
            Self::Process(transcript_params) => {
                write!(
                    f,
                    "Action::Process(): transcript_id = {:?}",
                    transcript_params.transcript_id()
                )
            }
            Self::Defer => write!(f, "Action::Defer"),
            Self::Drop => write!(f, "Action::Drop"),
        }
    }
}

/// Helper to hold the per-transcript state during the transcript
/// building process
struct TranscriptState<'a> {
    transcript_params: &'a IDkgTranscriptParams,
    completed_dealings: BTreeMap<NodeId, IDkgMultiSignedDealing>,
}

impl<'a> TranscriptState<'a> {
    fn new(transcript_params: &'a IDkgTranscriptParams) -> Self {
        Self {
            transcript_params,
            completed_dealings: BTreeMap::new(),
        }
    }

    // Adds a completed dealing to the transcript state. The dealing
    // is stored in the IDkgMultiSignedDealing format
    fn add_completed_dealing(
        &mut self,
        dealing: &'a EcdsaDealing,
        multi_sig: MultiSignature<EcdsaDealing>,
    ) {
        let verified_dealing = EcdsaVerifiedDealing {
            content: dealing.clone(),
            signature: multi_sig,
        };
        self.completed_dealings
            .insert(dealing.dealer_id, verified_dealing.into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::consensus::mocks::{dependencies, Dependencies};
    use ic_artifact_pool::ecdsa_pool::EcdsaPoolImpl;
    use ic_config::artifact_pool::ArtifactPoolConfig;
    use ic_ecdsa_object::EcdsaObject;
    use ic_interfaces::artifact_pool::UnvalidatedArtifact;
    use ic_interfaces::ecdsa::MutableEcdsaPool;
    use ic_interfaces::time_source::TimeSource;
    use ic_test_utilities::consensus::fake::*;
    use ic_test_utilities::crypto::{
        dummy_idkg_dealing_for_tests, dummy_idkg_transcript_id_for_tests,
    };
    use ic_test_utilities::types::ids::{NODE_1, NODE_2, NODE_3, NODE_4};
    use ic_test_utilities::with_test_replica_logger;
    use ic_test_utilities::FastForwardTimeSource;
    use ic_types::consensus::MultiSignatureShare;
    use ic_types::crypto::canister_threshold_sig::idkg::{
        IDkgDealers, IDkgReceivers, IDkgTranscriptId, IDkgTranscriptOperation, IDkgTranscriptParams,
    };
    use ic_types::crypto::AlgorithmId;
    use ic_types::{Height, RegistryVersion};
    use std::collections::BTreeSet;

    // Implementation of EcdsaBlockReader to inject the test transcript params
    struct TestEcdsaBlockReader {
        height: Height,
        requests: Vec<IDkgTranscriptParams>,
    }

    impl TestEcdsaBlockReader {
        fn new(height: Height, requests: Vec<IDkgTranscriptParams>) -> Self {
            Self { height, requests }
        }
    }

    impl EcdsaBlockReader for TestEcdsaBlockReader {
        fn height(&self) -> Height {
            self.height
        }

        fn requested_transcripts(&self) -> Box<dyn Iterator<Item = &IDkgTranscriptParams> + '_> {
            Box::new(self.requests.iter())
        }
    }

    fn create_dependencies(
        pool_config: ArtifactPoolConfig,
        logger: ReplicaLogger,
    ) -> (EcdsaPoolImpl, EcdsaPreSignerImpl) {
        let metrics_registry = MetricsRegistry::new();
        let Dependencies {
            pool,
            replica_config: _,
            membership: _,
            registry: _,
            crypto,
            ..
        } = dependencies(pool_config, 1);

        let pre_signer = EcdsaPreSignerImpl::new(
            NODE_1,
            pool.get_cache(),
            crypto,
            metrics_registry.clone(),
            logger.clone(),
        );
        let ecdsa_pool = EcdsaPoolImpl::new(logger, metrics_registry);

        (ecdsa_pool, pre_signer)
    }

    // Creates a test transcript param
    fn create_transcript_param(
        transcript_id: IDkgTranscriptId,
        dealer_list: &[NodeId],
        receiver_list: &[NodeId],
    ) -> IDkgTranscriptParams {
        let mut dealers = BTreeSet::new();
        dealer_list.iter().for_each(|val| {
            dealers.insert(*val);
        });
        let mut receivers = BTreeSet::new();
        receiver_list.iter().for_each(|val| {
            receivers.insert(*val);
        });
        IDkgTranscriptParams::new(
            transcript_id,
            IDkgDealers::new(dealers).unwrap(),
            IDkgReceivers::new(receivers).unwrap(),
            RegistryVersion::from(0),
            AlgorithmId::ThresholdEcdsaSecp256k1,
            IDkgTranscriptOperation::Random,
        )
        .unwrap()
    }

    // Creates a test dealing
    fn create_dealing(transcript_id: IDkgTranscriptId, dealer_id: NodeId) -> EcdsaDealing {
        EcdsaDealing {
            requested_height: Height::from(10),
            dealer_id,
            transcript_id,
            dealing: dummy_idkg_dealing_for_tests(),
        }
    }

    // Creates a test dealing support
    fn create_support(
        transcript_id: IDkgTranscriptId,
        dealer_id: NodeId,
        signer: NodeId,
    ) -> EcdsaDealingSupport {
        EcdsaDealingSupport {
            content: create_dealing(transcript_id, dealer_id),
            signature: MultiSignatureShare::fake(signer),
        }
    }

    // Checks that the dealing with the given id is being added to the validated
    // pool
    fn is_dealing_added_to_validated(
        change_set: &[EcdsaChangeAction],
        transcript_id: &IDkgTranscriptId,
    ) -> bool {
        for action in change_set {
            if let EcdsaChangeAction::AddToValidated(EcdsaMessage::EcdsaDealing(dealing)) = action {
                if dealing.transcript_id == *transcript_id && dealing.dealer_id == NODE_1 {
                    return true;
                }
            }
        }
        false
    }

    // Checks that the dealing support for the given dealing is being added to the
    // validated pool
    fn is_dealing_support_added_to_validated(
        change_set: &[EcdsaChangeAction],
        transcript_id: &IDkgTranscriptId,
        dealer_id: &NodeId,
    ) -> bool {
        for action in change_set {
            if let EcdsaChangeAction::AddToValidated(EcdsaMessage::EcdsaDealingSupport(support)) =
                action
            {
                let dealing = &support.content;
                if dealing.transcript_id == *transcript_id
                    && dealing.dealer_id == *dealer_id
                    && support.signature.signer == NODE_1
                {
                    return true;
                }
            }
        }
        false
    }

    // Checks that artifact is being moved from unvalidated to validated pool
    fn is_moved_to_validated(change_set: &[EcdsaChangeAction], msg_id: &EcdsaMessageId) -> bool {
        for action in change_set {
            if let EcdsaChangeAction::MoveToValidated(id) = action {
                if *id == *msg_id {
                    return true;
                }
            }
        }
        false
    }

    // Checks that artifact is being removed from validated pool
    fn is_removed_from_validated(
        change_set: &[EcdsaChangeAction],
        msg_id: &EcdsaMessageId,
    ) -> bool {
        for action in change_set {
            if let EcdsaChangeAction::RemoveValidated(id) = action {
                if *id == *msg_id {
                    return true;
                }
            }
        }
        false
    }

    // Checks that artifact is being removed from unvalidated pool
    fn is_removed_from_unvalidated(
        change_set: &[EcdsaChangeAction],
        msg_id: &EcdsaMessageId,
    ) -> bool {
        for action in change_set {
            if let EcdsaChangeAction::RemoveUnvalidated(id) = action {
                if *id == *msg_id {
                    return true;
                }
            }
        }
        false
    }

    // Checks that artifact is being dropped as invalid
    fn is_handle_invalid(change_set: &[EcdsaChangeAction], msg_id: &EcdsaMessageId) -> bool {
        for action in change_set {
            if let EcdsaChangeAction::HandleInvalid(id, _) = action {
                if *id == *msg_id {
                    return true;
                }
            }
        }
        false
    }

    // Tests the Action logic
    #[test]
    fn test_action() {
        let (id_1, id_2, id_3, id_4) = (
            dummy_idkg_transcript_id_for_tests(1),
            dummy_idkg_transcript_id_for_tests(2),
            dummy_idkg_transcript_id_for_tests(3),
            dummy_idkg_transcript_id_for_tests(4),
        );

        // The finalized block requests transcripts 1, 2, 3
        let nodes = [NODE_1];
        let block_reader = TestEcdsaBlockReader::new(
            Height::from(100),
            vec![
                create_transcript_param(id_1, &nodes, &nodes),
                create_transcript_param(id_2, &nodes, &nodes),
                create_transcript_param(id_3, &nodes, &nodes),
            ],
        );

        // Message from a node ahead of us
        assert_eq!(
            Action::action(&block_reader, Height::from(200), &id_4),
            Action::Defer
        );

        // Messages for transcripts not being currently requested
        assert_eq!(
            Action::action(
                &block_reader,
                Height::from(100),
                &dummy_idkg_transcript_id_for_tests(234)
            ),
            Action::Drop
        );
        assert_eq!(
            Action::action(
                &block_reader,
                Height::from(10),
                &dummy_idkg_transcript_id_for_tests(234)
            ),
            Action::Drop
        );

        // Messages for transcripts currently requested
        let action = Action::action(&block_reader, Height::from(100), &id_1);
        match action {
            Action::Process(_) => {}
            _ => panic!("Unexpected action: {:?}", action),
        }

        let action = Action::action(&block_reader, Height::from(10), &id_2);
        match action {
            Action::Process(_) => {}
            _ => panic!("Unexpected action: {:?}", action),
        }
    }

    // Tests that dealings are sent for new transcripts, and requests already
    // in progress are filtered out.
    #[test]
    fn test_ecdsa_send_dealings() {
        ic_test_utilities::artifact_pool_config::with_test_pool_config(|pool_config| {
            with_test_replica_logger(|logger| {
                let (mut ecdsa_pool, pre_signer) = create_dependencies(pool_config, logger);
                let (id_1, id_2, id_3, id_4, id_5) = (
                    dummy_idkg_transcript_id_for_tests(1),
                    dummy_idkg_transcript_id_for_tests(2),
                    dummy_idkg_transcript_id_for_tests(3),
                    dummy_idkg_transcript_id_for_tests(4),
                    dummy_idkg_transcript_id_for_tests(5),
                );

                // Set up the ECDSA pool. Pool has dealings for transcripts 1, 2, 3.
                // Only dealing for transcript 1 is issued by us.
                let dealing_1 = create_dealing(id_1, NODE_1);
                let dealing_2 = create_dealing(id_2, NODE_2);
                let dealing_3 = create_dealing(id_3, NODE_3);
                let change_set = vec![
                    EcdsaChangeAction::AddToValidated(EcdsaMessage::EcdsaDealing(dealing_1)),
                    EcdsaChangeAction::AddToValidated(EcdsaMessage::EcdsaDealing(dealing_2)),
                    EcdsaChangeAction::AddToValidated(EcdsaMessage::EcdsaDealing(dealing_3)),
                ];
                ecdsa_pool.apply_changes(change_set);

                // Set up the transcript creation request
                // The block requests transcripts 1, 4, 5
                let t1 = create_transcript_param(id_1, &[NODE_1], &[NODE_2]);
                let t2 = create_transcript_param(id_4, &[NODE_1], &[NODE_3]);
                let t3 = create_transcript_param(id_5, &[NODE_1], &[NODE_4]);
                let block_reader = TestEcdsaBlockReader::new(Height::from(100), vec![t1, t2, t3]);

                // Since transcript 1 is already in progress, we should issue
                // dealings only for transcripts 4, 5
                let change_set = pre_signer.send_dealings(&ecdsa_pool, &block_reader);
                assert_eq!(change_set.len(), 2);
                assert!(is_dealing_added_to_validated(&change_set, &id_4));
                assert!(is_dealing_added_to_validated(&change_set, &id_5));
            })
        })
    }

    // Tests that dealing is not issued if the node is in the list of dealers
    // specified by the transcript params
    #[test]
    fn test_ecdsa_non_dealers_dont_send_dealings() {
        ic_test_utilities::artifact_pool_config::with_test_pool_config(|pool_config| {
            with_test_replica_logger(|logger| {
                let (ecdsa_pool, pre_signer) = create_dependencies(pool_config, logger);
                let (id_1, id_2) = (
                    dummy_idkg_transcript_id_for_tests(1),
                    dummy_idkg_transcript_id_for_tests(2),
                );

                // transcript 1 has NODE_1 as a dealer
                let t1 = create_transcript_param(id_1, &[NODE_1], &[NODE_1]);

                // Transcript 2 doesn't have NODE_1 as a dealer
                let t2 = create_transcript_param(id_2, &[NODE_2], &[NODE_2]);

                // Transcript 2 should not result in a dealing
                let block_reader = TestEcdsaBlockReader::new(Height::from(100), vec![t1, t2]);

                let change_set = pre_signer.send_dealings(&ecdsa_pool, &block_reader);
                assert_eq!(change_set.len(), 1);
                assert!(is_dealing_added_to_validated(&change_set, &id_1));
            })
        })
    }

    // Tests that received dealings are accepted/processed for eligible transcript
    // requests, and others dealings are either deferred or dropped.
    // TODO: mock crypto and test failure path
    #[test]
    fn test_ecdsa_validate_dealings() {
        ic_test_utilities::artifact_pool_config::with_test_pool_config(|pool_config| {
            with_test_replica_logger(|logger| {
                let (mut ecdsa_pool, pre_signer) = create_dependencies(pool_config, logger);
                let time_source = FastForwardTimeSource::new();
                let (id_1, id_2, id_3, id_4) = (
                    dummy_idkg_transcript_id_for_tests(1),
                    dummy_idkg_transcript_id_for_tests(2),
                    dummy_idkg_transcript_id_for_tests(3),
                    dummy_idkg_transcript_id_for_tests(4),
                );

                // Set up the transcript creation request
                // The block requests transcripts 2, 3
                let t2 = create_transcript_param(id_2, &[NODE_2], &[NODE_1]);
                let t3 = create_transcript_param(id_3, &[NODE_2], &[NODE_1]);
                let block_reader = TestEcdsaBlockReader::new(Height::from(100), vec![t2, t3]);

                // Set up the ECDSA pool
                // A dealing from a node ahead of us (deferred)
                let mut dealing = create_dealing(id_1, NODE_2);
                dealing.requested_height = Height::from(200);
                ecdsa_pool.insert(UnvalidatedArtifact {
                    message: EcdsaMessage::EcdsaDealing(dealing),
                    peer_id: NODE_2,
                    timestamp: time_source.get_relative_time(),
                });

                // A dealing for a transcript that is requested by finalized block (accepted)
                let mut dealing = create_dealing(id_2, NODE_2);
                dealing.requested_height = Height::from(100);
                let key = dealing.key();
                let msg_id_2 = EcdsaDealing::key_to_outer_hash(&key);
                ecdsa_pool.insert(UnvalidatedArtifact {
                    message: EcdsaMessage::EcdsaDealing(dealing),
                    peer_id: NODE_2,
                    timestamp: time_source.get_relative_time(),
                });

                // A dealing for a transcript that is requested by finalized block (accepted)
                let mut dealing = create_dealing(id_3, NODE_2);
                dealing.requested_height = Height::from(10);
                let key = dealing.key();
                let msg_id_3 = EcdsaDealing::key_to_outer_hash(&key);
                ecdsa_pool.insert(UnvalidatedArtifact {
                    message: EcdsaMessage::EcdsaDealing(dealing),
                    peer_id: NODE_2,
                    timestamp: time_source.get_relative_time(),
                });

                // A dealing for a transcript that is not requested by finalized block (dropped)
                let mut dealing = create_dealing(id_4, NODE_2);
                dealing.requested_height = Height::from(5);
                let key = dealing.key();
                let msg_id_4 = EcdsaDealing::key_to_outer_hash(&key);
                ecdsa_pool.insert(UnvalidatedArtifact {
                    message: EcdsaMessage::EcdsaDealing(dealing),
                    peer_id: NODE_2,
                    timestamp: time_source.get_relative_time(),
                });

                let change_set = pre_signer.validate_dealings(&ecdsa_pool, &block_reader);
                assert_eq!(change_set.len(), 3);
                assert!(is_moved_to_validated(&change_set, &msg_id_2));
                assert!(is_moved_to_validated(&change_set, &msg_id_3));
                assert!(is_removed_from_unvalidated(&change_set, &msg_id_4));
            })
        })
    }

    // Tests that duplicate dealings from a dealer for the same transcript
    // are dropped.
    #[test]
    fn test_ecdsa_duplicate_dealing_from_dealer() {
        ic_test_utilities::artifact_pool_config::with_test_pool_config(|pool_config| {
            with_test_replica_logger(|logger| {
                let (mut ecdsa_pool, pre_signer) = create_dependencies(pool_config, logger);
                let time_source = FastForwardTimeSource::new();
                let id_2 = dummy_idkg_transcript_id_for_tests(2);

                // Set up the ECDSA pool
                // Validated pool has: {transcript 2, dealer = NODE_2}
                let dealing = create_dealing(id_2, NODE_2);
                let change_set = vec![EcdsaChangeAction::AddToValidated(
                    EcdsaMessage::EcdsaDealing(dealing),
                )];
                ecdsa_pool.apply_changes(change_set);

                // Unvalidated pool has: {transcript 2, dealer = NODE_2, height = 100}
                let mut dealing = create_dealing(id_2, NODE_2);
                dealing.requested_height = Height::from(100);
                let key = dealing.key();
                let msg_id_2 = EcdsaDealing::key_to_outer_hash(&key);
                ecdsa_pool.insert(UnvalidatedArtifact {
                    message: EcdsaMessage::EcdsaDealing(dealing),
                    peer_id: NODE_2,
                    timestamp: time_source.get_relative_time(),
                });

                let t2 = create_transcript_param(id_2, &[NODE_2], &[NODE_1]);
                let block_reader = TestEcdsaBlockReader::new(Height::from(100), vec![t2]);

                let change_set = pre_signer.validate_dealings(&ecdsa_pool, &block_reader);
                assert_eq!(change_set.len(), 1);
                assert!(is_handle_invalid(&change_set, &msg_id_2));
            })
        })
    }

    // Tests that duplicate dealings from a dealer for the same transcript
    // in the unvalidated pool are dropped.
    #[test]
    fn test_ecdsa_duplicate_dealing_from_dealer_in_batch() {
        ic_test_utilities::artifact_pool_config::with_test_pool_config(|pool_config| {
            with_test_replica_logger(|logger| {
                let (mut ecdsa_pool, pre_signer) = create_dependencies(pool_config, logger);
                let time_source = FastForwardTimeSource::new();
                let id_2 = dummy_idkg_transcript_id_for_tests(2);

                // Set up the ECDSA pool
                // Unvalidated pool has: {transcript 2, dealer = NODE_2, height = 100}
                let mut dealing = create_dealing(id_2, NODE_2);
                dealing.requested_height = Height::from(100);
                let key = dealing.key();
                let msg_id_2_a = EcdsaDealing::key_to_outer_hash(&key);
                ecdsa_pool.insert(UnvalidatedArtifact {
                    message: EcdsaMessage::EcdsaDealing(dealing),
                    peer_id: NODE_2,
                    timestamp: time_source.get_relative_time(),
                });

                // Unvalidated pool has: {transcript 2, dealer = NODE_2, height = 10}
                let mut dealing = create_dealing(id_2, NODE_2);
                dealing.requested_height = Height::from(10);
                let key = dealing.key();
                let msg_id_2_b = EcdsaDealing::key_to_outer_hash(&key);
                ecdsa_pool.insert(UnvalidatedArtifact {
                    message: EcdsaMessage::EcdsaDealing(dealing),
                    peer_id: NODE_2,
                    timestamp: time_source.get_relative_time(),
                });

                // Unvalidated pool has: {transcript 2, dealer = NODE_3, height = 90}
                let mut dealing = create_dealing(id_2, NODE_3);
                dealing.requested_height = Height::from(90);
                let key = dealing.key();
                let msg_id_3 = EcdsaDealing::key_to_outer_hash(&key);
                ecdsa_pool.insert(UnvalidatedArtifact {
                    message: EcdsaMessage::EcdsaDealing(dealing),
                    peer_id: NODE_3,
                    timestamp: time_source.get_relative_time(),
                });

                let t2 = create_transcript_param(id_2, &[NODE_2, NODE_3], &[NODE_1]);
                let block_reader = TestEcdsaBlockReader::new(Height::from(100), vec![t2]);

                // msg_id_2_a, msg_id_2_a should be dropped as duplicates
                let change_set = pre_signer.validate_dealings(&ecdsa_pool, &block_reader);
                assert_eq!(change_set.len(), 3);
                assert!(is_handle_invalid(&change_set, &msg_id_2_a));
                assert!(is_handle_invalid(&change_set, &msg_id_2_b));
                assert!(is_moved_to_validated(&change_set, &msg_id_3));
            })
        })
    }

    // Tests that dealings from a dealer that is not in the dealer list for the
    // transcript are dropped.
    #[test]
    fn test_ecdsa_unexpected_dealing_from_dealer() {
        ic_test_utilities::artifact_pool_config::with_test_pool_config(|pool_config| {
            with_test_replica_logger(|logger| {
                let (mut ecdsa_pool, pre_signer) = create_dependencies(pool_config, logger);
                let time_source = FastForwardTimeSource::new();
                let id_2 = dummy_idkg_transcript_id_for_tests(2);

                // Unvalidated pool has: {transcript 2, dealer = NODE_2, height = 100}
                let mut dealing = create_dealing(id_2, NODE_2);
                dealing.requested_height = Height::from(100);
                let key = dealing.key();
                let msg_id_2 = EcdsaDealing::key_to_outer_hash(&key);
                ecdsa_pool.insert(UnvalidatedArtifact {
                    message: EcdsaMessage::EcdsaDealing(dealing),
                    peer_id: NODE_2,
                    timestamp: time_source.get_relative_time(),
                });

                // NODE_2 is not in the dealer list
                let t2 = create_transcript_param(id_2, &[NODE_3], &[NODE_1]);
                let block_reader = TestEcdsaBlockReader::new(Height::from(100), vec![t2]);

                let change_set = pre_signer.validate_dealings(&ecdsa_pool, &block_reader);
                assert_eq!(change_set.len(), 1);
                assert!(is_handle_invalid(&change_set, &msg_id_2));
            })
        })
    }

    // Tests that support shares are sent to eligible dealings
    #[test]
    fn test_ecdsa_send_support() {
        ic_test_utilities::artifact_pool_config::with_test_pool_config(|pool_config| {
            with_test_replica_logger(|logger| {
                let (mut ecdsa_pool, pre_signer) = create_dependencies(pool_config, logger);
                let id = dummy_idkg_transcript_id_for_tests(1);

                // We haven't sent support yet, and we are in the receiver list
                let dealing = create_dealing(id, NODE_2);
                let change_set = vec![EcdsaChangeAction::AddToValidated(
                    EcdsaMessage::EcdsaDealing(dealing),
                )];
                ecdsa_pool.apply_changes(change_set);
                let t = create_transcript_param(id, &[NODE_2], &[NODE_1]);

                let block_reader = TestEcdsaBlockReader::new(Height::from(100), vec![t]);
                let change_set = pre_signer.send_dealing_support(&ecdsa_pool, &block_reader);
                assert_eq!(change_set.len(), 1);
                assert!(is_dealing_support_added_to_validated(
                    &change_set,
                    &id,
                    &NODE_2
                ));
                ecdsa_pool.apply_changes(change_set);

                // Since we already issued support for the dealing, it should not produce any
                // more support.
                let change_set = pre_signer.send_dealing_support(&ecdsa_pool, &block_reader);
                assert!(change_set.is_empty());
            })
        })
    }

    // Tests that support shares are not sent by nodes not in the receiver list for
    // the transcript
    #[test]
    fn test_ecdsa_non_receivers_dont_send_support() {
        ic_test_utilities::artifact_pool_config::with_test_pool_config(|pool_config| {
            with_test_replica_logger(|logger| {
                let (mut ecdsa_pool, pre_signer) = create_dependencies(pool_config, logger);
                let id = dummy_idkg_transcript_id_for_tests(1);

                // We are not in the receiver list for the transcript
                let dealing = create_dealing(id, NODE_2);
                let change_set = vec![EcdsaChangeAction::AddToValidated(
                    EcdsaMessage::EcdsaDealing(dealing),
                )];
                ecdsa_pool.apply_changes(change_set);
                let t = create_transcript_param(id, &[NODE_2], &[NODE_3]);

                let block_reader = TestEcdsaBlockReader::new(Height::from(100), vec![t]);
                let change_set = pre_signer.send_dealing_support(&ecdsa_pool, &block_reader);
                assert!(change_set.is_empty());
            })
        })
    }

    // Tests that support shares are not sent for transcripts we are not building
    #[test]
    fn test_ecdsa_no_support_for_missing_transcript_params() {
        ic_test_utilities::artifact_pool_config::with_test_pool_config(|pool_config| {
            with_test_replica_logger(|logger| {
                let (mut ecdsa_pool, pre_signer) = create_dependencies(pool_config, logger);
                let id = dummy_idkg_transcript_id_for_tests(1);

                let dealing = create_dealing(id, NODE_2);
                let change_set = vec![EcdsaChangeAction::AddToValidated(
                    EcdsaMessage::EcdsaDealing(dealing),
                )];
                ecdsa_pool.apply_changes(change_set);

                let block_reader = TestEcdsaBlockReader::new(Height::from(100), vec![]);
                let change_set = pre_signer.send_dealing_support(&ecdsa_pool, &block_reader);
                assert!(change_set.is_empty());
            })
        })
    }

    // Tests that received support shares are accepted/processed for eligible
    // transcript requests, and others dealings are either deferred or dropped.
    #[test]
    fn test_ecdsa_validate_dealing_support() {
        ic_test_utilities::artifact_pool_config::with_test_pool_config(|pool_config| {
            with_test_replica_logger(|logger| {
                let (mut ecdsa_pool, pre_signer) = create_dependencies(pool_config, logger);
                let time_source = FastForwardTimeSource::new();
                let (id_1, id_2, id_3, id_4) = (
                    dummy_idkg_transcript_id_for_tests(1),
                    dummy_idkg_transcript_id_for_tests(2),
                    dummy_idkg_transcript_id_for_tests(3),
                    dummy_idkg_transcript_id_for_tests(4),
                );

                // Set up the transcript creation request
                // The block requests transcripts 2, 3
                let t2 = create_transcript_param(id_2, &[NODE_2], &[NODE_3]);
                let t3 = create_transcript_param(id_3, &[NODE_2], &[NODE_3]);
                let block_reader = TestEcdsaBlockReader::new(Height::from(100), vec![t2, t3]);

                // Set up the ECDSA pool
                // A share from a node ahead of us (share deferred)
                let mut support = create_support(id_1, NODE_2, NODE_3);
                support.content.requested_height = Height::from(200);
                ecdsa_pool.insert(UnvalidatedArtifact {
                    message: EcdsaMessage::EcdsaDealingSupport(support),
                    peer_id: NODE_3,
                    timestamp: time_source.get_relative_time(),
                });

                // A dealing for a transcript that is requested by finalized block,
                // and we already have the dealing(share accepted)
                let mut dealing = create_dealing(id_2, NODE_2);
                dealing.requested_height = Height::from(25);
                let change_set = vec![EcdsaChangeAction::AddToValidated(
                    EcdsaMessage::EcdsaDealing(dealing),
                )];
                ecdsa_pool.apply_changes(change_set);

                let mut support = create_support(id_2, NODE_2, NODE_3);
                support.content.requested_height = Height::from(25);
                let key = support.key();
                let msg_id_2 = EcdsaDealingSupport::key_to_outer_hash(&key);
                ecdsa_pool.insert(UnvalidatedArtifact {
                    message: EcdsaMessage::EcdsaDealingSupport(support),
                    peer_id: NODE_3,
                    timestamp: time_source.get_relative_time(),
                });

                // A dealing for a transcript that is requested by finalized block,
                // but we don't have the dealing yet(share deferred)
                let mut support = create_support(id_3, NODE_2, NODE_3);
                support.content.requested_height = Height::from(10);
                ecdsa_pool.insert(UnvalidatedArtifact {
                    message: EcdsaMessage::EcdsaDealingSupport(support),
                    peer_id: NODE_3,
                    timestamp: time_source.get_relative_time(),
                });

                // A dealing for a transcript that is not requested by finalized block
                // (share dropped)
                let mut support = create_support(id_4, NODE_2, NODE_3);
                support.content.requested_height = Height::from(5);
                let key = support.key();
                let msg_id_4 = EcdsaDealingSupport::key_to_outer_hash(&key);
                ecdsa_pool.insert(UnvalidatedArtifact {
                    message: EcdsaMessage::EcdsaDealingSupport(support),
                    peer_id: NODE_3,
                    timestamp: time_source.get_relative_time(),
                });

                let change_set = pre_signer.validate_dealing_support(&ecdsa_pool, &block_reader);
                assert_eq!(change_set.len(), 2);
                assert!(is_moved_to_validated(&change_set, &msg_id_2));
                assert!(is_removed_from_unvalidated(&change_set, &msg_id_4));
            })
        })
    }

    // Tests that duplicate support from a node for the same dealing
    // are dropped.
    #[test]
    fn test_ecdsa_duplicate_support_from_node() {
        ic_test_utilities::artifact_pool_config::with_test_pool_config(|pool_config| {
            with_test_replica_logger(|logger| {
                let (mut ecdsa_pool, pre_signer) = create_dependencies(pool_config, logger);
                let time_source = FastForwardTimeSource::new();
                let id = dummy_idkg_transcript_id_for_tests(1);

                // Set up the ECDSA pool
                // Validated pool has: support {transcript 2, dealer = NODE_2, signer = NODE_3}
                let dealing = create_dealing(id, NODE_2);
                let change_set = vec![EcdsaChangeAction::AddToValidated(
                    EcdsaMessage::EcdsaDealing(dealing),
                )];
                ecdsa_pool.apply_changes(change_set);

                let support = create_support(id, NODE_2, NODE_3);
                let change_set = vec![EcdsaChangeAction::AddToValidated(
                    EcdsaMessage::EcdsaDealingSupport(support),
                )];
                ecdsa_pool.apply_changes(change_set);

                // Unvalidated pool has: support {transcript 2, dealer = NODE_2, signer =
                // NODE_3}
                let mut support = create_support(id, NODE_2, NODE_3);
                support.content.requested_height = Height::from(100);
                let key = support.key();
                let msg_id = EcdsaDealingSupport::key_to_outer_hash(&key);
                ecdsa_pool.insert(UnvalidatedArtifact {
                    message: EcdsaMessage::EcdsaDealingSupport(support),
                    peer_id: NODE_3,
                    timestamp: time_source.get_relative_time(),
                });

                let t = create_transcript_param(id, &[NODE_2], &[NODE_3]);
                let block_reader = TestEcdsaBlockReader::new(Height::from(100), vec![t]);

                let change_set = pre_signer.validate_dealing_support(&ecdsa_pool, &block_reader);
                assert_eq!(change_set.len(), 1);
                assert!(is_handle_invalid(&change_set, &msg_id));
            })
        })
    }

    // Tests that duplicate support from a node for the same dealing
    // in the unvalidated pool are dropped.
    #[test]
    fn test_ecdsa_duplicate_support_from_node_in_batch() {
        ic_test_utilities::artifact_pool_config::with_test_pool_config(|pool_config| {
            with_test_replica_logger(|logger| {
                let (mut ecdsa_pool, pre_signer) = create_dependencies(pool_config, logger);
                let time_source = FastForwardTimeSource::new();
                let id = dummy_idkg_transcript_id_for_tests(1);

                // Set up the ECDSA pool
                // Unvalidated pool has: support {transcript 2, dealer = NODE_2, signer =
                // NODE_3}
                let mut support = create_support(id, NODE_2, NODE_3);
                support.content.requested_height = Height::from(100);
                let key = support.key();
                let msg_id_1_a = EcdsaDealingSupport::key_to_outer_hash(&key);
                ecdsa_pool.insert(UnvalidatedArtifact {
                    message: EcdsaMessage::EcdsaDealingSupport(support),
                    peer_id: NODE_3,
                    timestamp: time_source.get_relative_time(),
                });

                // Unvalidated pool has: support {transcript 2, dealer = NODE_2, signer =
                // NODE_3}
                let mut support = create_support(id, NODE_2, NODE_3);
                support.content.requested_height = Height::from(10);
                let key = support.key();
                let msg_id_1_b = EcdsaDealingSupport::key_to_outer_hash(&key);
                ecdsa_pool.insert(UnvalidatedArtifact {
                    message: EcdsaMessage::EcdsaDealingSupport(support),
                    peer_id: NODE_3,
                    timestamp: time_source.get_relative_time(),
                });

                // Unvalidated pool has: support {transcript 2, dealer = NODE_2, signer =
                // NODE_4}
                let dealing = create_dealing(id, NODE_2);
                let change_set = vec![EcdsaChangeAction::AddToValidated(
                    EcdsaMessage::EcdsaDealing(dealing),
                )];
                ecdsa_pool.apply_changes(change_set);

                let mut support = create_support(id, NODE_2, NODE_4);
                support.content.requested_height = Height::from(10);
                let key = support.key();
                let msg_id_2 = EcdsaDealingSupport::key_to_outer_hash(&key);
                ecdsa_pool.insert(UnvalidatedArtifact {
                    message: EcdsaMessage::EcdsaDealingSupport(support),
                    peer_id: NODE_4,
                    timestamp: time_source.get_relative_time(),
                });

                let t = create_transcript_param(id, &[NODE_2], &[NODE_3, NODE_4]);
                let block_reader = TestEcdsaBlockReader::new(Height::from(100), vec![t]);

                let change_set = pre_signer.validate_dealing_support(&ecdsa_pool, &block_reader);
                assert_eq!(change_set.len(), 3);
                assert!(is_handle_invalid(&change_set, &msg_id_1_a));
                assert!(is_handle_invalid(&change_set, &msg_id_1_b));
                assert!(is_moved_to_validated(&change_set, &msg_id_2));
            })
        })
    }

    // Tests that support from a node that is not in the receiver list for the
    // transcript are dropped.
    #[test]
    fn test_ecdsa_unexpected_support_from_node() {
        ic_test_utilities::artifact_pool_config::with_test_pool_config(|pool_config| {
            with_test_replica_logger(|logger| {
                let (mut ecdsa_pool, pre_signer) = create_dependencies(pool_config, logger);
                let time_source = FastForwardTimeSource::new();
                let id = dummy_idkg_transcript_id_for_tests(1);

                // Unvalidated pool has: support {transcript 2, dealer = NODE_2, signer =
                // NODE_3}
                let mut support = create_support(id, NODE_2, NODE_3);
                support.content.requested_height = Height::from(10);
                let key = support.key();
                let msg_id = EcdsaDealingSupport::key_to_outer_hash(&key);
                ecdsa_pool.insert(UnvalidatedArtifact {
                    message: EcdsaMessage::EcdsaDealingSupport(support),
                    peer_id: NODE_3,
                    timestamp: time_source.get_relative_time(),
                });

                // NODE_3 is not in the receiver list
                let t = create_transcript_param(id, &[NODE_2], &[NODE_4]);
                let block_reader = TestEcdsaBlockReader::new(Height::from(100), vec![t]);
                let change_set = pre_signer.validate_dealing_support(&ecdsa_pool, &block_reader);
                assert_eq!(change_set.len(), 1);
                assert!(is_handle_invalid(&change_set, &msg_id));
            })
        })
    }

    // Tests purging of dealings from unvalidated pool
    #[test]
    fn test_ecdsa_purge_unvalidated_dealings() {
        ic_test_utilities::artifact_pool_config::with_test_pool_config(|pool_config| {
            with_test_replica_logger(|logger| {
                let (mut ecdsa_pool, pre_signer) = create_dependencies(pool_config, logger);
                let time_source = FastForwardTimeSource::new();
                let (id_1, id_2, id_3) = (
                    dummy_idkg_transcript_id_for_tests(1),
                    dummy_idkg_transcript_id_for_tests(2),
                    dummy_idkg_transcript_id_for_tests(3),
                );

                // Dealing 1: height <= current_height, in_progress (not purged)
                let mut dealing_1 = create_dealing(id_1, NODE_2);
                dealing_1.requested_height = Height::from(20);
                ecdsa_pool.insert(UnvalidatedArtifact {
                    message: EcdsaMessage::EcdsaDealing(dealing_1),
                    peer_id: NODE_2,
                    timestamp: time_source.get_relative_time(),
                });

                // Dealing 2: height <= current_height, !in_progress (purged)
                let mut dealing_2 = create_dealing(id_2, NODE_2);
                dealing_2.requested_height = Height::from(20);
                let key = dealing_2.key();
                let msg_id_2 = EcdsaDealing::key_to_outer_hash(&key);
                ecdsa_pool.insert(UnvalidatedArtifact {
                    message: EcdsaMessage::EcdsaDealing(dealing_2),
                    peer_id: NODE_2,
                    timestamp: time_source.get_relative_time(),
                });

                // Dealing 3: height > current_height (not purged)
                let mut dealing_3 = create_dealing(id_3, NODE_2);
                dealing_3.requested_height = Height::from(200);
                ecdsa_pool.insert(UnvalidatedArtifact {
                    message: EcdsaMessage::EcdsaDealing(dealing_3),
                    peer_id: NODE_2,
                    timestamp: time_source.get_relative_time(),
                });

                let t = create_transcript_param(id_1, &[NODE_2], &[NODE_4]);
                let block_reader = TestEcdsaBlockReader::new(Height::from(100), vec![t]);
                let change_set = pre_signer.purge_artifacts(&ecdsa_pool, &block_reader);
                assert_eq!(change_set.len(), 1);
                assert!(is_removed_from_unvalidated(&change_set, &msg_id_2));
            })
        })
    }

    // Tests purging of dealings from validated pool
    #[test]
    fn test_ecdsa_purge_validated_dealings() {
        ic_test_utilities::artifact_pool_config::with_test_pool_config(|pool_config| {
            with_test_replica_logger(|logger| {
                let (mut ecdsa_pool, pre_signer) = create_dependencies(pool_config, logger);
                let (id_1, id_2, id_3) = (
                    dummy_idkg_transcript_id_for_tests(1),
                    dummy_idkg_transcript_id_for_tests(2),
                    dummy_idkg_transcript_id_for_tests(3),
                );

                // Dealing 1: height <= current_height, in_progress (not purged)
                let mut dealing_1 = create_dealing(id_1, NODE_2);
                dealing_1.requested_height = Height::from(20);

                // Dealing 2: height <= current_height, !in_progress (purged)
                let mut dealing_2 = create_dealing(id_2, NODE_2);
                dealing_2.requested_height = Height::from(20);
                let key = dealing_2.key();
                let msg_id_2 = EcdsaDealing::key_to_outer_hash(&key);

                // Dealing 3: height > current_height (not purged)
                let mut dealing_3 = create_dealing(id_3, NODE_2);
                dealing_3.requested_height = Height::from(200);

                let change_set = vec![
                    EcdsaChangeAction::AddToValidated(EcdsaMessage::EcdsaDealing(dealing_1)),
                    EcdsaChangeAction::AddToValidated(EcdsaMessage::EcdsaDealing(dealing_2)),
                    EcdsaChangeAction::AddToValidated(EcdsaMessage::EcdsaDealing(dealing_3)),
                ];
                ecdsa_pool.apply_changes(change_set);

                let t = create_transcript_param(id_1, &[NODE_2], &[NODE_4]);
                let block_reader = TestEcdsaBlockReader::new(Height::from(100), vec![t]);
                let change_set = pre_signer.purge_artifacts(&ecdsa_pool, &block_reader);
                assert_eq!(change_set.len(), 1);
                assert!(is_removed_from_validated(&change_set, &msg_id_2));
            })
        })
    }

    // Tests purging of dealing support from unvalidated pool
    #[test]
    fn test_ecdsa_purge_unvalidated_dealing_support() {
        ic_test_utilities::artifact_pool_config::with_test_pool_config(|pool_config| {
            with_test_replica_logger(|logger| {
                let (mut ecdsa_pool, pre_signer) = create_dependencies(pool_config, logger);
                let time_source = FastForwardTimeSource::new();
                let (id_1, id_2, id_3) = (
                    dummy_idkg_transcript_id_for_tests(1),
                    dummy_idkg_transcript_id_for_tests(2),
                    dummy_idkg_transcript_id_for_tests(3),
                );

                // Support 1: height <= current_height, in_progress (not purged)
                let mut support_1 = create_support(id_1, NODE_2, NODE_3);
                support_1.content.requested_height = Height::from(20);
                ecdsa_pool.insert(UnvalidatedArtifact {
                    message: EcdsaMessage::EcdsaDealingSupport(support_1),
                    peer_id: NODE_2,
                    timestamp: time_source.get_relative_time(),
                });

                // Dealing 2: height <= current_height, !in_progress (purged)
                let mut support_2 = create_support(id_2, NODE_2, NODE_3);
                support_2.content.requested_height = Height::from(20);
                let key = support_2.key();
                let msg_id_2 = EcdsaDealingSupport::key_to_outer_hash(&key);
                ecdsa_pool.insert(UnvalidatedArtifact {
                    message: EcdsaMessage::EcdsaDealingSupport(support_2),
                    peer_id: NODE_2,
                    timestamp: time_source.get_relative_time(),
                });

                // Dealing 3: height > current_height (not purged)
                let mut support_3 = create_support(id_3, NODE_2, NODE_3);
                support_3.content.requested_height = Height::from(200);
                ecdsa_pool.insert(UnvalidatedArtifact {
                    message: EcdsaMessage::EcdsaDealingSupport(support_3),
                    peer_id: NODE_2,
                    timestamp: time_source.get_relative_time(),
                });

                let t = create_transcript_param(id_1, &[NODE_2], &[NODE_4]);
                let block_reader = TestEcdsaBlockReader::new(Height::from(100), vec![t]);
                let change_set = pre_signer.purge_artifacts(&ecdsa_pool, &block_reader);
                assert_eq!(change_set.len(), 1);
                assert!(is_removed_from_unvalidated(&change_set, &msg_id_2));
            })
        })
    }

    // Tests purging of dealing support from validated pool
    #[test]
    fn test_ecdsa_purge_validated_dealing_support() {
        ic_test_utilities::artifact_pool_config::with_test_pool_config(|pool_config| {
            with_test_replica_logger(|logger| {
                let (mut ecdsa_pool, pre_signer) = create_dependencies(pool_config, logger);
                let (id_1, id_2, id_3) = (
                    dummy_idkg_transcript_id_for_tests(1),
                    dummy_idkg_transcript_id_for_tests(2),
                    dummy_idkg_transcript_id_for_tests(3),
                );

                // Support 1: height <= current_height, in_progress (not purged)
                let mut support_1 = create_support(id_1, NODE_2, NODE_3);
                support_1.content.requested_height = Height::from(20);

                // Dealing 2: height <= current_height, !in_progress (purged)
                let mut support_2 = create_support(id_2, NODE_2, NODE_3);
                support_2.content.requested_height = Height::from(20);
                let key = support_2.key();
                let msg_id_2 = EcdsaDealingSupport::key_to_outer_hash(&key);

                // Dealing 3: height > current_height (not purged)
                let mut support_3 = create_support(id_3, NODE_2, NODE_3);
                support_3.content.requested_height = Height::from(200);

                let change_set = vec![
                    EcdsaChangeAction::AddToValidated(EcdsaMessage::EcdsaDealingSupport(support_1)),
                    EcdsaChangeAction::AddToValidated(EcdsaMessage::EcdsaDealingSupport(support_2)),
                    EcdsaChangeAction::AddToValidated(EcdsaMessage::EcdsaDealingSupport(support_3)),
                ];
                ecdsa_pool.apply_changes(change_set);

                let t = create_transcript_param(id_1, &[NODE_2], &[NODE_4]);
                let block_reader = TestEcdsaBlockReader::new(Height::from(100), vec![t]);
                let change_set = pre_signer.purge_artifacts(&ecdsa_pool, &block_reader);
                assert_eq!(change_set.len(), 1);
                assert!(is_removed_from_validated(&change_set, &msg_id_2));
            })
        })
    }
}
