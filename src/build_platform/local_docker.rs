use std::path::Path;
use std::{env, fs};

use chrono::Duration;
use git2::{Cred, CredentialType};
use sysinfo::{Disk, DiskExt, SystemExt};

use crate::build_platform::{docker, Build, BuildPlatform, BuildResult, CacheResult, Credentials, Image, Kind};
use crate::cmd::command::CommandError::Killed;
use crate::cmd::command::QoveryCommand;
use crate::errors::{CommandError, EngineError, Tag};
use crate::events::{EngineEvent, EventDetails, EventMessage, ToTransmitter, Transmitter};
use crate::fs::workspace_directory;
use crate::git;
use crate::logger::{LogLevel, Logger};
use crate::models::{
    Context, Listen, Listener, Listeners, ListenersHelper, ProgressInfo, ProgressLevel, ProgressScope,
};

const BUILD_DURATION_TIMEOUT_MIN: i64 = 30;

/// https://buildpacks.io/
const BUILDPACKS_BUILDERS: [&str; 1] = [
    "heroku/buildpacks:20",
    // removed because it does not support dynamic port binding
    //"gcr.io/buildpacks/builder:v1",
    //"paketobuildpacks/builder:base",
];

/// use Docker in local
pub struct LocalDocker {
    context: Context,
    id: String,
    name: String,
    listeners: Listeners,
    logger: Box<dyn Logger>,
}

impl LocalDocker {
    pub fn new(context: Context, id: &str, name: &str, logger: Box<dyn Logger>) -> Self {
        LocalDocker {
            context,
            id: id.to_string(),
            name: name.to_string(),
            listeners: vec![],
            logger,
        }
    }

    fn image_does_exist(&self, image: &Image) -> Result<bool, EngineError> {
        let mut cmd = QoveryCommand::new(
            "docker",
            &vec!["image", "inspect", image.name_with_tag().as_str()],
            &self.get_docker_host_envs(),
        );

        Ok(matches!(cmd.exec(), Ok(_)))
    }

    fn get_docker_host_envs(&self) -> Vec<(&str, &str)> {
        match self.context.docker_tcp_socket() {
            Some(tcp_socket) => vec![("DOCKER_HOST", tcp_socket.as_str())],
            None => vec![],
        }
    }

    /// Read Dockerfile content from location path and return an array of bytes
    fn get_dockerfile_content(&self, dockerfile_path: &str) -> Result<Vec<u8>, EngineError> {
        match fs::read(dockerfile_path) {
            Ok(bytes) => Ok(bytes),
            Err(err) => {
                let engine_error = EngineError::new_docker_cannot_read_dockerfile(
                    self.get_event_details(),
                    dockerfile_path.to_string(),
                    CommandError::new(err.to_string(), None),
                );
                self.logger
                    .log(LogLevel::Error, EngineEvent::Error(engine_error.clone(), None));
                Err(engine_error)
            }
        }
    }

    fn build_image_with_docker(
        &self,
        build: Build,
        dockerfile_complete_path: &str,
        into_dir_docker_style: &str,
        env_var_args: Vec<String>,
        use_build_cache: bool,
        lh: &ListenersHelper,
        is_task_canceled: &dyn Fn() -> bool,
    ) -> Result<BuildResult, EngineError> {
        let mut docker_args = if !use_build_cache {
            vec!["build", "--no-cache"]
        } else {
            vec!["build"]
        };

        let args = self.context.docker_build_options();
        for v in args.iter() {
            for s in v.iter() {
                docker_args.push(String::as_str(s));
            }
        }

        let name_with_tag = build.image.name_with_tag();
        let name_with_latest_tag = build.image.name_with_latest_tag();

        docker_args.extend(vec![
            "-f",
            dockerfile_complete_path,
            "-t",
            name_with_tag.as_str(),
            "-t",
            name_with_latest_tag.as_str(),
        ]);

        let dockerfile_content = self.get_dockerfile_content(dockerfile_complete_path)?;
        let env_var_args = match docker::match_used_env_var_args(env_var_args, dockerfile_content) {
            Ok(env_var_args) => env_var_args,
            Err(err) => {
                let engine_error = EngineError::new_docker_cannot_extract_env_vars_from_dockerfile(
                    self.get_event_details(),
                    dockerfile_complete_path.to_string(),
                    CommandError::new(err.to_string(), None),
                );
                self.logger
                    .log(LogLevel::Error, EngineEvent::Error(engine_error.clone(), None));
                return Err(engine_error);
            }
        };

        let mut docker_args = if env_var_args.is_empty() {
            docker_args
        } else {
            let mut build_args = vec![];

            env_var_args.iter().for_each(|arg_value| {
                build_args.push("--build-arg");
                build_args.push(arg_value.as_str());
            });

            docker_args.extend(build_args);
            docker_args
        };

        docker_args.push(into_dir_docker_style);

        // docker build
        let mut cmd = QoveryCommand::new("docker", &docker_args, &self.get_docker_host_envs());

        let exit_status = cmd.exec_with_abort(
            Duration::minutes(BUILD_DURATION_TIMEOUT_MIN),
            |line| {
                self.logger.log(
                    LogLevel::Info,
                    EngineEvent::Info(self.get_event_details(), EventMessage::new_from_safe(line.to_string())),
                );

                lh.deployment_in_progress(ProgressInfo::new(
                    ProgressScope::Application {
                        id: build.image.application_id.clone(),
                    },
                    ProgressLevel::Info,
                    Some(line),
                    self.context.execution_id(),
                ));
            },
            |line| {
                self.logger.log(
                    LogLevel::Warning,
                    EngineEvent::Warning(self.get_event_details(), EventMessage::new_from_safe(line.to_string())),
                );

                lh.deployment_in_progress(ProgressInfo::new(
                    ProgressScope::Application {
                        id: build.image.application_id.clone(),
                    },
                    ProgressLevel::Warn,
                    Some(line),
                    self.context.execution_id(),
                ));
            },
            is_task_canceled,
        );

        match exit_status {
            Ok(_) => Ok(BuildResult { build }),
            Err(Killed(_)) => Err(EngineError::new_task_cancellation_requested(self.get_event_details())),
            Err(err) => Err(EngineError::new_docker_cannot_build_container_image(
                self.get_event_details(),
                self.name_with_id(),
                CommandError::new(format!("{:?}", err), None),
            )),
        }
    }

    fn build_image_with_buildpacks(
        &self,
        build: Build,
        into_dir_docker_style: &str,
        env_var_args: Vec<String>,
        use_build_cache: bool,
        lh: &ListenersHelper,
        is_task_canceled: &dyn Fn() -> bool,
    ) -> Result<BuildResult, EngineError> {
        let name_with_tag = build.image.name_with_tag();

        let args = self.context.docker_build_options();

        let mut exit_status: Result<(), CommandError> =
            Err(CommandError::new_from_safe_message("No builder names".to_string()));

        for builder_name in BUILDPACKS_BUILDERS.iter() {
            let mut buildpacks_args = if !use_build_cache {
                vec!["build", name_with_tag.as_str(), "--clear-cache"]
            } else {
                vec!["build", name_with_tag.as_str()]
            };

            for v in args.iter() {
                for s in v.iter() {
                    buildpacks_args.push(String::as_str(s));
                }
            }

            buildpacks_args.extend(vec!["--path", into_dir_docker_style]);

            let mut buildpacks_args = if env_var_args.is_empty() {
                buildpacks_args
            } else {
                let mut build_args = vec![];

                env_var_args.iter().for_each(|x| {
                    build_args.push("--env");
                    build_args.push(x.as_str());
                });

                buildpacks_args.extend(build_args);
                buildpacks_args
            };

            buildpacks_args.push("-B");
            buildpacks_args.push(builder_name);
            if let Some(buildpacks_language) = &build.git_repository.buildpack_language {
                buildpacks_args.push("-b");
                match buildpacks_language.split('@').collect::<Vec<&str>>().as_slice() {
                    [builder] => {
                        // no version specified, so we use the latest builder
                        buildpacks_args.push(builder);
                    }
                    [builder, _version] => {
                        // version specified, we need to use the specified builder
                        // but also ensure that the user has set the correct runtime version in his project
                        // this is language dependent
                        // https://elements.heroku.com/buildpacks/heroku/heroku-buildpack-python
                        // https://devcenter.heroku.com/articles/buildpacks
                        // TODO: Check user project is correctly configured for this builder and version
                        buildpacks_args.push(builder);
                    }
                    _ => {
                        let msg = format!(
                            "Cannot build: Invalid buildpacks language format: expected `builder[@version]` got {}",
                            buildpacks_language
                        );
                        lh.deployment_error(ProgressInfo::new(
                            ProgressScope::Application {
                                id: build.image.application_id.clone(),
                            },
                            ProgressLevel::Error,
                            Some(msg.clone()),
                            self.context.execution_id(),
                        ));

                        let err = EngineError::new_buildpack_invalid_language_format(
                            self.get_event_details(),
                            buildpacks_language.to_string(),
                        );

                        self.logger.log(LogLevel::Error, EngineEvent::Error(err.clone(), None));

                        return Err(err);
                    }
                }
            }

            // Just a fallback for now to help our bot loving users deploy their apps
            // Long term solution requires lots of changes in UI and Core as well
            // And passing some params to the engine
            if let Ok(content) = fs::read_to_string(format!("{}/{}", into_dir_docker_style, "Procfile")) {
                if content.contains("worker") {
                    buildpacks_args.push("--default-process");
                    buildpacks_args.push("worker");
                }
            }

            // buildpacks build
            let mut cmd = QoveryCommand::new("pack", &buildpacks_args, &self.get_docker_host_envs());
            exit_status = cmd
                .exec_with_abort(
                    Duration::minutes(BUILD_DURATION_TIMEOUT_MIN),
                    |line| {
                        self.logger.log(
                            LogLevel::Info,
                            EngineEvent::Info(self.get_event_details(), EventMessage::new_from_safe(line.to_string())),
                        );

                        lh.deployment_in_progress(ProgressInfo::new(
                            ProgressScope::Application {
                                id: build.image.application_id.clone(),
                            },
                            ProgressLevel::Info,
                            Some(line),
                            self.context.execution_id(),
                        ));
                    },
                    |line| {
                        self.logger.log(
                            LogLevel::Warning,
                            EngineEvent::Warning(
                                self.get_event_details(),
                                EventMessage::new_from_safe(line.to_string()),
                            ),
                        );

                        lh.deployment_in_progress(ProgressInfo::new(
                            ProgressScope::Application {
                                id: build.image.application_id.clone(),
                            },
                            ProgressLevel::Warn,
                            Some(line),
                            self.context.execution_id(),
                        ));
                    },
                    is_task_canceled,
                )
                .map_err(|err| CommandError::new(format!("{:?}", err), None));

            if exit_status.is_ok() {
                // quit now if the builder successfully build the app
                break;
            }
        }

        match exit_status {
            Ok(_) => Ok(BuildResult { build }),
            Err(Killed(_)) => Err(EngineError::new_task_cancellation_requested(self.get_event_details())),
            Err(err) => {
                let error = EngineError::new_buildpack_cannot_build_container_image(
                    self.get_event_details(),
                    self.name_with_id(),
                    BUILDPACKS_BUILDERS.iter().map(|b| b.to_string()).collect(),
                    CommandError::new(format!("{:?}", err), None),
                );

                self.logger
                    .log(LogLevel::Error, EngineEvent::Error(error.clone(), None));

                Err(error)
            }
        }
    }

    fn get_repository_build_root_path(&self, build: &Build) -> Result<String, EngineError> {
        workspace_directory(
            self.context.workspace_root_dir(),
            self.context.execution_id(),
            format!("build/{}", build.image.name.as_str()),
        )
        .map_err(|err| {
            EngineError::new_cannot_get_workspace_directory(
                self.get_event_details(),
                CommandError::new(err.to_string(), None),
            )
        })
    }
}

impl BuildPlatform for LocalDocker {
    fn context(&self) -> &Context {
        &self.context
    }

    fn kind(&self) -> Kind {
        Kind::LocalDocker
    }

    fn id(&self) -> &str {
        self.id.as_str()
    }

    fn name(&self) -> &str {
        self.name.as_str()
    }

    fn is_valid(&self) -> Result<(), EngineError> {
        if !crate::cmd::command::does_binary_exist("docker") {
            return Err(EngineError::new_missing_required_binary(
                self.get_event_details(),
                "docker".to_string(),
            ));
        }

        if !crate::cmd::command::does_binary_exist("pack") {
            return Err(EngineError::new_missing_required_binary(
                self.get_event_details(),
                "pack".to_string(),
            ));
        }

        Ok(())
    }

    fn has_cache(&self, build: &Build) -> Result<CacheResult, EngineError> {
        info!("LocalDocker.has_cache() called for {}", self.name());

        // Check if a local cache layers for the container image exists.
        let repository_root_path = self.get_repository_build_root_path(&build)?;

        let parent_build = build.to_previous_build(repository_root_path).map_err(|err| {
            EngineError::new_builder_get_build_error(self.get_event_details(), build.image.commit_id.to_string(), err)
        })?;

        let parent_build = match parent_build {
            Some(parent_build) => parent_build,
            None => return Ok(CacheResult::MissWithoutParentBuild),
        };

        // check if local layers exist
        let mut cmd = QoveryCommand::new("docker", &["images", "-q", parent_build.image.name.as_str()], &[]);

        let mut result = CacheResult::Miss(parent_build);
        let _ = cmd.exec_with_timeout(
            Duration::minutes(1), // `docker images` command can be slow with tons of images - it's probably not indexed
            |_| result = CacheResult::Hit, // if a line is returned, then the image is locally present
            |r_err| error!("Error executing docker command {}", r_err),
        );

        Ok(result)
    }

    fn build(
        &self,
        build: Build,
        force_build: bool,
        is_task_canceled: &dyn Fn() -> bool,
    ) -> Result<BuildResult, EngineError> {
        let event_details = self.get_event_details();

        self.logger.log(
            LogLevel::Info,
            EngineEvent::Info(
                event_details.clone(),
                EventMessage::new_from_safe("LocalDocker.build() called".to_string()),
            ),
        );

        if is_task_canceled() {
            return Err(EngineError::new_task_cancellation_requested(event_details.clone()));
        }

        let listeners_helper = ListenersHelper::new(&self.listeners);

        if !force_build && self.image_does_exist(&build.image)? {
            self.logger.log(
                LogLevel::Info,
                EngineEvent::Info(
                    event_details.clone(),
                    EventMessage::new_from_safe(format!(
                        "Image `{}` found on repository, container build is not required",
                        build.image.name_with_tag()
                    )),
                ),
            );

            return Ok(BuildResult { build });
        }

        let repository_root_path = self.get_repository_build_root_path(&build)?;

        self.logger.log(
            LogLevel::Info,
            EngineEvent::Info(
                event_details.clone(),
                EventMessage::new_from_safe(format!(
                    "Cloning repository: {} to {}",
                    build.git_repository.url, repository_root_path
                )),
            ),
        );

        let get_credentials = |user: &str| {
            let mut creds: Vec<(CredentialType, Cred)> = Vec::with_capacity(build.git_repository.ssh_keys.len() + 1);
            for ssh_key in build.git_repository.ssh_keys.iter() {
                let public_key = ssh_key.public_key.as_ref().map(|x| x.as_str());
                let passphrase = ssh_key.passphrase.as_ref().map(|x| x.as_str());
                if let Ok(cred) = Cred::ssh_key_from_memory(user, public_key, &ssh_key.private_key, passphrase) {
                    creds.push((CredentialType::SSH_MEMORY, cred));
                }
            }

            if let Some(Credentials { login, password }) = &build.git_repository.credentials {
                creds.push((
                    CredentialType::USER_PASS_PLAINTEXT,
                    Cred::userpass_plaintext(&login, &password).unwrap(),
                ));
            }

            creds
        };

        if Path::new(repository_root_path.as_str()).exists() {
            // remove folder before cloning it again
            // FIXME: reuse this folder and checkout the right commit
            let _ = fs::remove_dir_all(repository_root_path.as_str());
        }

        // git clone
        if is_task_canceled() {
            return Err(EngineError::new_task_cancellation_requested(event_details.clone()));
        }
        if let Err(clone_error) = git::clone_at_commit(
            &build.git_repository.url,
            &build.git_repository.commit_id,
            &repository_root_path,
            &get_credentials,
        ) {
            let error = EngineError::new_builder_clone_repository_error(
                self.get_event_details(),
                build.git_repository.url.to_string(),
                CommandError::new(clone_error.to_string(), None),
            );

            self.logger
                .log(LogLevel::Error, EngineEvent::Error(error.clone(), None));

            return Err(error);
        }

        let mut disable_build_cache = false;
        let mut env_var_args: Vec<String> = Vec::with_capacity(build.options.environment_variables.len());

        for ev in &build.options.environment_variables {
            if ev.key == "QOVERY_DISABLE_BUILD_CACHE" && ev.value.to_lowercase() == "true" {
                // this is a special flag to disable build cache dynamically
                // -- do not pass this env var key/value to as build parameter
                disable_build_cache = true;
            } else {
                env_var_args.push(format!("{}={}", ev.key, ev.value));
            }
        }

        // ensure docker_path is a mounted volume, otherwise ignore because it's not what Qovery does in production
        // ex: this cause regular cleanup on CI, leading to random tests errors
        match env::var_os("CI") {
            Some(_) => self.logger.log(
                LogLevel::Info,
                EngineEvent::Info(
                    event_details.clone(),
                    EventMessage::new_from_safe(
                        "CI environment variable found, no docker prune will be made".to_string(),
                    ),
                ),
            ),
            None => {
                // ensure there is enough disk space left before building a new image
                let docker_path_string = "/var/lib/docker";
                let docker_path = Path::new(docker_path_string);

                // get system info
                let mut system = sysinfo::System::new_all();
                system.refresh_all();

                for disk in system.get_disks() {
                    if disk.get_mount_point() == docker_path {
                        let event_details = self.get_event_details();
                        if let Err(e) = check_docker_space_usage_and_clean(
                            disk,
                            self.get_docker_host_envs(),
                            event_details.clone(),
                            &*self.logger(),
                        ) {
                            self.logger.log(
                                LogLevel::Warning,
                                EngineEvent::Warning(
                                    event_details.clone(),
                                    EventMessage::new(e.message_raw(), e.message_safe()),
                                ),
                            );
                        }
                        break;
                    };
                }
            }
        }

        let app_id = build.image.application_id.clone();
        let build_context_path = format!("{}/{}/.", repository_root_path.as_str(), build.git_repository.root_path);
        // If no Dockerfile specified, we should use BuildPacks
        let result = if build.git_repository.dockerfile_path.is_some() {
            // build container from the provided Dockerfile

            let dockerfile_relative_path = build.git_repository.dockerfile_path.as_ref().unwrap();
            let dockerfile_normalized_path = match dockerfile_relative_path.trim() {
                "" | "." | "/" | "/." | "./" | "Dockerfile" => "Dockerfile",
                dockerfile_root_path => dockerfile_root_path,
            };

            let dockerfile_relative_path = format!("{}/{}", build.git_repository.root_path, dockerfile_normalized_path);
            let dockerfile_absolute_path = format!("{}/{}", repository_root_path.as_str(), dockerfile_relative_path);

            // If the dockerfile does not exist, abort
            if !Path::new(dockerfile_absolute_path.as_str()).exists() {
                listeners_helper.error(ProgressInfo::new(
                    ProgressScope::Application {
                        id: build.image.application_id.clone(),
                    },
                    ProgressLevel::Error,
                    Some(format!(
                        "Dockerfile is not present at location {}",
                        dockerfile_relative_path
                    )),
                    self.context.execution_id(),
                ));

                let error =
                    EngineError::new_docker_cannot_find_dockerfile(self.get_event_details(), dockerfile_absolute_path);

                self.logger
                    .log(LogLevel::Error, EngineEvent::Error(error.clone(), None));

                return Err(error);
            }

            self.build_image_with_docker(
                build,
                dockerfile_absolute_path.as_str(),
                build_context_path.as_str(),
                env_var_args,
                !disable_build_cache,
                &listeners_helper,
                is_task_canceled,
            )
        } else {
            // build container with Buildpacks
            self.build_image_with_buildpacks(
                build,
                build_context_path.as_str(),
                env_var_args,
                !disable_build_cache,
                &listeners_helper,
                is_task_canceled,
            )
        };

        let msg = match &result {
            Ok(_) => format!("✅ Container {} is built", self.name_with_id()),
            Err(engine_err) if engine_err.tag() == &Tag::TaskCancellationRequested => {
                format!("🚫 Container {} build has been canceled", self.name_with_id())
            }
            Err(engine_err) => {
                format!(
                    "❌ Container {} failed to be build: {}",
                    self.name_with_id(),
                    engine_err.message()
                )
            }
        };

        listeners_helper.deployment_in_progress(ProgressInfo::new(
            ProgressScope::Application { id: app_id },
            ProgressLevel::Info,
            Some(msg.to_string()),
            self.context.execution_id(),
        ));

        self.logger.log(
            LogLevel::Info,
            EngineEvent::Info(event_details.clone(), EventMessage::new_from_safe(msg.to_string())),
        );

        result
    }

    fn build_error(&self, build: Build) -> Result<BuildResult, EngineError> {
        let event_details = self.get_event_details();
        self.logger.log(
            LogLevel::Warning,
            EngineEvent::Warning(
                event_details.clone(),
                EventMessage::new_from_safe(format!("LocalDocker.build_error() called for {}", self.name())),
            ),
        );

        let listener_helper = ListenersHelper::new(&self.listeners);

        // FIXME
        let message = String::from("something goes wrong (not implemented)");

        listener_helper.error(ProgressInfo::new(
            ProgressScope::Application {
                id: build.image.application_id,
            },
            ProgressLevel::Error,
            Some(message.as_str()),
            self.context.execution_id(),
        ));

        let err = EngineError::new_not_implemented_error(event_details);

        self.logger.log(LogLevel::Error, EngineEvent::Error(err.clone(), None));

        // FIXME
        Err(err)
    }

    fn logger(&self) -> Box<dyn Logger> {
        self.logger.clone()
    }
}

impl Listen for LocalDocker {
    fn listeners(&self) -> &Listeners {
        &self.listeners
    }

    fn add_listener(&mut self, listener: Listener) {
        self.listeners.push(listener);
    }
}

impl ToTransmitter for LocalDocker {
    fn to_transmitter(&self) -> Transmitter {
        Transmitter::BuildPlatform(self.id().to_string(), self.name().to_string())
    }
}

fn check_docker_space_usage_and_clean(
    docker_path_size_info: &Disk,
    envs: Vec<(&str, &str)>,
    event_details: EventDetails,
    logger: &dyn Logger,
) -> Result<(), CommandError> {
    let docker_max_disk_percentage_usage_before_purge = 60; // arbitrary percentage that should make the job anytime
    let available_space = docker_path_size_info.get_available_space();
    let docker_percentage_remaining = available_space * 100 / docker_path_size_info.get_total_space();

    if docker_percentage_remaining < docker_max_disk_percentage_usage_before_purge || available_space == 0 {
        logger.log(
            LogLevel::Warning,
            EngineEvent::Warning(
                event_details.clone(),
                EventMessage::new_from_safe(format!(
                    "Docker disk remaining ({}%) is lower than {}%, requesting cleaning (purge)",
                    docker_percentage_remaining, docker_max_disk_percentage_usage_before_purge
                )),
            ),
        );

        return docker_prune_images(envs);
    };

    logger.log(
        LogLevel::Info,
        EngineEvent::Info(
            event_details.clone(),
            EventMessage::new_from_safe(format!(
                "No need to purge old docker images, only {}% ({}/{}) disk used",
                100 - docker_percentage_remaining,
                docker_path_size_info.get_available_space(),
                docker_path_size_info.get_total_space(),
            )),
        ),
    );

    Ok(())
}

fn docker_prune_images(envs: Vec<(&str, &str)>) -> Result<(), CommandError> {
    let all_prunes_commands = vec![
        vec!["container", "prune", "-f"],
        vec!["image", "prune", "-a", "-f"],
        vec!["builder", "prune", "-a", "-f"],
        vec!["volume", "prune", "-f"],
    ];

    let mut errored_commands = vec![];
    for prune in all_prunes_commands {
        let mut cmd = QoveryCommand::new("docker", &prune, &envs);
        if let Err(e) = cmd.exec_with_timeout(Duration::minutes(BUILD_DURATION_TIMEOUT_MIN), |_| {}, |_| {}) {
            errored_commands.push(format!("{} {:?}", prune[0], e));
        }
    }

    if errored_commands.len() > 0 {
        return Err(CommandError::new(
            errored_commands.join("/ "),
            Some("Error while trying to prune images.".to_string()),
        ));
    }

    Ok(())
}
