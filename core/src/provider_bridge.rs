// JSON-over-stdio bridge to Python provider processes.
// Spawns a Python process per operation, sends a JSON command on stdin,
// reads a JSON response from stdout.

use std::process::Stdio;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::models::{JobHandle, JobResult, ResourceRequest};

/// The response types we can get back from the Python provider host.
#[derive(Debug)]
pub enum ProviderResponse {
    Handle(JobHandle),
    Status(String),
    Result(JobResult),
    Capabilities(serde_json::Value),
    Ok,
    Error(String),
}

pub struct ProviderBridge {
    python_bin: String,
    integrations_path: String,
    workspace_dir: String,
}

impl ProviderBridge {
    pub fn new(python_bin: String, integrations_path: String, workspace_dir: String) -> Self {
        Self {
            python_bin,
            integrations_path,
            workspace_dir,
        }
    }

    async fn call(&self, command: serde_json::Value) -> anyhow::Result<serde_json::Value> {
        let mut child = Command::new(&self.python_bin)
            .arg("-m")
            .arg("integrations.provider_host")
            .env("PYTHONPATH", &self.integrations_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        let stdin = child.stdin.as_mut().unwrap();
        stdin
            .write_all(serde_json::to_string(&command)?.as_bytes())
            .await?;
        stdin.shutdown().await?;

        let output = child.wait_with_output().await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("provider process failed: {}", stderr);
        }

        let stdout = String::from_utf8(output.stdout)?;
        let response: serde_json::Value = serde_json::from_str(&stdout)?;

        if let Some(err) = response.get("error") {
            anyhow::bail!("provider error: {}", err);
        }

        Ok(response)
    }

    pub async fn launch(
        &self,
        provider: &str,
        request: &ResourceRequest,
        config: &serde_json::Value,
    ) -> anyhow::Result<JobHandle> {
        let cmd = serde_json::json!({
            "method": "launch",
            "provider": provider,
            "request": request,
            "config": config,
            "workspace_dir": self.workspace_dir,
        });
        let resp = self.call(cmd).await?;
        let data = &resp["data"];
        Ok(serde_json::from_value(data.clone())?)
    }

    pub async fn poll(&self, provider: &str, handle: &JobHandle) -> anyhow::Result<ProviderResponse> {
        let cmd = serde_json::json!({
            "method": "poll",
            "provider": provider,
            "handle": handle,
        });
        let resp = self.call(cmd).await?;
        let resp_type = resp["type"].as_str().unwrap_or("");
        match resp_type {
            "status" => {
                let status = resp["data"]["status"].as_str().unwrap_or("running");
                Ok(ProviderResponse::Status(status.to_string()))
            }
            "result" => {
                let result: JobResult = serde_json::from_value(resp["data"].clone())?;
                Ok(ProviderResponse::Result(result))
            }
            _ => Ok(ProviderResponse::Status("running".to_string())),
        }
    }

    pub async fn cancel(&self, provider: &str, handle: &JobHandle) -> anyhow::Result<()> {
        let cmd = serde_json::json!({
            "method": "cancel",
            "provider": provider,
            "handle": handle,
        });
        self.call(cmd).await?;
        Ok(())
    }

    pub async fn capabilities(&self, provider: &str) -> anyhow::Result<serde_json::Value> {
        let cmd = serde_json::json!({
            "method": "capabilities",
            "provider": provider,
        });
        let resp = self.call(cmd).await?;
        Ok(resp["data"].clone())
    }
}
