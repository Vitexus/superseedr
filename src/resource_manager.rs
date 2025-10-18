// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::{HashMap, VecDeque};
use thiserror::Error;
use tokio::sync::{mpsc, oneshot};

// Process one batch of this many permits, then re-queue the work.
const PERMIT_GRANT_BATCH_SIZE: usize = 64;

#[derive(Debug)]
pub struct PermitGuard {
    pub resource_type: ResourceType,
    control_tx: mpsc::Sender<ControlCommand>,
}

impl Drop for PermitGuard {
    fn drop(&mut self) {
        let _ = self.control_tx.try_send(ControlCommand::Release {
            resource: self.resource_type,
        });
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
pub enum ResourceType {
    PeerConnection,
    DiskRead,
    DiskWrite,
}

#[derive(Error, Debug, Clone)]
pub enum ResourceManagerError {
    #[error("The resource manager has been shut down.")]
    ManagerShutdown,
    #[error("The request queue for the resource is full.")]
    QueueFull,
}

#[derive(Clone, Debug)]
pub struct ResourceManagerClient {
    acquire_txs: HashMap<ResourceType, mpsc::Sender<AcquireCommand>>,
    control_tx: mpsc::Sender<ControlCommand>,
}

impl ResourceManagerClient {
    pub async fn acquire_peer_connection(&self) -> Result<PermitGuard, ResourceManagerError> {
        self.acquire(ResourceType::PeerConnection).await
    }
    pub async fn acquire_disk_read(&self) -> Result<PermitGuard, ResourceManagerError> {
        self.acquire(ResourceType::DiskRead).await
    }
    pub async fn acquire_disk_write(&self) -> Result<PermitGuard, ResourceManagerError> {
        self.acquire(ResourceType::DiskWrite).await
    }

    pub async fn update_limits(
        &self,
        new_limits: HashMap<ResourceType, usize>,
    ) -> Result<(), ResourceManagerError> {
        let command = ControlCommand::UpdateLimits { limits: new_limits };
        self.control_tx
            .send(command)
            .await
            .map_err(|_| ResourceManagerError::ManagerShutdown)
    }

    async fn acquire(&self, resource: ResourceType) -> Result<PermitGuard, ResourceManagerError> {
        let (respond_to, rx) = oneshot::channel();
        let command = AcquireCommand { respond_to };
        let tx = self.acquire_txs.get(&resource).unwrap();

        tx.send(command)
            .await
            .map_err(|_| ResourceManagerError::ManagerShutdown)?;

        match rx.await {
            Ok(result) => result,
            Err(_) => Err(ResourceManagerError::ManagerShutdown),
        }
    }
}

#[derive(Debug)]
struct AcquireCommand {
    respond_to: oneshot::Sender<Result<PermitGuard, ResourceManagerError>>,
}

#[derive(Debug)]
pub enum ControlCommand {
    Release {
        resource: ResourceType,
    },
    UpdateLimits {
        limits: HashMap<ResourceType, usize>,
    },
    ProcessQueue {
        resource: ResourceType,
    },
}

pub struct ResourceManager {
    acquire_rxs: HashMap<ResourceType, mpsc::Receiver<AcquireCommand>>,
    control_rx: mpsc::Receiver<ControlCommand>,
    control_tx: mpsc::Sender<ControlCommand>,
    resources: HashMap<ResourceType, ResourceState>,
}

struct ResourceState {
    limit: usize,
    in_use: usize,
    max_queue_size: usize,
    wait_queue: VecDeque<oneshot::Sender<Result<PermitGuard, ResourceManagerError>>>,
}

impl ResourceManager {
    pub fn new(limits: HashMap<ResourceType, (usize, usize)>) -> (Self, ResourceManagerClient) {
        let (control_tx, control_rx) = mpsc::channel(256);
        let mut acquire_txs = HashMap::new();
        let mut acquire_rxs = HashMap::new();
        let mut resources = HashMap::new();

        let all_types = [
            ResourceType::PeerConnection,
            ResourceType::DiskRead,
            ResourceType::DiskWrite,
        ];

        for res_type in all_types.into_iter() {
            let (limit, max_queue_size) = limits.get(&res_type).copied().unwrap_or((0, 0));
            let (tx, rx) = mpsc::channel(256);
            acquire_txs.insert(res_type, tx);
            acquire_rxs.insert(res_type, rx);
            resources.insert(
                res_type,
                ResourceState {
                    limit,
                    in_use: 0,
                    max_queue_size,
                    wait_queue: VecDeque::new(),
                },
            );
        }

        let client = ResourceManagerClient {
            acquire_txs,
            control_tx: control_tx.clone(),
        };
        let actor = Self {
            acquire_rxs,
            control_rx,
            control_tx,
            resources,
        };
        (actor, client)
    }

    pub async fn run(mut self) {
        let mut peer_rx = self
            .acquire_rxs
            .remove(&ResourceType::PeerConnection)
            .unwrap();
        let mut read_rx = self.acquire_rxs.remove(&ResourceType::DiskRead).unwrap();
        let mut write_rx = self.acquire_rxs.remove(&ResourceType::DiskWrite).unwrap();

        loop {
            tokio::select! {
                // Now, each branch uses its own independent variable.
                Some(cmd) = peer_rx.recv() => self.handle_acquire(ResourceType::PeerConnection, cmd.respond_to),
                Some(cmd) = read_rx.recv() => self.handle_acquire(ResourceType::DiskRead, cmd.respond_to),
                Some(cmd) = write_rx.recv() => self.handle_acquire(ResourceType::DiskWrite, cmd.respond_to),

                Some(cmd) = self.control_rx.recv() => {
                    match cmd {
                        ControlCommand::Release { resource } => self.handle_release(resource),
                        ControlCommand::UpdateLimits { limits } => self.handle_update_limits(limits),
                        ControlCommand::ProcessQueue { resource } => self.handle_process_queue(resource),
                    }
                },
                else => { break; }
            }
        }
        println!("Resource Manager shut down.");
    }

    fn handle_acquire(
        &mut self,
        resource: ResourceType,
        respond_to: oneshot::Sender<Result<PermitGuard, ResourceManagerError>>,
    ) {
        let state = self.resources.get_mut(&resource).unwrap();

        if state.in_use < state.limit {
            state.in_use += 1;
            let guard = PermitGuard {
                resource_type: resource,
                control_tx: self.control_tx.clone(),
            };
            let _ = respond_to.send(Ok(guard));
        } else if state.wait_queue.len() < state.max_queue_size {
            state.wait_queue.push_back(respond_to);
        } else {
            let _ = respond_to.send(Err(ResourceManagerError::QueueFull));
        }
    }

    fn handle_release(&mut self, resource: ResourceType) {
        let state = self.resources.get_mut(&resource).unwrap();
        state.in_use = state.in_use.saturating_sub(1);
        let _ = self
            .control_tx
            .try_send(ControlCommand::ProcessQueue { resource });
    }

    fn handle_update_limits(&mut self, limits: HashMap<ResourceType, usize>) {
        for (resource, new_limit) in limits {
            if let Some(state) = self.resources.get_mut(&resource) {
                state.limit = new_limit;
                let _ = self
                    .control_tx
                    .try_send(ControlCommand::ProcessQueue { resource });
            }
        }
    }

    fn handle_process_queue(&mut self, resource: ResourceType) {
        let state = self.resources.get_mut(&resource).unwrap();
        for _ in 0..PERMIT_GRANT_BATCH_SIZE {
            if state.in_use >= state.limit {
                return;
            }
            if let Some(next_in_line) = state.wait_queue.pop_front() {
                if !next_in_line.is_closed() {
                    state.in_use += 1;
                    let guard = PermitGuard {
                        resource_type: resource,
                        control_tx: self.control_tx.clone(),
                    };
                    if next_in_line.send(Ok(guard)).is_err() {
                        state.in_use -= 1;
                    }
                }
            } else {
                return;
            }
        }
        if state.in_use < state.limit && !state.wait_queue.is_empty() {
            let _ = self
                .control_tx
                .try_send(ControlCommand::ProcessQueue { resource });
        }
    }
}
