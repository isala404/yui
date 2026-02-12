use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionInput {
    pub job_id: Uuid,
    pub trace_id: Uuid,
    pub prompt: String,
    pub attachments: Vec<serde_json::Value>,
    pub session_id: Option<String>,
    pub resume_input: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionConfig {
    pub docker_image: String,
    pub workspace_dir: String,
    pub media_dir: String,
    pub sessions_dir: String,
    pub start_timeout_secs: u64,
    pub idle_timeout_secs: u64,
    pub max_attachment_mb: u64,
}

impl Default for ExecutionConfig {
    fn default() -> Self {
        Self {
            docker_image: "claude-code:latest".to_string(),
            workspace_dir: "storage/workspaces".to_string(),
            media_dir: "storage/media".to_string(),
            sessions_dir: "storage/sessions".to_string(),
            start_timeout_secs: 60,
            idle_timeout_secs: 300,
            max_attachment_mb: 100,
        }
    }
}

// JSONL protocol frames from the container
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ContainerFrame {
    #[serde(rename = "session")]
    Session { session_id: String },
    #[serde(rename = "log")]
    Log { stream: String, line: String },
    #[serde(rename = "ask_user")]
    AskUser { question: String },
    #[serde(rename = "final")]
    Final {
        output: String,
        #[serde(default)]
        attachments: Vec<serde_json::Value>,
    },
    #[serde(rename = "error")]
    Error {
        message: String,
        #[serde(default)]
        retryable: bool,
    },
}

#[derive(Debug, Clone)]
pub enum ExecutionOutcome {
    Completed {
        output: String,
        attachments: Vec<serde_json::Value>,
    },
    Paused {
        question: String,
    },
    Failed {
        error: String,
    },
}

pub struct AgentExecutor {
    config: ExecutionConfig,
}

impl AgentExecutor {
    pub fn from_env() -> Self {
        let config = ExecutionConfig {
            docker_image: std::env::var("YUI_DOCKER_IMAGE")
                .unwrap_or_else(|_| "claude-code:latest".to_string()),
            workspace_dir: std::env::var("YUI_WORKSPACE_DIR")
                .unwrap_or_else(|_| "storage/workspaces".to_string()),
            media_dir: std::env::var("YUI_MEDIA_DIR")
                .unwrap_or_else(|_| "storage/media".to_string()),
            sessions_dir: std::env::var("YUI_SESSIONS_DIR")
                .unwrap_or_else(|_| "storage/sessions".to_string()),
            start_timeout_secs: std::env::var("YUI_DOCKER_TIMEOUT_START_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(60),
            idle_timeout_secs: std::env::var("YUI_DOCKER_TIMEOUT_IDLE_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(300),
            max_attachment_mb: std::env::var("YUI_MAX_ATTACHMENT_MB")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(100),
        };
        Self { config }
    }

    fn canonical_or(path: &str) -> PathBuf {
        Path::new(path)
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(path))
    }

    pub async fn execute(
        &self,
        input: ExecutionInput,
        log_tx: tokio::sync::mpsc::UnboundedSender<(String, String)>,
    ) -> ExecutionOutcome {
        let workspace = format!("{}/{}", self.config.workspace_dir, input.job_id);
        if let Err(e) = tokio::fs::create_dir_all(&workspace).await {
            return ExecutionOutcome::Failed {
                error: format!("failed to create workspace: {e}"),
            };
        }

        let prompt_path = format!("{workspace}/prompt.txt");
        let prompt_content = if let Some(ref resume) = input.resume_input {
            format!("{}\n\nUser response: {resume}", input.prompt)
        } else {
            input.prompt.clone()
        };
        if let Err(e) = tokio::fs::write(&prompt_path, &prompt_content).await {
            return ExecutionOutcome::Failed {
                error: format!("failed to write prompt: {e}"),
            };
        }

        let attachments_json = serde_json::to_string(&input.attachments).unwrap_or_default();

        let workspace_abs = Self::canonical_or(&workspace);
        let media_abs = Self::canonical_or(&self.config.media_dir);
        let sessions_abs = Self::canonical_or(&self.config.sessions_dir);

        let container_name = format!("yui-job-{}", input.job_id.as_simple());

        let mut cmd = Command::new("docker");
        cmd.arg("run")
            .arg("--rm")
            .arg("--name")
            .arg(&container_name)
            .arg("-v")
            .arg(format!("{}:/workspace", workspace_abs.display()))
            .arg("-v")
            .arg(format!("{}:/storage/media:ro", media_abs.display()))
            .arg("-v")
            .arg(format!("{}:/storage/sessions", sessions_abs.display()))
            .arg("-e")
            .arg(format!("YUI_JOB_ID={}", input.job_id))
            .arg("-e")
            .arg(format!("YUI_TRACE_ID={}", input.trace_id))
            .arg("-e")
            .arg("YUI_PROMPT_PATH=/workspace/prompt.txt")
            .arg("-e")
            .arg(format!("YUI_ATTACHMENTS_JSON={attachments_json}"))
            .arg("-e")
            .arg("IS_SANDBOX=1");

        if let Some(ref session_id) = input.session_id {
            cmd.arg("-e").arg(format!("YUI_SESSION_ID={session_id}"));
        }

        // mount Claude auth credentials for the non-root yui user
        // on macOS, credentials live in keychain so we extract to a temp dir
        // on Linux, they live in ~/.claude/.credentials.json
        let auth_dir = format!("{}/claude-auth", workspace);
        if let Err(e) = tokio::fs::create_dir_all(&auth_dir).await {
            tracing::warn!(error = %e, "failed to create claude auth dir");
        } else {
            let creds_written = write_claude_credentials(&auth_dir).await;
            if creds_written {
                let auth_abs = Self::canonical_or(&auth_dir);
                cmd.arg("-v")
                    .arg(format!("{}:/mnt/claude-auth:ro", auth_abs.display()));
            }
        }

        // resource limits
        cmd.arg("--memory=2g").arg("--cpus=2");

        cmd.arg(&self.config.docker_image);

        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                return ExecutionOutcome::Failed {
                    error: format!("failed to spawn docker: {e}"),
                };
            }
        };

        let mut final_output = None;
        let mut final_attachments: Vec<serde_json::Value> = vec![];
        let mut ask_question = None;

        if let Some(stdout) = child.stdout.take() {
            let reader = BufReader::new(stdout);
            let mut lines = reader.lines();

            let idle_timeout = tokio::time::Duration::from_secs(self.config.idle_timeout_secs);

            loop {
                let line_result = tokio::time::timeout(idle_timeout, lines.next_line()).await;

                match line_result {
                    Ok(Ok(Some(line))) => {
                        match serde_json::from_str::<ContainerFrame>(&line) {
                            Ok(ContainerFrame::Session { .. }) => {}
                            Ok(ContainerFrame::Log { stream, line: text }) => {
                                let _ = log_tx.send((stream, text));
                            }
                            Ok(ContainerFrame::AskUser { question }) => {
                                ask_question = Some(question);
                                // kill container after receiving ask_user
                                let _ = kill_container(&container_name).await;
                                break;
                            }
                            Ok(ContainerFrame::Final {
                                output,
                                attachments,
                            }) => {
                                final_output = Some(output);
                                final_attachments = attachments;
                            }
                            Ok(ContainerFrame::Error { message, .. }) => {
                                return ExecutionOutcome::Failed { error: message };
                            }
                            Err(_) => {
                                // plain log line
                                let _ = log_tx.send(("stdout".to_string(), line));
                            }
                        }
                    }
                    Ok(Ok(None)) => break,
                    Ok(Err(e)) => {
                        tracing::warn!(error = %e, "error reading container stdout");
                        break;
                    }
                    Err(_) => {
                        // idle timeout
                        let _ = kill_container(&container_name).await;
                        return ExecutionOutcome::Failed {
                            error: format!(
                                "container idle timeout after {}s",
                                self.config.idle_timeout_secs
                            ),
                        };
                    }
                }
            }
        }

        if let Some(stderr) = child.stderr.take() {
            let reader = BufReader::new(stderr);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let _ = log_tx.send(("stderr".to_string(), line));
            }
        }

        let exit_status = child.wait().await;

        if let Some(question) = ask_question {
            return ExecutionOutcome::Paused { question };
        }

        if let Some(output) = final_output {
            let resolved = self
                .collect_output_files(&workspace, &final_attachments)
                .await;
            return ExecutionOutcome::Completed {
                output,
                attachments: resolved,
            };
        }

        match exit_status {
            Ok(status) if status.success() => ExecutionOutcome::Completed {
                output: "task completed (no structured output)".to_string(),
                attachments: vec![],
            },
            Ok(status) => {
                let code = status.code().unwrap_or(-1);
                ExecutionOutcome::Failed {
                    error: format!("container exited with code {code}"),
                }
            }
            Err(e) => ExecutionOutcome::Failed {
                error: format!("failed to wait for container: {e}"),
            },
        }
    }
}

impl AgentExecutor {
    /// Copy output files from workspace to storage/media/ and return outbox-ready attachment entries.
    async fn collect_output_files(
        &self,
        workspace: &str,
        container_attachments: &[serde_json::Value],
    ) -> Vec<serde_json::Value> {
        let mut result = vec![];
        let media_dir = &self.config.media_dir;

        if let Err(e) = tokio::fs::create_dir_all(media_dir).await {
            tracing::warn!(error = %e, "failed to create media dir");
            return result;
        }

        for att in container_attachments {
            let container_path = match att["path"].as_str() {
                Some(p) => p,
                None => continue,
            };
            let name = att["name"].as_str().unwrap_or("file");
            let mime = att["mime"].as_str().unwrap_or("application/octet-stream");
            let ftype = att["type"].as_str().unwrap_or("document");

            // container path /workspace/foo.mp4 -> host path {workspace}/foo.mp4
            let host_path = container_path.replacen("/workspace/", &format!("{workspace}/"), 1);

            if !Path::new(&host_path).exists() {
                tracing::warn!(path = %host_path, "output file not found on host");
                continue;
            }

            let dest_name = format!("{}_{name}", uuid::Uuid::new_v4().as_simple());
            let dest_path = format!("{media_dir}/{dest_name}");

            match tokio::fs::copy(&host_path, &dest_path).await {
                Ok(size) => {
                    tracing::info!(src = %host_path, dst = %dest_path, size, "copied output file to media");
                    result.push(serde_json::json!({
                        "type": ftype,
                        "path": dest_path,
                        "name": name,
                        "mime": mime,
                    }));
                }
                Err(e) => {
                    tracing::warn!(error = %e, path = %host_path, "failed to copy output file");
                }
            }
        }

        result
    }
}

/// Extract Claude Code credentials and write them to the auth dir for container mounting.
/// On macOS: extracts from Keychain via `security` command.
/// On Linux: copies from ~/.claude/.credentials.json if it exists.
async fn write_claude_credentials(auth_dir: &str) -> bool {
    let creds_path = format!("{auth_dir}/credentials.json");
    let config_path = format!("{auth_dir}/claude.json");

    // try macOS keychain first
    if let Ok(output) = tokio::process::Command::new("security")
        .args([
            "find-generic-password",
            "-s",
            "Claude Code-credentials",
            "-w",
        ])
        .output()
        .await
        && output.status.success()
    {
        let keychain_data = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !keychain_data.is_empty() {
            if let Err(e) = tokio::fs::write(&creds_path, &keychain_data).await {
                tracing::warn!(error = %e, "failed to write claude credentials");
                return false;
            }
            // write minimal config with onboarding flag
            let _ = tokio::fs::write(&config_path, r#"{"hasCompletedOnboarding":true}"#).await;
            tracing::info!("claude credentials extracted from macOS keychain");
            return true;
        }
    }

    // fallback: try Linux file-based credentials
    if let Ok(home) = std::env::var("HOME") {
        let linux_creds = format!("{home}/.claude/.credentials.json");
        if std::path::Path::new(&linux_creds).exists()
            && let Ok(data) = tokio::fs::read(&linux_creds).await
        {
            let _ = tokio::fs::write(&creds_path, &data).await;
            let _ = tokio::fs::write(&config_path, r#"{"hasCompletedOnboarding":true}"#).await;
            tracing::info!("claude credentials copied from ~/.claude/.credentials.json");
            return true;
        }
    }

    tracing::warn!("no claude credentials found for container auth");
    false
}

async fn kill_container(name: &str) -> bool {
    Command::new("docker")
        .args(["kill", name])
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_container_frame(line: &str) -> Option<ContainerFrame> {
        serde_json::from_str(line).ok()
    }

    #[test]
    fn parses_session_frame() {
        let frame: ContainerFrame =
            serde_json::from_str(r#"{"type":"session","session_id":"abc123"}"#).unwrap();
        assert!(matches!(frame, ContainerFrame::Session { session_id } if session_id == "abc123"));
    }

    #[test]
    fn parses_log_frame() {
        let frame: ContainerFrame =
            serde_json::from_str(r#"{"type":"log","stream":"stdout","line":"hello"}"#).unwrap();
        assert!(
            matches!(frame, ContainerFrame::Log { stream, line } if stream == "stdout" && line == "hello")
        );
    }

    #[test]
    fn parses_ask_user_frame() {
        let frame: ContainerFrame =
            serde_json::from_str(r#"{"type":"ask_user","question":"what color?"}"#).unwrap();
        assert!(matches!(frame, ContainerFrame::AskUser { question } if question == "what color?"));
    }

    #[test]
    fn parses_final_frame() {
        let frame: ContainerFrame =
            serde_json::from_str(r#"{"type":"final","output":"done!","attachments":[]}"#).unwrap();
        assert!(matches!(frame, ContainerFrame::Final { output, .. } if output == "done!"));
    }

    #[test]
    fn parses_error_frame() {
        let frame: ContainerFrame =
            serde_json::from_str(r#"{"type":"error","message":"boom","retryable":true}"#).unwrap();
        assert!(
            matches!(frame, ContainerFrame::Error { message, retryable } if message == "boom" && retryable)
        );
    }

    #[test]
    fn non_json_returns_none() {
        assert!(parse_container_frame("just a log line").is_none());
    }

    #[test]
    fn default_config_is_reasonable() {
        let config = ExecutionConfig::default();
        assert_eq!(config.start_timeout_secs, 60);
        assert_eq!(config.idle_timeout_secs, 300);
        assert_eq!(config.max_attachment_mb, 100);
    }

    #[test]
    fn canonical_or_falls_back_to_original_path() {
        let missing = "/tmp/yui-agent-executor-does-not-exist";
        let resolved = AgentExecutor::canonical_or(missing);
        assert_eq!(resolved, std::path::PathBuf::from(missing));
    }
}
