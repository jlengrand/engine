use qovery_engine::cloud_provider::scaleway::application::Zone;
use qovery_engine::cloud_provider::scaleway::kubernetes::node::{Node, NodeType};
use qovery_engine::cloud_provider::scaleway::kubernetes::{Kapsule, KapsuleOptions};
use qovery_engine::cloud_provider::scaleway::Scaleway;
use qovery_engine::cloud_provider::TerraformStateCredentials;
use qovery_engine::container_registry::scaleway_container_registry::ScalewayCR;
use qovery_engine::dns_provider::DnsProvider;
use qovery_engine::engine::Engine;
use qovery_engine::models::Context;
use qovery_engine::object_storage::scaleway_object_storage::{BucketDeleteStrategy, ScalewayOS};

use crate::cloudflare::dns_provider_cloudflare;
use crate::utilities::{build_platform_local_docker, generate_id, FuncTestsSecrets};

use std::str::FromStr;
use tracing::error;

pub const SCW_QOVERY_ORGANIZATION_ID: &str = "zslbgcfanc6r2nbs";
pub const SCW_KUBE_TEST_CLUSTER_NAME: &str = "qovery-zb3a2b3b8";
pub const SCW_KUBE_TEST_CLUSTER_ID: &str = "zb3a2b3b8";
pub const SCW_TEST_ZONE: Zone = Zone::Paris2;
pub const SCW_KUBERNETES_VERSION: &str = "1.18";
pub const SCW_DATABASE_INSTANCE_TYPE: &str = "not-used";
pub const SCW_DATABASE_DISK_TYPE: &str = "scw-sbv-ssd-0";

pub fn container_registry_scw(context: &Context) -> ScalewayCR {
    let secrets = FuncTestsSecrets::new();
    if secrets.SCALEWAY_ACCESS_KEY.is_none()
        || secrets.SCALEWAY_SECRET_KEY.is_none()
        || secrets.SCALEWAY_DEFAULT_PROJECT_ID.is_none()
    {
        error!("Please check your Vault connectivity (token/address) or SCALEWAY_ACCESS_KEY/SCALEWAY_SECRET_KEY/SCALEWAY_DEFAULT_PROJECT_ID envrionment variables are set");
        std::process::exit(1)
    }
    let random_id = generate_id();
    let scw_secret_key = secrets
        .SCALEWAY_SECRET_KEY
        .expect("SCALEWAY_SECRET_KEY is not set in secrets");
    let scw_default_project_id = secrets
        .SCALEWAY_DEFAULT_PROJECT_ID
        .expect("SCALEWAY_DEFAULT_PROJECT_ID is not set in secrets");

    ScalewayCR::new(
        context.clone(),
        format!("default-registry-qovery-test-{}", random_id.clone()).as_str(),
        format!("default-registry-qovery-test-{}", random_id.clone()).as_str(),
        scw_secret_key.as_str(),
        scw_default_project_id.as_str(),
        SCW_TEST_ZONE,
    )
}

pub fn cloud_provider_scaleway(context: &Context) -> Scaleway {
    let secrets = FuncTestsSecrets::new();

    Scaleway::new(
        context.clone(),
        SCW_KUBE_TEST_CLUSTER_ID,
        SCW_QOVERY_ORGANIZATION_ID,
        SCW_KUBE_TEST_CLUSTER_NAME,
        secrets
            .SCALEWAY_ACCESS_KEY
            .expect("SCALEWAY_ACCESS_KEY is not set in secrets")
            .as_str(),
        secrets
            .SCALEWAY_SECRET_KEY
            .expect("SCALEWAY_SECRET_KEY is not set in secrets")
            .as_str(),
        TerraformStateCredentials {
            access_key_id: secrets
                .TERRAFORM_AWS_ACCESS_KEY_ID
                .expect("TERRAFORM_AWS_ACCESS_KEY_ID is not set in secrets"),
            secret_access_key: secrets
                .TERRAFORM_AWS_SECRET_ACCESS_KEY
                .expect("TERRAFORM_AWS_SECRET_ACCESS_KEY is not set in secrets"),
            region: "eu-west-3".to_string(),
        },
    )
}

pub fn scw_kubernetes_cluster_options(secrets: FuncTestsSecrets) -> KapsuleOptions {
    KapsuleOptions::new(
        secrets.QOVERY_API_URL.expect("QOVERY_API_URL is not set in secrets"),
        secrets.QOVERY_NATS_URL.expect("QOVERY_NATS_URL is not set in secrets"),
        secrets
            .QOVERY_NATS_USERNAME
            .expect("QOVERY_NATS_USERNAME is not set in secrets"),
        secrets
            .QOVERY_NATS_PASSWORD
            .expect("QOVERY_NATS_PASSWORD is not set in secrets"),
        secrets.QOVERY_SSH_USER.expect("QOVERY_SSH_USER is not set in secrets"),
        "admin".to_string(),
        "qovery".to_string(),
        secrets
            .QOVERY_AGENT_CONTROLLER_TOKEN
            .expect("QOVERY_AGENT_CONTROLLER_TOKEN is not set in secrets"),
        secrets
            .QOVERY_ENGINE_CONTROLLER_TOKEN
            .expect("QOVERY_ENGINE_CONTROLLER_TOKEN is not set in secrets"),
        secrets
            .SCALEWAY_DEFAULT_PROJECT_ID
            .expect("SCALEWAY_DEFAULT_PROJECT_ID is not set in secrets"),
        secrets
            .SCALEWAY_ACCESS_KEY
            .expect("SCALEWAY_ACCESS_KEY is not set in secrets"),
        secrets
            .SCALEWAY_SECRET_KEY
            .expect("SCALEWAY_SECRET_KEY is not set in secrets"),
        secrets
            .LETS_ENCRYPT_EMAIL_REPORT
            .expect("LETS_ENCRYPT_EMAIL_REPORT is not set in secrets"),
    )
}

pub fn scw_object_storage(context: Context, region: Zone) -> ScalewayOS {
    let secrets = FuncTestsSecrets::new();
    let random_id = generate_id();

    ScalewayOS::new(
        context,
        format!("qovery-test-object-storage-{}", random_id.clone()),
        format!("Qovery Test Object-Storage {}", random_id),
        secrets
            .SCALEWAY_ACCESS_KEY
            .expect("SCALEWAY_ACCESS_KEY is not set in secrets"),
        secrets
            .SCALEWAY_SECRET_KEY
            .expect("SCALEWAY_SECRET_KEY is not set in secrets"),
        region,
        BucketDeleteStrategy::Empty, // do not delete bucket due to deletion 24h delay
        false,
    )
}

pub fn scw_kubernetes_nodes() -> Vec<Node> {
    // Note: Dev1M is a bit too small to handle engine + local docker, hence using Dev1L
    scw_kubernetes_custom_nodes(10, NodeType::Dev1L)
}

pub fn scw_kubernetes_custom_nodes(count: usize, node_type: NodeType) -> Vec<Node> {
    vec![Node::new(node_type); count]
}

pub fn docker_scw_cr_engine(context: &Context) -> Engine {
    // use Scaleway CR
    let container_registry = Box::new(container_registry_scw(context));

    // use LocalDocker
    let build_platform = Box::new(build_platform_local_docker(context));

    // use Scaleway
    let cloud_provider = Box::new(cloud_provider_scaleway(context));

    let dns_provider = Box::new(dns_provider_cloudflare(context));

    Engine::new(
        context.clone(),
        build_platform,
        container_registry,
        cloud_provider,
        dns_provider,
    )
}

pub fn scw_kubernetes_kapsule<'a>(
    context: &Context,
    cloud_provider: &'a Scaleway,
    dns_provider: &'a dyn DnsProvider,
    nodes: Vec<Node>,
) -> Kapsule<'a> {
    let secrets = FuncTestsSecrets::new();
    Kapsule::<'a>::new(
        context.clone(),
        SCW_KUBE_TEST_CLUSTER_ID.to_string(),
        SCW_KUBE_TEST_CLUSTER_NAME.to_string(),
        SCW_KUBERNETES_VERSION.to_string(),
        Zone::from_str(
            secrets
                .clone()
                .SCALEWAY_DEFAULT_REGION
                .expect("SCALEWAY_DEFAULT_REGION is not set in secrets")
                .as_str(),
        )
        .unwrap(),
        cloud_provider,
        dns_provider,
        nodes,
        scw_kubernetes_cluster_options(secrets),
    )
}
