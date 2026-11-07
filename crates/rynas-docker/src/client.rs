//! The bollard client over `/var/run/docker.sock`. Only the calls rynas needs are wrapped
//! (lists, inspect, start/stop/restart, logs, events, disk usage, the registry digest):
//! nothing here forwards an arbitrary request to the engine.

use std::collections::HashMap;

use bollard::Docker;
use bollard::errors::Error as BErr;
use bollard::models as bm;
use bollard::query_parameters as qp;
use futures_util::{Stream, StreamExt};
use rynas_api_types::docker::{ImageSummary, LogLine, NetworkSummary, VolumeSummary};

use crate::model::{self, Seen};

/// Seconds to the answer head of a call (streams then last as long as they need).
pub const TIMEOUT_S: u64 = 30;
/// Seconds a container gets to stop before it is killed.
pub const STOP_GRACE_S: i32 = 30;
/// Most lines of a log tail.
pub const MAX_TAIL: u32 = 5000;

/// Why a call failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DockerError {
    /// The socket is missing or does not answer.
    Unavailable(String),
    NotFound(String),
    Conflict(String),
    Failed(String),
}

impl std::fmt::Display for DockerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DockerError::Unavailable(m)
            | DockerError::NotFound(m)
            | DockerError::Conflict(m)
            | DockerError::Failed(m) => f.write_str(m),
        }
    }
}

impl From<BErr> for DockerError {
    fn from(e: BErr) -> Self {
        match e {
            BErr::DockerResponseServerError {
                status_code: 404,
                message,
            } => DockerError::NotFound(message),
            BErr::DockerResponseServerError {
                status_code: 409,
                message,
            } => DockerError::Conflict(message),
            BErr::DockerResponseServerError {
                status_code,
                message,
            } => DockerError::Failed(format!("Docker answered {status_code}: {message}")),
            BErr::SocketNotFoundError(p) => {
                DockerError::Unavailable(format!("no Docker socket at {p}"))
            }
            BErr::RequestTimeoutError => {
                DockerError::Unavailable("Docker did not answer in time".into())
            }
            BErr::HyperResponseError { .. } | BErr::IOError { .. } => {
                DockerError::Unavailable(format!("Docker is not reachable: {e}"))
            }
            other => DockerError::Failed(other.to_string()),
        }
    }
}

/// A connection to the engine (cheap to clone: a pooled client).
#[derive(Clone)]
pub struct Engine {
    docker: Docker,
}

impl Engine {
    /// Connects to `socket` and negotiates the API version with the engine (older engines
    /// get their own version; Docker 25+ is needed by today's managers anyway).
    pub async fn connect(socket: &str) -> Result<Self, DockerError> {
        let d = Docker::connect_with_unix(socket, TIMEOUT_S, bollard::API_DEFAULT_VERSION)?;
        let d = d.negotiate_version().await?;
        Ok(Self { docker: d })
    }

    pub async fn version(&self) -> Result<bm::SystemVersion, DockerError> {
        Ok(self.docker.version().await?)
    }

    pub async fn info(&self) -> Result<bm::SystemInfo, DockerError> {
        Ok(self.docker.info().await?)
    }

    /// Every container (running or not).
    pub async fn containers(&self) -> Result<Vec<Seen>, DockerError> {
        let opts = qp::ListContainersOptions {
            all: true,
            ..Default::default()
        };
        let v = self.docker.list_containers(Some(opts)).await?;
        Ok(v.iter().map(model::container_of).collect())
    }

    pub async fn images(&self) -> Result<Vec<ImageSummary>, DockerError> {
        let v = self
            .docker
            .list_images(Some(qp::ListImagesOptions {
                all: false,
                ..Default::default()
            }))
            .await?;
        Ok(v.iter().map(model::image_of).collect())
    }

    pub async fn df(&self) -> Result<bm::SystemDataUsageResponse, DockerError> {
        Ok(self.docker.df(None::<qp::DataUsageOptions>).await?)
    }

    /// Volumes, with sizes (`name → (size, refs)`, [`model::df_volumes`]) when given.
    pub async fn volumes(
        &self,
        sizes: Option<&HashMap<String, (i64, i64)>>,
    ) -> Result<Vec<VolumeSummary>, DockerError> {
        let empty = HashMap::new();
        let sizes = sizes.unwrap_or(&empty);
        let r = self
            .docker
            .list_volumes(None::<qp::ListVolumesOptions>)
            .await?;
        let mut v: Vec<VolumeSummary> = r
            .volumes
            .unwrap_or_default()
            .iter()
            .map(|x| model::volume_of(x, sizes))
            .collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(v)
    }

    pub async fn networks(&self) -> Result<Vec<NetworkSummary>, DockerError> {
        let mut v: Vec<NetworkSummary> = self
            .docker
            .list_networks(None::<qp::ListNetworksOptions>)
            .await?
            .iter()
            .map(model::network_of)
            .collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(v)
    }

    pub async fn start(&self, id: &str) -> Result<(), DockerError> {
        Ok(self
            .docker
            .start_container(id, None::<qp::StartContainerOptions>)
            .await?)
    }

    pub async fn stop(&self, id: &str) -> Result<(), DockerError> {
        let o = qp::StopContainerOptions {
            t: Some(STOP_GRACE_S),
            ..Default::default()
        };
        Ok(self.docker.stop_container(id, Some(o)).await?)
    }

    pub async fn restart(&self, id: &str) -> Result<(), DockerError> {
        let o = qp::RestartContainerOptions {
            t: Some(STOP_GRACE_S),
            ..Default::default()
        };
        Ok(self.docker.restart_container(id, Some(o)).await?)
    }

    /// Does an image (id or reference) exist locally?
    pub async fn image_exists(&self, image: &str) -> bool {
        self.docker.inspect_image(image).await.is_ok()
    }

    /// The registry digest (`sha256:…`) of `image` now, without pulling.
    pub async fn registry_digest(&self, image: &str) -> Result<String, String> {
        match self.docker.inspect_registry_image(image, None).await {
            Ok(d) => d
                .descriptor
                .digest
                .ok_or_else(|| "the registry gave no digest".into()),
            Err(e) => Err(DockerError::from(e).to_string()),
        }
    }

    /// Log lines of a container: the last `tail` lines, then (with `follow`) new ones as
    /// they come.
    pub fn logs(
        &self,
        id: &str,
        tail: u32,
        follow: bool,
        timestamps: bool,
    ) -> impl Stream<Item = Result<LogLine, DockerError>> + Send + 'static + use<> {
        let o = qp::LogsOptions {
            follow,
            stdout: true,
            stderr: true,
            timestamps,
            tail: tail.min(MAX_TAIL).to_string(),
            ..Default::default()
        };
        self.docker.logs(id, Some(o)).map(|r| {
            r.map(|o| {
                let (stream, msg) = match o {
                    bollard::container::LogOutput::StdErr { message } => ("stderr", message),
                    bollard::container::LogOutput::StdOut { message }
                    | bollard::container::LogOutput::Console { message } => ("stdout", message),
                    bollard::container::LogOutput::StdIn { message } => ("stdin", message),
                };
                let mut line = String::from_utf8_lossy(&msg).into_owned();
                while line.ends_with('\n') || line.ends_with('\r') {
                    line.pop();
                }
                LogLine {
                    stream: stream.into(),
                    line,
                }
            })
            .map_err(DockerError::from)
        })
    }

    /// The engine's event stream (containers only).
    pub fn events(
        &self,
    ) -> impl Stream<Item = Result<bm::EventMessage, DockerError>> + Send + 'static + use<> {
        let mut filters = HashMap::new();
        filters.insert("type".to_string(), vec!["container".to_string()]);
        // no healthcheck exec_* or attach/resize noise (crate::events::FOLLOWED_ACTIONS)
        filters.insert(
            "event".to_string(),
            crate::events::FOLLOWED_ACTIONS
                .iter()
                .map(|a| a.to_string())
                .collect(),
        );
        let o = qp::EventsOptions {
            filters: Some(filters),
            ..Default::default()
        };
        self.docker
            .events(Some(o))
            .map(|r| r.map_err(DockerError::from))
    }
}

/// `true` when the error means the engine is not answering (as opposed to a refused call).
pub fn is_unavailable(e: &DockerError) -> bool {
    matches!(e, DockerError::Unavailable(_))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_map() {
        let e: DockerError = BErr::DockerResponseServerError {
            status_code: 404,
            message: "No such container: x".into(),
        }
        .into();
        assert_eq!(e, DockerError::NotFound("No such container: x".into()));
        let e: DockerError = BErr::DockerResponseServerError {
            status_code: 409,
            message: "in use".into(),
        }
        .into();
        assert!(matches!(e, DockerError::Conflict(_)));
        let e: DockerError = BErr::SocketNotFoundError("/var/run/docker.sock".into()).into();
        assert!(is_unavailable(&e));
    }

    #[tokio::test]
    async fn missing_socket_is_unavailable() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("docker.sock");
        let e = Engine::connect(p.to_str().unwrap()).await.err().unwrap();
        assert!(is_unavailable(&e), "{e:?}");
    }
}
