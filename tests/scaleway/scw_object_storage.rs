pub use crate::helpers::utilities::{context, generate_id, FuncTestsSecrets};
pub use qovery_engine::cloud_provider::scaleway::application::Zone;
pub use qovery_engine::object_storage::scaleway_object_storage::{BucketDeleteStrategy, ScalewayOS};
pub use qovery_engine::object_storage::ObjectStorage;
pub use tempfile::NamedTempFile;

pub const TEST_ZONE: Zone = Zone::Paris1;

#[cfg(feature = "test-scw-infra")]
#[test]
fn test_delete_bucket_hard_delete_strategy() {
    // setup:
    let context = context();
    let secrets = FuncTestsSecrets::new();
    let scw_access_key = secrets.SCALEWAY_ACCESS_KEY.unwrap_or_else(|| "undefined".to_string());
    let scw_secret_key = secrets.SCALEWAY_SECRET_KEY.unwrap_or_else(|| "undefined".to_string());

    let _test = "";

    let scaleway_os = ScalewayOS::new(
        context,
        generate_id(),
        "test".to_string(),
        scw_access_key,
        scw_secret_key,
        TEST_ZONE,
        BucketDeleteStrategy::HardDelete,
        false,
    );

    let bucket_name = format!("qovery-test-bucket-{}", generate_id());

    scaleway_os
        .create_bucket(bucket_name.as_str())
        .expect("error while creating object-storage bucket");

    // compute:
    let result = scaleway_os.delete_bucket(bucket_name.as_str());

    // validate:
    assert!(result.is_ok());
    assert!(!scaleway_os.bucket_exists(bucket_name.as_str()))
}

#[cfg(feature = "test-scw-infra")]
#[test]
fn test_delete_bucket_empty_strategy() {
    // setup:
    let context = context();
    let secrets = FuncTestsSecrets::new();
    let scw_access_key = secrets.SCALEWAY_ACCESS_KEY.unwrap_or_else(|| "undefined".to_string());
    let scw_secret_key = secrets.SCALEWAY_SECRET_KEY.unwrap_or_else(|| "undefined".to_string());

    let scaleway_os = ScalewayOS::new(
        context,
        generate_id(),
        "test".to_string(),
        scw_access_key,
        scw_secret_key,
        TEST_ZONE,
        BucketDeleteStrategy::Empty,
        false,
    );

    let bucket_name = format!("qovery-test-bucket-{}", generate_id());

    scaleway_os
        .create_bucket(bucket_name.as_str())
        .expect("error while creating object-storage bucket");

    // compute:
    let result = scaleway_os.delete_bucket(bucket_name.as_str());

    // validate:
    assert!(result.is_ok());
    assert!(scaleway_os.bucket_exists(bucket_name.as_str()));

    // clean-up:
    scaleway_os
        .delete_bucket(bucket_name.as_str())
        .unwrap_or_else(|_| panic!("error deleting object storage bucket {}", bucket_name));
}

#[cfg(feature = "test-scw-infra")]
#[test]
fn test_create_bucket() {
    // setup:
    let context = context();
    let secrets = FuncTestsSecrets::new();
    let scw_access_key = secrets.SCALEWAY_ACCESS_KEY.unwrap_or_else(|| "undefined".to_string());
    let scw_secret_key = secrets.SCALEWAY_SECRET_KEY.unwrap_or_else(|| "undefined".to_string());

    let scaleway_os = ScalewayOS::new(
        context,
        generate_id(),
        "test".to_string(),
        scw_access_key,
        scw_secret_key,
        TEST_ZONE,
        BucketDeleteStrategy::HardDelete,
        false,
    );

    let bucket_name = format!("qovery-test-bucket-{}", generate_id());

    // compute:
    let result = scaleway_os.create_bucket(bucket_name.as_str());

    // validate:
    assert!(result.is_ok());

    // clean-up:
    scaleway_os
        .delete_bucket(bucket_name.as_str())
        .unwrap_or_else(|_| panic!("error deleting object storage bucket {}", bucket_name));
}

#[cfg(feature = "test-scw-infra")]
#[test]
fn test_put_file() {
    // setup:
    let context = context();
    let secrets = FuncTestsSecrets::new();
    let scw_access_key = secrets.SCALEWAY_ACCESS_KEY.unwrap_or_else(|| "undefined".to_string());
    let scw_secret_key = secrets.SCALEWAY_SECRET_KEY.unwrap_or_else(|| "undefined".to_string());

    let scaleway_os = ScalewayOS::new(
        context,
        generate_id(),
        "test".to_string(),
        scw_access_key,
        scw_secret_key,
        TEST_ZONE,
        BucketDeleteStrategy::HardDelete,
        false,
    );

    let bucket_name = format!("qovery-test-bucket-{}", generate_id());
    let object_key = format!("test-object-{}", generate_id());

    scaleway_os
        .create_bucket(bucket_name.as_str())
        .expect("error while creating object-storage bucket");

    let temp_file = NamedTempFile::new().expect("error while creating tempfile");

    // compute:
    let result = scaleway_os.put(
        bucket_name.as_str(),
        object_key.as_str(),
        temp_file.into_temp_path().to_str().unwrap(),
    );

    // validate:
    assert!(result.is_ok());
    assert!(scaleway_os
        .get(bucket_name.as_str(), object_key.as_str(), false)
        .is_ok());

    // clean-up:
    scaleway_os
        .delete_bucket(bucket_name.as_str())
        .unwrap_or_else(|_| panic!("error deleting object storage bucket {}", bucket_name));
}

#[cfg(feature = "test-scw-infra")]
#[test]
fn test_get_file() {
    // setup:
    let context = context();
    let secrets = FuncTestsSecrets::new();
    let scw_access_key = secrets.SCALEWAY_ACCESS_KEY.unwrap_or_else(|| "undefined".to_string());
    let scw_secret_key = secrets.SCALEWAY_SECRET_KEY.unwrap_or_else(|| "undefined".to_string());

    let scaleway_os = ScalewayOS::new(
        context,
        generate_id(),
        "test".to_string(),
        scw_access_key,
        scw_secret_key,
        TEST_ZONE,
        BucketDeleteStrategy::HardDelete,
        false,
    );

    let bucket_name = format!("qovery-test-bucket-{}", generate_id());
    let object_key = format!("test-object-{}", generate_id());

    scaleway_os
        .create_bucket(bucket_name.as_str())
        .expect("error while creating object-storage bucket");

    let temp_file = NamedTempFile::new().expect("error while creating tempfile");
    let tempfile_path = temp_file.into_temp_path();
    let tempfile_path = tempfile_path.to_str().unwrap();

    scaleway_os
        .put(bucket_name.as_str(), object_key.as_str(), tempfile_path)
        .unwrap_or_else(|_| panic!("error while putting file {} into bucket {}", tempfile_path, bucket_name));

    // compute:
    let result = scaleway_os.get(bucket_name.as_str(), object_key.as_str(), false);

    // validate:
    assert!(result.is_ok());
    assert!(scaleway_os
        .get(bucket_name.as_str(), object_key.as_str(), false)
        .is_ok());

    // clean-up:
    scaleway_os
        .delete_bucket(bucket_name.as_str())
        .unwrap_or_else(|_| panic!("error deleting object storage bucket {}", bucket_name));
}
