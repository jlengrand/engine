use std::borrow::Borrow;
use std::str::FromStr;

use rusoto_core::{Client, HttpClient, Region, RusotoError};
use rusoto_credential::StaticProvider;
use rusoto_ecr::{
    CreateRepositoryRequest, DescribeImagesRequest, DescribeRepositoriesError, DescribeRepositoriesRequest, Ecr,
    EcrClient, GetAuthorizationTokenRequest, ImageDetail, ImageIdentifier, PutLifecyclePolicyRequest, Repository,
};
use rusoto_sts::{GetCallerIdentityRequest, Sts, StsClient};

use crate::build_platform::Image;
use crate::cmd::command::QoveryCommand;
use crate::container_registry::docker::{docker_pull_image, docker_tag_and_push_image};
use crate::container_registry::{ContainerRegistry, Kind, PullResult, PushResult};
use crate::errors::{CommandError, EngineError};
use crate::events::{EngineEvent, EventMessage, ToTransmitter, Transmitter};
use crate::logger::{LogLevel, Logger};
use crate::models::{
    Context, Listen, Listener, Listeners, ListenersHelper, ProgressInfo, ProgressLevel, ProgressScope,
};
use crate::runtime::block_on;
use retry::delay::Fixed;
use retry::Error::Operation;
use retry::OperationResult;
use serde_json::json;

pub struct ECR {
    context: Context,
    id: String,
    name: String,
    access_key_id: String,
    secret_access_key: String,
    region: Region,
    listeners: Listeners,
    logger: Box<dyn Logger>,
}

impl ECR {
    pub fn new(
        context: Context,
        id: &str,
        name: &str,
        access_key_id: &str,
        secret_access_key: &str,
        region: &str,
        logger: Box<dyn Logger>,
    ) -> Self {
        ECR {
            context,
            id: id.to_string(),
            name: name.to_string(),
            access_key_id: access_key_id.to_string(),
            secret_access_key: secret_access_key.to_string(),
            region: Region::from_str(region).unwrap(),
            listeners: vec![],
            logger,
        }
    }

    pub fn credentials(&self) -> StaticProvider {
        StaticProvider::new(
            self.access_key_id.to_string(),
            self.secret_access_key.to_string(),
            None,
            None,
        )
    }

    pub fn client(&self) -> Client {
        Client::new_with(self.credentials(), HttpClient::new().unwrap())
    }

    pub fn ecr_client(&self) -> EcrClient {
        EcrClient::new_with_client(self.client(), self.region.clone())
    }

    fn get_repository(&self, image: &Image) -> Option<Repository> {
        let mut drr = DescribeRepositoriesRequest::default();
        drr.repository_names = Some(vec![image.name.to_string()]);

        let r = block_on(self.ecr_client().describe_repositories(drr));

        match r {
            Err(_) => None,
            Ok(res) => match res.repositories {
                // assume there is only one repository returned - why? Because we set only one repository_names above
                Some(repositories) => repositories.into_iter().next(),
                _ => None,
            },
        }
    }

    fn get_image(&self, image: &Image) -> Option<ImageDetail> {
        let mut dir = DescribeImagesRequest::default();
        dir.repository_name = image.name.to_string();

        let mut image_identifier = ImageIdentifier::default();
        image_identifier.image_tag = Some(image.tag.to_string());
        dir.image_ids = Some(vec![image_identifier]);

        let r = block_on(self.ecr_client().describe_images(dir));

        match r {
            Err(_) => None,
            Ok(res) => match res.image_details {
                // assume there is only one repository returned - why? Because we set only one repository_names above
                Some(image_details) => image_details.into_iter().next(),
                _ => None,
            },
        }
    }

    fn docker_envs(&self) -> Vec<(&str, &str)> {
        match self.context.docker_tcp_socket() {
            Some(tcp_socket) => vec![("DOCKER_HOST", tcp_socket.as_str())],
            None => vec![],
        }
    }

    fn push_image(&self, dest: String, dest_latest_tag: String, image: &Image) -> Result<PushResult, EngineError> {
        // READ https://docs.aws.amazon.com/AmazonECR/latest/userguide/docker-push-ecr-image.html
        // docker tag e9ae3c220b23 aws_account_id.dkr.ecr.region.amazonaws.com/my-web-app
        let event_details = self.get_event_details();

        match docker_tag_and_push_image(
            self.kind(),
            self.docker_envs(),
            &image,
            dest.clone(),
            dest_latest_tag,
            event_details.clone(),
            self.logger(),
        ) {
            Ok(_) => {
                let mut image = image.clone();
                image.registry_url = Some(dest);
                Ok(PushResult { image })
            }
            Err(e) => Err(EngineError::new_docker_push_image_error(
                event_details,
                image.name.to_string(),
                dest.to_string(),
                e,
            )),
        }
    }

    fn pull_image(&self, dest: String, image: &Image) -> Result<PullResult, EngineError> {
        // READ https://docs.aws.amazon.com/AmazonECR/latest/userguide/docker-pull-ecr-image.html
        // docker pull aws_account_id.dkr.ecr.us-west-2.amazonaws.com/amazonlinux:latest
        let event_details = self.get_event_details();

        match docker_pull_image(
            self.kind(),
            self.docker_envs(),
            dest.clone(),
            event_details.clone(),
            self.logger(),
        ) {
            Ok(_) => {
                let mut image = image.clone();
                image.registry_url = Some(dest);
                Ok(PullResult::Some(image))
            }
            Err(e) => Err(EngineError::new_docker_pull_image_error(
                event_details,
                image.name.to_string(),
                dest.to_string(),
                e,
            )),
        }
    }

    fn create_repository(&self, image: &Image) -> Result<Repository, EngineError> {
        let event_details = self.get_event_details();
        let repository_name = image.name.as_str();

        self.logger().log(
            LogLevel::Info,
            EngineEvent::Info(
                event_details.clone(),
                EventMessage::new_from_safe(format!("Creating ECR repository {}", &repository_name)),
            ),
        );

        let mut repo_creation_counter = 0;
        let container_registry_request = DescribeRepositoriesRequest {
            repository_names: Some(vec![repository_name.to_string()]),
            ..Default::default()
        };
        let crr = CreateRepositoryRequest {
            repository_name: repository_name.to_string(),
            ..Default::default()
        };

        // ensure repository is created
        // need to do all this checks and retry because of several issues encountered like: 200 API response code while repo is not created
        let repo_created = retry::retry(Fixed::from_millis(5000).take(24), || {
            match block_on(
                self.ecr_client()
                    .describe_repositories(container_registry_request.clone()),
            ) {
                Ok(x) => {
                    self.logger().log(
                        LogLevel::Debug,
                        EngineEvent::Debug(
                            event_details.clone(),
                            EventMessage::new_from_safe(format!("Created {:?} repository", x)),
                        ),
                    );
                    OperationResult::Ok(())
                }
                Err(e) => {
                    match e {
                        RusotoError::Service(s) => match s {
                            DescribeRepositoriesError::RepositoryNotFound(_) => {
                                if repo_creation_counter != 0 {
                                    self.logger().log(
                                        LogLevel::Warning,
                                        EngineEvent::Warning(
                                            event_details.clone(),
                                            EventMessage::new_from_safe(format!(
                                                "Repository {} was not found, {}x retrying...",
                                                &repository_name, &repo_creation_counter
                                            )),
                                        ),
                                    );
                                }
                                repo_creation_counter += 1;
                            }
                            _ => self.logger().log(
                                LogLevel::Warning,
                                EngineEvent::Warning(
                                    event_details.clone(),
                                    EventMessage::new(
                                        "Error while trying to create repository.".to_string(),
                                        Some(format!("{:?}", s)),
                                    ),
                                ),
                            ),
                        },
                        _ => self.logger().log(
                            LogLevel::Warning,
                            EngineEvent::Warning(
                                event_details.clone(),
                                EventMessage::new(
                                    "Error while trying to create repository.".to_string(),
                                    Some(format!("{:?}", e)),
                                ),
                            ),
                        ),
                    }

                    // TODO: This behavior is weird, returning an ok message saying repository has been created in an error ...
                    let msg = match block_on(self.ecr_client().create_repository(crr.clone())) {
                        Ok(_) => format!("repository {} created", &repository_name),
                        Err(err) => format!("{:?}", err),
                    };

                    OperationResult::Retry(Err(EngineError::new_container_registry_namespace_creation_error(
                        event_details.clone(),
                        repository_name.to_string(),
                        self.name_with_id(),
                        CommandError::new(msg.to_string(), Some("Can't create ECR repository".to_string())),
                    )))
                }
            }
        });

        match repo_created {
            Ok(_) => self.logger.log(
                LogLevel::Info,
                EngineEvent::Info(
                    event_details.clone(),
                    EventMessage::new_from_safe(format!(
                        "repository {} created after {} attempt(s)",
                        &repository_name, repo_creation_counter,
                    )),
                ),
            ),
            Err(Operation { error, .. }) => return error,
            Err(retry::Error::Internal(e)) => {
                return Err(EngineError::new_container_registry_namespace_creation_error(
                    event_details.clone(),
                    repository_name.to_string(),
                    self.name_with_id(),
                    CommandError::new_from_safe_message(e),
                ))
            }
        };

        // apply retention policy
        let retention_policy_in_days = match self.context.is_test_cluster() {
            true => 1,
            false => 365,
        };
        let lifecycle_policy_text = json!({
          "rules": [
            {
              "action": {
                "type": "expire"
              },
              "selection": {
                "countType": "sinceImagePushed",
                "countUnit": "days",
                "countNumber": retention_policy_in_days,
                "tagStatus": "any"
              },
              "description": "Images retention policy",
              "rulePriority": 1
            }
          ]
        });

        let plp = PutLifecyclePolicyRequest {
            repository_name: image.name.clone(),
            lifecycle_policy_text: lifecycle_policy_text.to_string(),
            ..Default::default()
        };

        match block_on(self.ecr_client().put_lifecycle_policy(plp)) {
            Err(err) => Err(
                EngineError::new_container_registry_repository_set_lifecycle_policy_error(
                    event_details.clone(),
                    repository_name.to_string(),
                    CommandError::new_from_safe_message(err.to_string()),
                ),
            ),
            _ => Ok(self.get_repository(image).expect("cannot get repository")),
        }
    }

    fn get_or_create_repository(&self, image: &Image) -> Result<Repository, EngineError> {
        let event_details = self.get_event_details();

        // check if the repository already exists
        let repository = self.get_repository(image);
        if repository.is_some() {
            self.logger.log(
                LogLevel::Info,
                EngineEvent::Info(
                    event_details.clone(),
                    EventMessage::new_from_safe(format!("ECR repository {} already exists", image.name.as_str())),
                ),
            );
            return Ok(repository.unwrap());
        }

        self.create_repository(image)
    }

    fn get_credentials(&self) -> Result<ECRCredentials, EngineError> {
        let event_details = self.get_event_details();
        let r = block_on(
            self.ecr_client()
                .get_authorization_token(GetAuthorizationTokenRequest::default()),
        );

        let (access_token, password, endpoint_url) = match r {
            Ok(t) => match t.authorization_data {
                Some(authorization_data) => {
                    let ad = authorization_data.first().unwrap();
                    let b64_token = ad.authorization_token.as_ref().unwrap();

                    let decoded_token = base64::decode(b64_token).unwrap();
                    let token = std::str::from_utf8(decoded_token.as_slice()).unwrap();

                    let s_token: Vec<&str> = token.split(':').collect::<Vec<_>>();

                    (
                        s_token.first().unwrap().to_string(),
                        s_token.get(1).unwrap().to_string(),
                        ad.clone().proxy_endpoint.unwrap(),
                    )
                }
                None => {
                    return Err(EngineError::new_container_registry_get_credentials_error(
                        event_details.clone(),
                        self.name_with_id(),
                    ));
                }
            },
            _ => {
                return Err(EngineError::new_container_registry_get_credentials_error(
                    event_details.clone(),
                    self.name_with_id(),
                ));
            }
        };

        Ok(ECRCredentials::new(access_token, password, endpoint_url))
    }

    fn exec_docker_login(&self) -> Result<(), EngineError> {
        let event_details = self.get_event_details();
        let credentials = self.get_credentials()?;

        let mut cmd = QoveryCommand::new(
            "docker",
            &vec![
                "login",
                "-u",
                credentials.access_token.as_str(),
                "-p",
                credentials.password.as_str(),
                credentials.endpoint_url.as_str(),
            ],
            &self.docker_envs(),
        );

        if let Err(_) = cmd.exec() {
            return Err(EngineError::new_client_invalid_cloud_provider_credentials(
                event_details.clone(),
            ));
        };

        Ok(())
    }
}

impl ToTransmitter for ECR {
    fn to_transmitter(&self) -> Transmitter {
        Transmitter::ContainerRegistry(self.id().to_string(), self.name().to_string())
    }
}

impl ContainerRegistry for ECR {
    fn context(&self) -> &Context {
        &self.context
    }

    fn kind(&self) -> Kind {
        Kind::Ecr
    }

    fn id(&self) -> &str {
        self.id.as_str()
    }

    fn name(&self) -> &str {
        self.name.as_str()
    }

    fn is_valid(&self) -> Result<(), EngineError> {
        let client = StsClient::new_with_client(self.client(), Region::default());
        let s = block_on(client.get_caller_identity(GetCallerIdentityRequest::default()));

        match s {
            Ok(_) => Ok(()),
            Err(_) => Err(EngineError::new_client_invalid_cloud_provider_credentials(
                self.get_event_details(),
            )),
        }
    }

    fn on_create(&self) -> Result<(), EngineError> {
        self.logger.log(
            LogLevel::Info,
            EngineEvent::Info(
                self.get_event_details(),
                EventMessage::new_from_safe("ECR.on_create() called".to_string()),
            ),
        );
        Ok(())
    }

    fn on_create_error(&self) -> Result<(), EngineError> {
        self.logger.log(
            LogLevel::Info,
            EngineEvent::Info(
                self.get_event_details(),
                EventMessage::new_from_safe("ECR.on_create_error() called".to_string()),
            ),
        );

        unimplemented!()
    }

    fn on_delete(&self) -> Result<(), EngineError> {
        self.logger.log(
            LogLevel::Info,
            EngineEvent::Info(
                self.get_event_details(),
                EventMessage::new_from_safe("ECR.on_delete() called".to_string()),
            ),
        );
        unimplemented!()
    }

    fn on_delete_error(&self) -> Result<(), EngineError> {
        self.logger.log(
            LogLevel::Info,
            EngineEvent::Info(
                self.get_event_details(),
                EventMessage::new_from_safe("ECR.on_delete_error() called".to_string()),
            ),
        );
        unimplemented!()
    }

    fn does_image_exists(&self, image: &Image) -> bool {
        self.get_image(image).is_some()
    }

    fn pull(&self, image: &Image) -> Result<PullResult, EngineError> {
        let event_details = self.get_event_details();
        let listeners_helper = ListenersHelper::new(&self.listeners);

        if !self.does_image_exists(image) {
            let info_message = format!(
                "image `{}` does not exist in ECR {} repository",
                image.name_with_tag(),
                self.name()
            );

            self.logger.log(
                LogLevel::Info,
                EngineEvent::Info(
                    event_details.clone(),
                    EventMessage::new_from_safe(info_message.to_string()),
                ),
            );

            listeners_helper.deployment_in_progress(ProgressInfo::new(
                ProgressScope::Application {
                    id: image.application_id.clone(),
                },
                ProgressLevel::Info,
                Some(info_message),
                self.context.execution_id(),
            ));

            return Ok(PullResult::None);
        }

        let info_message = format!(
            "pull image `{:?}` from ECR {} repository",
            image.name_with_tag(),
            self.name()
        );

        self.logger.log(
            LogLevel::Info,
            EngineEvent::Info(
                event_details.clone(),
                EventMessage::new_from_safe(info_message.to_string()),
            ),
        );

        listeners_helper.deployment_in_progress(ProgressInfo::new(
            ProgressScope::Application {
                id: image.application_id.clone(),
            },
            ProgressLevel::Info,
            Some(info_message),
            self.context.execution_id(),
        ));

        let _ = self.exec_docker_login()?;

        let repository = self.get_or_create_repository(image)?;

        let dest = format!("{}:{}", repository.repository_uri.unwrap(), image.tag.as_str());

        // pull image
        self.pull_image(dest, image)
    }

    fn push(&self, image: &Image, force_push: bool) -> Result<PushResult, EngineError> {
        let _ = self.exec_docker_login()?;

        let repository = if force_push {
            self.create_repository(image)
        } else {
            self.get_or_create_repository(image)
        }?;

        let repository_uri = repository.repository_uri.expect("Error getting repository URI");
        let dest = format!("{}:{}", repository_uri, image.tag.as_str());

        let listeners_helper = ListenersHelper::new(&self.listeners);

        if !force_push && self.does_image_exists(image) {
            // check if image does exist - if yes, do not upload it again
            let info_message = format!(
                "image {} found on ECR {} repository, container build is not required",
                image.name_with_tag(),
                self.name()
            );

            self.logger.log(
                LogLevel::Info,
                EngineEvent::Info(
                    self.get_event_details(),
                    EventMessage::new_from_safe(info_message.to_string()),
                ),
            );

            listeners_helper.deployment_in_progress(ProgressInfo::new(
                ProgressScope::Application {
                    id: image.application_id.clone(),
                },
                ProgressLevel::Info,
                Some(info_message),
                self.context.execution_id(),
            ));

            let mut image = image.clone();
            image.registry_url = Some(dest);

            return Ok(PushResult { image });
        }

        let info_message = format!(
            "image `{}` does not exist on ECR {} repository, starting image upload",
            image.name_with_tag(),
            self.name()
        );

        self.logger.log(
            LogLevel::Info,
            EngineEvent::Info(
                self.get_event_details(),
                EventMessage::new_from_safe(info_message.to_string()),
            ),
        );

        listeners_helper.deployment_in_progress(ProgressInfo::new(
            ProgressScope::Application {
                id: image.application_id.clone(),
            },
            ProgressLevel::Info,
            Some(info_message),
            self.context.execution_id(),
        ));

        let dest_latest_tag = format!("{}:latest", repository_uri);
        self.push_image(dest, dest_latest_tag, image)
    }

    fn push_error(&self, image: &Image) -> Result<PushResult, EngineError> {
        // TODO change this
        Ok(PushResult { image: image.clone() })
    }

    fn logger(&self) -> &dyn Logger {
        self.logger.borrow()
    }
}

impl Listen for ECR {
    fn listeners(&self) -> &Listeners {
        &self.listeners
    }

    fn add_listener(&mut self, listener: Listener) {
        self.listeners.push(listener);
    }
}

struct ECRCredentials {
    access_token: String,
    password: String,
    endpoint_url: String,
}

impl ECRCredentials {
    fn new(access_token: String, password: String, endpoint_url: String) -> Self {
        ECRCredentials {
            access_token,
            password,
            endpoint_url,
        }
    }
}
