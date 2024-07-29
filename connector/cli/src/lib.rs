use std::collections::HashMap;
use std::sync::{Arc, Weak};

use async_trait::async_trait;
use log::{error, info};
use serde::Deserialize;

use treadmill_rs::api::switchboard_supervisor::ParameterValue;
use treadmill_rs::api::switchboard_supervisor::{JobInitSpec, RestartPolicy};
use treadmill_rs::connector;
use treadmill_rs::image::manifest::ImageId;
use uuid::Uuid;

pub mod cli;
pub mod edit_tree;

#[derive(Deserialize, Clone, Debug)]
pub struct CliConnectorImageConfig {}

#[derive(Deserialize, Clone, Debug)]
pub struct CliConnectorConfig {
    pub images: HashMap<Uuid, CliConnectorImageConfig>,
}

#[derive(Deserialize, Clone, Debug)]
pub struct JobConfig {}

pub struct CliConnector<S: connector::Supervisor> {
    _supervisor_id: Uuid,
    _config: CliConnectorConfig,
    supervisor: Weak<S>,
}

#[derive(Debug, Clone)]
pub struct CliSelectJobCommand {
    job_id: Option<Uuid>,
}

impl CliSelectJobCommand {
    pub fn parse<'a>(input: cli::ParseInput<'a, CliState>) -> cli::ParseChain<'a, CliState, Self> {
        let input = input
            .parse()
            .add_completions(|prefix| {
                input
                    .ctx
                    .jobs
                    .keys()
                    .map(|k| k.to_string())
                    .filter(move |k| k.trim() == "" || k.starts_with(&prefix))
            })
            .parse_subcommands(&[("new", &|input| {
                input.parse().accept(CliSelectJobCommand {
                    job_id: Some(Uuid::new_v4()),
                })
            })]);

        if input.exact_match.is_some() {
            input
        } else {
            match input.parse_uuid_arg() {
                cli::ArgumentParseResult::Ok(uuid, parse_chain) => {
                    parse_chain.accept(CliSelectJobCommand { job_id: Some(uuid) })
                }
                cli::ArgumentParseResult::Missing(parse_chain) => {
                    parse_chain.accept(CliSelectJobCommand { job_id: None })
                }
                cli::ArgumentParseResult::ParseError(_e, parse_chain) => parse_chain,
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct CliStartJobCommand;
impl CliStartJobCommand {
    pub fn parse<'a>(input: cli::ParseInput<'a, CliState>) -> cli::ParseChain<'a, CliState, Self> {
        match input.ctx.selected_job {
            None => input.parse().error("Select a job first (job <uuid>)"),
            Some(_) => input.parse().accept(CliStartJobCommand),
        }
    }
}

#[derive(Debug, Clone)]
pub struct CliStopJobCommand;
impl CliStopJobCommand {
    pub fn parse<'a>(input: cli::ParseInput<'a, CliState>) -> cli::ParseChain<'a, CliState, Self> {
        match input.ctx.selected_job {
            None => input.parse().error("Select a job first (job <uuid>)"),
            Some(_) => input.parse().accept(CliStopJobCommand),
        }
    }
}

#[derive(Debug, Clone)]
pub struct CliResetJobCommand;
impl CliResetJobCommand {
    pub fn parse<'a>(input: cli::ParseInput<'a, CliState>) -> cli::ParseChain<'a, CliState, Self> {
        match input.ctx.selected_job {
            None => input.parse().error("Select a job first (job <uuid>)"),
            Some(_) => input.parse().accept(CliResetJobCommand),
        }
    }
}

#[derive(Debug, Clone)]
pub struct CliRebootJobCommand;
impl CliRebootJobCommand {
    pub fn parse<'a>(input: cli::ParseInput<'a, CliState>) -> cli::ParseChain<'a, CliState, Self> {
        match input.ctx.selected_job {
            None => input.parse().error("Select a job first (job <uuid>)"),
            Some(_) => input.parse().accept(CliRebootJobCommand),
        }
    }
}

#[derive(Debug, Clone)]
pub struct CliEditCommand;
impl CliEditCommand {
    pub fn parse<'a>(input: cli::ParseInput<'a, CliState>) -> cli::ParseChain<'a, CliState, Self> {
        input.parse().accept(CliEditCommand)
    }
}

#[derive(Debug, Clone)]
pub struct CliHelpCommand;
impl CliHelpCommand {
    pub fn parse<'a>(input: cli::ParseInput<'a, CliState>) -> cli::ParseChain<'a, CliState, Self> {
        input.parse().accept(CliHelpCommand)
    }
}

#[derive(Debug, Clone)]
pub struct CliExitCommand;
impl CliExitCommand {
    pub fn parse<'a>(input: cli::ParseInput<'a, CliState>) -> cli::ParseChain<'a, CliState, Self> {
        input.parse().accept(CliExitCommand)
    }
}

#[derive(Debug, Clone)]
pub struct CliEditTreeExitCommand;
impl CliEditTreeExitCommand {
    pub fn parse<'a>(input: cli::ParseInput<'a, CliState>) -> cli::ParseChain<'a, CliState, Self> {
        input.parse().accept(CliEditTreeExitCommand)
    }
}

#[derive(Debug, Clone)]
pub enum CliCommand {
    Help(CliHelpCommand),
    Exit(CliExitCommand),
    Edit(CliEditCommand),
    SelectJob(CliSelectJobCommand),
    StartJob(CliStartJobCommand),
    StopJob(CliStopJobCommand),
    RebootJob(CliRebootJobCommand),
    ResetJob(CliResetJobCommand),
    EditTreeCommand(edit_tree::EditTreeCommand),
    EditTreeExit(CliEditTreeExitCommand),
}

impl CliCommand {
    pub fn parse<'a>(input: cli::ParseInput<'a, CliState>) -> cli::ParseChain<'a, CliState, Self> {
        // We have a modal interface. By default we execute regular
        // CliCommands, but we can also be in the `EditTree`
        // mode. Check whether we are in this mode. In that case, we
        // use that parser instead:
        if input.ctx.edit_tree_mode {
            // We're in EditTree mode, use that mode's parser, but
            // also catch the `exit` command, as that'll drop us back
            // into our regular mode:
            //
            // EditTree requires an `Input` with a different context
            // argument, but we want to carry `Input` (with the proper
            // offsets) through the chain. Thus swap the context
            // references as necessary:
            input
                .ctx
                .edit_tree_state
                .parse(input.clone().map_ctx(|c| &c.edit_tree_state))
                .map_match(CliCommand::EditTreeCommand)
                .map_input_ctx(|_| input.ctx)
                .parse_subcommands(&[("exit", &|input| {
                    CliEditTreeExitCommand::parse(input).map_match(CliCommand::EditTreeExit)
                })])
        } else {
            input
                .parse()
                .missing_unknown_command_error()
                .parse_subcommands(&[
                    ("exit", &|input| {
                        CliExitCommand::parse(input).map_match(CliCommand::Exit)
                    }),
                    ("help", &|input| {
                        CliHelpCommand::parse(input).map_match(CliCommand::Help)
                    }),
                    ("edit", &|input| {
                        CliEditCommand::parse(input).map_match(CliCommand::Edit)
                    }),
                    ("job", &|input| {
                        CliSelectJobCommand::parse(input).map_match(CliCommand::SelectJob)
                    }),
                    ("start", &|input| {
                        CliStartJobCommand::parse(input).map_match(CliCommand::StartJob)
                    }),
                    ("stop", &|input| {
                        CliStopJobCommand::parse(input).map_match(CliCommand::StopJob)
                    }),
                    ("reboot", &|input| {
                        CliRebootJobCommand::parse(input).map_match(CliCommand::RebootJob)
                    }),
                    ("reset", &|input| {
                        CliResetJobCommand::parse(input).map_match(CliCommand::ResetJob)
                    }),
                ])
        }
    }

    pub async fn execute<S: connector::Supervisor>(
        &self,
        state: &mut CliState,
        supervisor: &Arc<S>,
    ) -> bool {
        match self {
            CliCommand::Exit(CliExitCommand) => true,
            CliCommand::Help(CliHelpCommand) => unimplemented!(),
            CliCommand::SelectJob(CliSelectJobCommand { job_id }) => {
                state.selected_job = *job_id;
                if let Some(id) = job_id {
                    state.jobs.entry(*id).or_insert(JobState::default());
                }
                false
            }
            CliCommand::StartJob(CliStartJobCommand) => {
                let (job_id, cli_job_state) = if let Some(id) = state.selected_job {
                    (id, state.jobs.entry(id).or_insert(JobState::default()))
                } else {
                    error!("No job_id selected!");
                    return false;
                };

                // Immediately proceed to start the job:
                let ssh_keys = vec!["mytestsshkey".to_string()];

                info!(
                    "Requesting start of new job {}, ssh keys: {:?}",
                    job_id, &ssh_keys
                );

                let parameters = cli_job_state
                    .parameters
                    .iter()
                    .map(|(k, v)| {
                        (
                            k.clone(),
                            ParameterValue {
                                value: v.clone(),
                                secret: false,
                            },
                        )
                    })
                    .collect();

                let start_job_res = S::start_job(
                    supervisor,
                    connector::StartJobMessage {
                        job_id,
                        // TODO: require image to be defined
                        init_spec: JobInitSpec::Image {
                            image_id: ImageId([
                                0xe4, 0x8d, 0x86, 0x78, 0xb3, 0x1f, 0xf, 0xb1, 0x2c, 0xd1, 0x91,
                                0xc, 0xe2, 0xf1, 0xf2, 0xcd, 0x5c, 0xed, 0x62, 0x3b, 0xf1, 0x8d,
                                0x68, 0x79, 0xb2, 0xb2, 0xf1, 0x98, 0xdc, 0xed, 0x40, 0x5e,
                            ]),
                        },
                        ssh_keys,
                        restart_policy: RestartPolicy {
                            remaining_restart_count: 0,
                        },
                        ssh_rendezvous_servers: vec![],
                        parameters,
                    },
                )
                .await;

                if let Err(start_job_err) = start_job_res {
                    error!("Failed to start job: {:#?}", start_job_err);
                }

                false
            }
            CliCommand::StopJob(CliStopJobCommand) => {
                let (job_id, _cli_job_state) = if let Some(id) = state.selected_job {
                    (id, state.jobs.entry(id).or_insert(JobState::default()))
                } else {
                    error!("No job_id selected!");
                    return false;
                };

                let stop_job_res =
                    S::stop_job(supervisor, connector::StopJobMessage { job_id }).await;

                if let Err(stop_job_err) = stop_job_res {
                    error!("Failed to stop job: {:#?}", stop_job_err);
                }

                false
            }
            CliCommand::RebootJob(CliRebootJobCommand) => {
                unimplemented!()
            }
            CliCommand::ResetJob(CliResetJobCommand) => {
                unimplemented!()
            }
            CliCommand::Edit(CliEditCommand) => {
                state.edit_tree_mode = true;
                false
            }
            CliCommand::EditTreeCommand(edit_tree_cmd) => {
                state.edit_tree_state.execute(edit_tree_cmd);
                false
            }
            CliCommand::EditTreeExit(CliEditTreeExitCommand) => {
                state.edit_tree_mode = false;
                state.edit_tree_state.reset_context();
                false
            }
        }
    }
}

#[derive(Default, Debug, Clone)]
pub struct JobState {
    // TODO: allow customizations of these parameters.
    // started: bool,
    // image: Option<String>,
    parameters: HashMap<String, String>,
}

#[derive(Default, Debug, Clone)]
pub struct CliState {
    selected_job: Option<Uuid>,
    jobs: HashMap<Uuid, JobState>,
    edit_tree_mode: bool,
    edit_tree_state: edit_tree::EditTree,
}

impl<S: connector::Supervisor> CliConnector<S> {
    pub fn new(supervisor_id: Uuid, config: CliConnectorConfig, supervisor: Weak<S>) -> Self {
        CliConnector {
            _supervisor_id: supervisor_id,
            _config: config,
            supervisor,
        }
    }
}

#[async_trait]
impl<S: connector::Supervisor> connector::SupervisorConnector for CliConnector<S> {
    async fn run(&self) {
        // Acquire a "strong" Arc<> reference to the supervisor. Not holding
        // onto a strong reference beyond invocations of "run" will ensure that
        // the contained supervisor can be deallocated properly.
        let supervisor = self.supervisor.upgrade().unwrap();

        // Create a REPL. Check if we have a job-config pre-defined that we
        // should run, otherwise drop the user in a REPL that can define and
        // execute jobs.
        let mut cli_state = CliState::default();

        loop {
            // Get the current CLI prompt render config:
            let (prompt_prefix, answered_prompt_prefix) = if cli_state.edit_tree_mode {
                ("[edit] ?".to_string(), "[edit] >".to_string())
            } else if let Some(ref job_id) = cli_state.selected_job {
                (format!("job:{} ?", job_id), format!("job:{} >", job_id))
            } else {
                ("?".to_string(), ">".to_string())
            };

            let render_config = inquire::ui::RenderConfig::default()
                .with_prompt_prefix(inquire::ui::Styled::new(prompt_prefix.as_str()))
                .with_answered_prompt_prefix(inquire::ui::Styled::new(
                    answered_prompt_prefix.as_str(),
                ));

            let cmd_res = tokio::task::block_in_place(|| {
                inquire::Text::new("")
                    .with_autocomplete(|val: &str| {
                        Ok(
                            CliCommand::parse(cli::ParseInput::new(val, &cli_state, true))
                                .completions,
                        )
                    })
                    .with_validator(|val: &str| {
                        let parsed =
                            CliCommand::parse(cli::ParseInput::new(val, &cli_state, false));
                        match (parsed.error, parsed.exact_match.is_none()) {
                            (Some(errmsg), true) => {
                                Ok(inquire::validator::Validation::Invalid(errmsg.into()))
                            }
                            _ => Ok(inquire::validator::Validation::Valid),
                        }
                    })
                    .with_render_config(render_config)
                    .prompt()
            });

            match cmd_res {
                Ok(cmd) => {
                    let parsed = CliCommand::parse(cli::ParseInput::new(&cmd, &cli_state, false));

                    if let Some(matched) = parsed.exact_match {
                        if matched.execute(&mut cli_state, &supervisor).await {
                            // Asked to quit:
                            return;
                        }
                    } else {
                        error!("Internal error: parser did not produce a match for the entered command: {:#?}", parsed);
                    }
                }
                Err(inquire::error::InquireError::OperationCanceled) => {
                    // Asked to quit:
                    return;
                }
                Err(inquire::error::InquireError::OperationInterrupted) => {
                    // Nop, just print a new command line:
                }
                e => error!("Unhandled inquire error {:#?}", e),
            }
        }
    }

    async fn update_job_state(&self, job_id: Uuid, job_state: connector::JobState) {
        log::info!(
            "Supervisor provides job state for job {}: {:#?}",
            job_id,
            job_state
        );
    }

    async fn report_job_error(&self, job_id: Uuid, error: connector::JobError) {
        log::info!(
            "Supervisor provides job error: job {}, error: {:#?}",
            job_id,
            error,
        );
    }

    async fn send_job_console_log(&self, job_id: Uuid, console_bytes: Vec<u8>) {
        log::debug!(
            "Supervisor provides console log: job {}, length: {}, message: {:?}",
            job_id,
            console_bytes.len(),
            String::from_utf8_lossy(&console_bytes)
        );
    }
}
