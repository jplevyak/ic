/* tag::catalog[]
Title:: SSH Key Management Test

Goal:: Testing the newly-added registry support for readonly and backup SSH key management.

Coverage::
. adding/removing backup keys,
. adding/removing readonly keys,
. adding/removing a mixture of both,
. the max number of keys cannot be exceeded.

end::catalog[] */

use crate::{
    nns::{
        get_governance_canister, submit_external_proposal_with_test_id,
        vote_execute_proposal_assert_executed, vote_execute_proposal_assert_failed, NnsExt,
    },
    ssh_access_utils::*,
    util::{
        get_random_application_node_endpoint, get_random_nns_node_endpoint,
        get_random_unassigned_node_endpoint, runtime_from_url,
    },
};

use ic_fondue::{
    ic_manager::{IcControl, IcEndpoint, IcHandle},
    internet_computer::InternetComputer,
};
use ic_nns_governance::pb::v1::NnsFunction;
use ic_registry_subnet_type::SubnetType;
use ic_types::SubnetId;
use registry_canister::mutations::do_update_subnet::UpdateSubnetPayload;
use registry_canister::mutations::do_update_unassigned_nodes_config::UpdateUnassignedNodesConfigPayload;

use futures::Future;
use std::net::IpAddr;

pub fn config() -> InternetComputer {
    InternetComputer::new()
        .add_fast_single_node_subnet(SubnetType::System)
        .add_fast_single_node_subnet(SubnetType::Application)
        .with_unassigned_nodes(1)
}

pub fn root_cannot_authenticate(handle: IcHandle, ctx: &fondue::pot::Context) {
    let mut rng = ctx.rng.clone();

    // Choose a random nodes
    let nns_node = get_random_nns_node_endpoint(&handle, &mut rng);
    let app_node = get_random_application_node_endpoint(&handle, &mut rng);
    let unassigned_node = get_random_unassigned_node_endpoint(&handle, &mut rng);
    block_on(nns_node.assert_ready(ctx));
    block_on(app_node.assert_ready(ctx));
    //block_on(unassigned_node.assert_ready(ctx));

    let nns_node_ip: IpAddr = nns_node.ip_address().unwrap();
    let app_node_ip: IpAddr = app_node.ip_address().unwrap();
    let unassigned_node_ip: IpAddr = unassigned_node.ip_address().unwrap();

    let mean = AuthMean::Password("root".to_string());
    assert_authentication_fails(&nns_node_ip, "root", &mean);
    assert_authentication_fails(&app_node_ip, "root", &mean);
    assert_authentication_fails(&unassigned_node_ip, "root", &mean);
}

pub fn readonly_cannot_authenticate_without_a_key(handle: IcHandle, ctx: &fondue::pot::Context) {
    let mut rng = ctx.rng.clone();

    // Choose a random nodes
    let nns_node = get_random_nns_node_endpoint(&handle, &mut rng);
    let app_node = get_random_application_node_endpoint(&handle, &mut rng);
    let unassigned_node = get_random_unassigned_node_endpoint(&handle, &mut rng);

    let nns_node_ip: IpAddr = nns_node.ip_address().unwrap();
    let app_node_ip: IpAddr = app_node.ip_address().unwrap();
    let unassigned_node_ip: IpAddr = unassigned_node.ip_address().unwrap();

    let mean = AuthMean::None;
    assert_authentication_fails(&nns_node_ip, "readonly", &mean);
    assert_authentication_fails(&app_node_ip, "readonly", &mean);
    assert_authentication_fails(&unassigned_node_ip, "readonly", &mean);
}

pub fn readonly_cannot_authenticate_with_random_key(handle: IcHandle, ctx: &fondue::pot::Context) {
    let mut rng = ctx.rng.clone();

    // Choose a random nodes
    let nns_node = get_random_nns_node_endpoint(&handle, &mut rng);
    let app_node = get_random_application_node_endpoint(&handle, &mut rng);
    let unassigned_node = get_random_unassigned_node_endpoint(&handle, &mut rng);

    let nns_node_ip: IpAddr = nns_node.ip_address().unwrap();
    let app_node_ip: IpAddr = app_node.ip_address().unwrap();
    let unassigned_node_ip: IpAddr = unassigned_node.ip_address().unwrap();

    let (private_key, _public_key) = generate_key_strings();
    let mean = AuthMean::PrivateKey(private_key);
    assert_authentication_fails(&nns_node_ip, "readonly", &mean);
    assert_authentication_fails(&app_node_ip, "readonly", &mean);
    assert_authentication_fails(&unassigned_node_ip, "readonly", &mean);
}

pub fn keys_in_the_subnet_record_can_be_updated(handle: IcHandle, ctx: &fondue::pot::Context) {
    let mut rng = ctx.rng.clone();

    // Install NNS canisters
    ctx.install_nns_canisters(&handle, true);

    // Choose a random node from the nns subnet
    let nns_endpoint = get_random_nns_node_endpoint(&handle, &mut rng);
    block_on(nns_endpoint.assert_ready(ctx));

    let app_node = get_random_application_node_endpoint(&handle, &mut rng);
    let app_subnet_id = app_node.subnet_id().unwrap();
    let node_ip: IpAddr = app_node.ip_address().unwrap();

    // Update the registry with two new pairs of keys.
    let (readonly_private_key, readonly_public_key) = generate_key_strings();
    let (backup_private_key, backup_public_key) = generate_key_strings();
    let payload = get_updatesubnetpayload(
        app_subnet_id,
        Some(vec![readonly_public_key]),
        Some(vec![backup_public_key]),
    );
    block_on(update_the_subnet_record(nns_endpoint, payload));

    let readonly_mean = AuthMean::PrivateKey(readonly_private_key);
    let backup_mean = AuthMean::PrivateKey(backup_private_key);
    // Node manager updates checks if there is a new version of the registry every
    // 10 seconds. If so, it updates first the readonly and then the backup
    // keys. If backup key can authenticate we know that the readonly keys are
    // already updated too.
    wait_until_authentication_is_granted(&node_ip, "backup", &backup_mean);
    assert_authentication_works(&node_ip, "readonly", &readonly_mean);

    // Clear the keys in the registry
    let no_key_payload = get_updatesubnetpayload(app_subnet_id, Some(vec![]), Some(vec![]));
    block_on(update_the_subnet_record(nns_endpoint, no_key_payload));

    // Check that the access for these keys are also removed.
    wait_until_authentication_fails(&node_ip, "backup", &backup_mean);
    assert_authentication_fails(&node_ip, "readonly", &readonly_mean);
}

pub fn keys_for_unassigned_nodes_can_be_updated(handle: IcHandle, ctx: &fondue::pot::Context) {
    let mut rng = ctx.rng.clone();

    // Choose a random node from the nns subnet
    let nns_endpoint = get_random_nns_node_endpoint(&handle, &mut rng);
    block_on(nns_endpoint.assert_ready(ctx));

    let unassigned_node = get_random_unassigned_node_endpoint(&handle, &mut rng);
    let node_ip: IpAddr = unassigned_node.ip_address().unwrap();

    // Update the registry with two new pairs of keys.
    let (readonly_private_key, readonly_public_key) = generate_key_strings();
    let payload = get_updateunassignednodespayload(Some(vec![readonly_public_key]));
    block_on(update_ssh_keys_for_all_unassigned_nodes(
        nns_endpoint,
        payload,
    ));

    let readonly_mean = AuthMean::PrivateKey(readonly_private_key);
    wait_until_authentication_is_granted(&node_ip, "readonly", &readonly_mean);

    // Clear the keys in the registry
    let no_key_payload = get_updateunassignednodespayload(Some(vec![]));
    block_on(update_ssh_keys_for_all_unassigned_nodes(
        nns_endpoint,
        no_key_payload,
    ));

    // Check that the access for these keys are also removed.
    wait_until_authentication_fails(&node_ip, "readonly", &readonly_mean);
}

pub fn multiple_keys_can_access_one_account(handle: IcHandle, ctx: &fondue::pot::Context) {
    let mut rng = ctx.rng.clone();

    // Choose a random node from the nns subnet
    let nns_endpoint = get_random_nns_node_endpoint(&handle, &mut rng);
    block_on(nns_endpoint.assert_ready(ctx));

    let app_node = get_random_application_node_endpoint(&handle, &mut rng);
    let app_subnet_id = app_node.subnet_id().unwrap();
    let node_ip: IpAddr = app_node.ip_address().unwrap();

    // Update the registry with two new pairs of keys.
    let (readonly_private_key1, readonly_public_key1) = generate_key_strings();
    let (readonly_private_key2, readonly_public_key2) = generate_key_strings();
    let (readonly_private_key3, readonly_public_key3) = generate_key_strings();
    let (backup_private_key1, backup_public_key1) = generate_key_strings();
    let (backup_private_key2, backup_public_key2) = generate_key_strings();
    let (backup_private_key3, backup_public_key3) = generate_key_strings();
    let payload = get_updatesubnetpayload(
        app_subnet_id,
        Some(vec![
            readonly_public_key1,
            readonly_public_key2,
            readonly_public_key3,
        ]),
        Some(vec![
            backup_public_key1,
            backup_public_key2,
            backup_public_key3,
        ]),
    );
    block_on(update_the_subnet_record(nns_endpoint, payload));

    let readonly_mean1 = AuthMean::PrivateKey(readonly_private_key1);
    let readonly_mean2 = AuthMean::PrivateKey(readonly_private_key2);
    let readonly_mean3 = AuthMean::PrivateKey(readonly_private_key3);
    let backup_mean1 = AuthMean::PrivateKey(backup_private_key1);
    let backup_mean2 = AuthMean::PrivateKey(backup_private_key2);
    let backup_mean3 = AuthMean::PrivateKey(backup_private_key3);
    // Node manager updates checks if there is a new version of the registry every
    // 10 seconds. If so, it updates first the readonly and then the backup
    // keys. If backup key can authenticate we know that the readonly keys are
    // already updated too.
    wait_until_authentication_is_granted(&node_ip, "backup", &backup_mean1);
    assert_authentication_works(&node_ip, "backup", &backup_mean2);
    assert_authentication_works(&node_ip, "backup", &backup_mean3);
    assert_authentication_works(&node_ip, "readonly", &readonly_mean1);
    assert_authentication_works(&node_ip, "readonly", &readonly_mean2);
    assert_authentication_works(&node_ip, "readonly", &readonly_mean3);
}

pub fn multiple_keys_can_access_one_account_on_unassigned_nodes(
    handle: IcHandle,
    ctx: &fondue::pot::Context,
) {
    let mut rng = ctx.rng.clone();

    // Choose a random node from the nns subnet
    let nns_endpoint = get_random_nns_node_endpoint(&handle, &mut rng);
    block_on(nns_endpoint.assert_ready(ctx));

    let unassigned_node = get_random_unassigned_node_endpoint(&handle, &mut rng);
    let node_ip: IpAddr = unassigned_node.ip_address().unwrap();

    // Update the registry with two new pairs of keys.
    let (readonly_private_key1, readonly_public_key1) = generate_key_strings();
    let (readonly_private_key2, readonly_public_key2) = generate_key_strings();
    let (readonly_private_key3, readonly_public_key3) = generate_key_strings();
    let payload = get_updateunassignednodespayload(Some(vec![
        readonly_public_key1,
        readonly_public_key2,
        readonly_public_key3,
    ]));
    block_on(update_ssh_keys_for_all_unassigned_nodes(
        nns_endpoint,
        payload,
    ));

    let readonly_mean1 = AuthMean::PrivateKey(readonly_private_key1);
    let readonly_mean2 = AuthMean::PrivateKey(readonly_private_key2);
    let readonly_mean3 = AuthMean::PrivateKey(readonly_private_key3);
    // Node manager updates checks if there is a new version of the registry every
    // 10 seconds. If so, it updates first the readonly and then the backup
    // keys. If backup key can authenticate we know that the readonly keys are
    // already updated too.
    wait_until_authentication_is_granted(&node_ip, "readonly", &readonly_mean1);
    assert_authentication_works(&node_ip, "readonly", &readonly_mean2);
    assert_authentication_works(&node_ip, "readonly", &readonly_mean3);
}

pub fn updating_readonly_does_not_remove_backup_keys(handle: IcHandle, ctx: &fondue::pot::Context) {
    let mut rng = ctx.rng.clone();

    // Choose a random node from the nns subnet
    let nns_endpoint = get_random_nns_node_endpoint(&handle, &mut rng);
    block_on(nns_endpoint.assert_ready(ctx));

    let app_node = get_random_application_node_endpoint(&handle, &mut rng);
    let app_subnet_id = app_node.subnet_id().unwrap();
    let node_ip: IpAddr = app_node.ip_address().unwrap();

    // Add a backup key.
    let (backup_private_key, backup_public_key) = generate_key_strings();
    let payload1 = get_updatesubnetpayload(app_subnet_id, None, Some(vec![backup_public_key]));
    block_on(update_the_subnet_record(nns_endpoint, payload1));

    // Check that the backup key can authenticate.
    let backup_mean = AuthMean::PrivateKey(backup_private_key);
    wait_until_authentication_is_granted(&node_ip, "backup", &backup_mean);

    // Now add a readonly key.
    let (readonly_private_key, readonly_public_key) = generate_key_strings();
    let payload2 = get_updatesubnetpayload(app_subnet_id, Some(vec![readonly_public_key]), None);
    block_on(update_the_subnet_record(nns_endpoint, payload2));

    // Check that the readonly key can authenticate now and the backup key can still
    // authenticate too.
    let readonly_mean = AuthMean::PrivateKey(readonly_private_key);
    wait_until_authentication_is_granted(&node_ip, "readonly", &readonly_mean);
    assert_authentication_works(&node_ip, "backup", &backup_mean);

    // Now send a proposal that only removes the readonly keys.
    let payload3 = get_updatesubnetpayload(app_subnet_id, Some(vec![]), None);
    block_on(update_the_subnet_record(nns_endpoint, payload3));

    // Wait until the readonly key loses its access and ensure backup key still has
    // access.
    wait_until_authentication_fails(&node_ip, "readonly", &readonly_mean);
    assert_authentication_works(&node_ip, "backup", &backup_mean);
}

pub fn can_add_100_readonly_and_backup_keys(handle: IcHandle, ctx: &fondue::pot::Context) {
    let mut rng = ctx.rng.clone();

    // Choose a random node from the nns subnet
    let nns_endpoint = get_random_nns_node_endpoint(&handle, &mut rng);
    block_on(nns_endpoint.assert_ready(ctx));

    let app_subnet_id = get_random_application_node_endpoint(&handle, &mut rng)
        .subnet_id()
        .unwrap();

    let (_private_key, public_key) = generate_key_strings();
    // Update the registry with 50*2 new pairs of keys.
    let payload_for_subnet = get_updatesubnetpayload(
        app_subnet_id,
        Some(vec![public_key.clone(); 100]),
        Some(vec![public_key.clone(); 100]),
    );
    block_on(update_the_subnet_record(nns_endpoint, payload_for_subnet));

    // Also do that for unassigned nodes
    let payload_for_the_unassigned = get_updateunassignednodespayload(Some(vec![public_key; 50]));
    block_on(update_ssh_keys_for_all_unassigned_nodes(
        nns_endpoint,
        payload_for_the_unassigned,
    ));
}

pub fn cannot_add_101_readonly_or_backup_keys(handle: IcHandle, ctx: &fondue::pot::Context) {
    let mut rng = ctx.rng.clone();

    // Choose a random node from the nns subnet
    let nns_endpoint = get_random_nns_node_endpoint(&handle, &mut rng);
    block_on(nns_endpoint.assert_ready(ctx));

    let app_subnet_id = get_random_application_node_endpoint(&handle, &mut rng)
        .subnet_id()
        .unwrap();

    let (_private_key, public_key) = generate_key_strings();

    // Try to update the registry with 51 readonly keys.
    let readonly_payload = get_updatesubnetpayload(
        app_subnet_id,
        Some(vec![public_key.clone(); 101]),
        Some(vec![]),
    );
    block_on(fail_updating_the_subnet_record(
        nns_endpoint,
        readonly_payload,
    ));

    // Try to update the registry with 51 backup keys.
    let backup_payload = get_updatesubnetpayload(
        app_subnet_id,
        Some(vec![]),
        Some(vec![public_key.clone(); 101]),
    );
    block_on(fail_updating_the_subnet_record(
        nns_endpoint,
        backup_payload,
    ));

    // Also do that for unassigned nodes
    let readonly_payload_for_the_unassigned =
        get_updateunassignednodespayload(Some(vec![public_key; 101]));
    block_on(fail_updating_ssh_keys_for_all_unassigned_nodes(
        nns_endpoint,
        readonly_payload_for_the_unassigned,
    ));
}

fn get_updatesubnetpayload(
    subnet_id: SubnetId,
    readonly_keys: Option<Vec<String>>,
    backup_keys: Option<Vec<String>>,
) -> UpdateSubnetPayload {
    UpdateSubnetPayload {
        subnet_id,
        ingress_bytes_per_block_soft_cap: None,
        max_ingress_bytes_per_message: None,
        max_block_payload_size: None,
        unit_delay_millis: None,
        initial_notary_delay_millis: None,
        dkg_interval_length: None,
        dkg_dealings_per_block: None,
        max_artifact_streams_per_peer: None,
        max_chunk_wait_ms: None,
        max_duplicity: None,
        max_chunk_size: None,
        receive_check_cache_size: None,
        pfn_evaluation_period_ms: None,
        registry_poll_period_ms: None,
        retransmission_request_ms: None,
        advert_best_effort_percentage: None,
        set_gossip_config_to_default: false,
        start_as_nns: None,
        subnet_type: None,
        is_halted: None,
        max_instructions_per_message: None,
        max_instructions_per_round: None,
        max_instructions_per_install_code: None,
        features: None,
        ecdsa_config: None,
        max_number_of_canisters: None,
        ssh_readonly_access: readonly_keys,
        ssh_backup_access: backup_keys,
    }
}

async fn update_the_subnet_record(nns_endpoint: &IcEndpoint, payload: UpdateSubnetPayload) {
    let r = runtime_from_url(nns_endpoint.url.clone());
    let gov_can = get_governance_canister(&r);

    let proposal_id =
        submit_external_proposal_with_test_id(&gov_can, NnsFunction::UpdateConfigOfSubnet, payload)
            .await;

    vote_execute_proposal_assert_executed(&gov_can, proposal_id).await;
}

async fn fail_updating_the_subnet_record(nns_endpoint: &IcEndpoint, payload: UpdateSubnetPayload) {
    let r = runtime_from_url(nns_endpoint.url.clone());
    let gov_can = get_governance_canister(&r);

    let proposal_id =
        submit_external_proposal_with_test_id(&gov_can, NnsFunction::UpdateConfigOfSubnet, payload)
            .await;

    vote_execute_proposal_assert_failed(&gov_can, proposal_id, "too long").await;
}

fn get_updateunassignednodespayload(
    readonly_keys: Option<Vec<String>>,
) -> UpdateUnassignedNodesConfigPayload {
    UpdateUnassignedNodesConfigPayload {
        ssh_readonly_access: readonly_keys,
        replica_version: None,
    }
}

async fn update_ssh_keys_for_all_unassigned_nodes(
    nns_endpoint: &IcEndpoint,
    payload: UpdateUnassignedNodesConfigPayload,
) {
    let r = runtime_from_url(nns_endpoint.url.clone());
    let gov_can = get_governance_canister(&r);

    let proposal_id = submit_external_proposal_with_test_id(
        &gov_can,
        NnsFunction::UpdateUnassignedNodesConfig,
        payload,
    )
    .await;

    vote_execute_proposal_assert_executed(&gov_can, proposal_id).await;
}

async fn fail_updating_ssh_keys_for_all_unassigned_nodes(
    nns_endpoint: &IcEndpoint,
    payload: UpdateUnassignedNodesConfigPayload,
) {
    let r = runtime_from_url(nns_endpoint.url.clone());
    let gov_can = get_governance_canister(&r);

    let proposal_id = submit_external_proposal_with_test_id(
        &gov_can,
        NnsFunction::UpdateUnassignedNodesConfig,
        payload,
    )
    .await;

    vote_execute_proposal_assert_failed(&gov_can, proposal_id, "too long").await;
}

fn block_on<F>(f: F)
where
    F: Future<Output = ()>,
{
    let rt = tokio::runtime::Runtime::new().expect("Could not create tokio runtime.");
    rt.block_on(f);
}
