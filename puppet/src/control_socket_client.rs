use std::collections::HashMap;

use anyhow::{bail, Context, Result};

// TCP control socket transport implementation:
#[cfg(feature = "transport_tcp")]
pub use tml_tcp_control_socket_client as tcp;

use treadmill_rs::api::supervisor_puppet::{
    NetworkConfig, ParameterValue, PuppetEvent, PuppetReq, SupervisorEvent, SupervisorResp,
};

pub enum ControlSocketClient {
    #[cfg(feature = "transport_tcp")]
    Tcp(tcp::TcpControlSocketClient),
}

impl ControlSocketClient {
    pub async fn request(&self, req: PuppetReq) -> Result<SupervisorResp> {
        match self {
            #[cfg(feature = "transport_tcp")]
            ControlSocketClient::Tcp(client) => client.request(req).await,
        }
    }

    pub async fn send_event(&self, ev: PuppetEvent) -> Result<()> {
        match self {
            #[cfg(feature = "transport_tcp")]
            ControlSocketClient::Tcp(client) => client.send_event(ev).await,
        }
    }

    pub async fn listen(&self) -> Result<(u64, SupervisorEvent)> {
        match self {
            #[cfg(feature = "transport_tcp")]
            ControlSocketClient::Tcp(client) => client.listen().await,
        }
    }

    pub async fn shutdown(self) -> Result<()> {
        match self {
            #[cfg(feature = "transport_tcp")]
            ControlSocketClient::Tcp(client) => client.shutdown().await,
        }
    }

    pub async fn get_ssh_keys(&self) -> Result<Vec<String>> {
        let resp = self
            .request(PuppetReq::SSHKeys)
            .await
            .context("Sending SSH keys request to supervisor")?;

        match resp {
            SupervisorResp::SSHKeysResp { ssh_keys } => Ok(ssh_keys),
            _ => {
                bail!(
                    "Invalid supervisor response to SSH keys request: {:?}",
                    resp
                );
            }
        }
    }

    pub async fn get_network_config(&self) -> Result<NetworkConfig> {
        let resp = self
            .request(PuppetReq::NetworkConfig)
            .await
            .context("Sending network config request to supervisor")?;

        match resp {
            SupervisorResp::NetworkConfig(nc) => Ok(nc),
            _ => {
                bail!(
                    "Invalid supervisor response to network config request: {:?}",
                    resp
                );
            }
        }
    }

    pub async fn get_parameters(&self) -> Result<HashMap<String, ParameterValue>> {
        let resp = self
            .request(PuppetReq::Parameters)
            .await
            .context("Sending parameters request to supervisor")?;

        match resp {
            SupervisorResp::Parameters { parameters } => Ok(parameters),
            _ => {
                bail!(
                    "Invalid supervisor response to parameters request: {:?}",
                    resp
                );
            }
        }
    }

    pub async fn report_ready(&self) -> Result<()> {
        self.send_event(PuppetEvent::Ready).await
    }
}
