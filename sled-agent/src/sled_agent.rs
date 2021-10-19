//! Sled agent implementation

use crate::config::Config;
use crate::instance_manager::InstanceManager;
use crate::storage_manager::StorageManager;
use omicron_common::api::{
    external::Error, internal::nexus::DiskRuntimeState,
    internal::nexus::InstanceRuntimeState,
    internal::sled_agent::DiskStateRequested,
    internal::sled_agent::InstanceHardware,
    internal::sled_agent::InstanceRuntimeStateRequested,
};
use slog::Logger;
use std::sync::Arc;
use tokio::task::JoinHandle;
use uuid::Uuid;

#[cfg(test)]
use crate::mocks::MockNexusClient as NexusClient;
#[cfg(not(test))]
use omicron_common::NexusClient;

// TODO: I wanna make a task that continually reports the storage status
// upward to nexus.

/// Describes an executing Sled Agent object.
///
/// Contains both a connection to the Nexus, as well as managed instances.
pub struct SledAgent {
    storage: StorageManager,
    instances: InstanceManager,
    nexus_client: Arc<NexusClient>,

}

impl SledAgent {
    /// Initializes a new [`SledAgent`] object.
    pub async fn new(
        config: &Config,
        log: Logger,
        nexus_client: Arc<NexusClient>,
    ) -> Result<SledAgent, Error> {
        let id = &config.id;
        let vlan = config.vlan.clone();
        info!(&log, "created sled agent"; "id" => ?id);

        let storage = match &config.zpools {
            Some(pools) => StorageManager::new_from_zpools(pools.clone()).await?,
            None => StorageManager::new()?,
        };
        // TODO-nit: Could remove nexus_client from IM?
        // basically just one less place to store it, could be passed in
        // 'ensure'. idk.
        let instances = InstanceManager::new(log, vlan, nexus_client.clone())?;

        Ok(SledAgent { storage, instances, nexus_client })
    }

    /// Idempotently ensures that a given Instance is running on the sled.
    pub async fn instance_ensure(
        &self,
        instance_id: Uuid,
        initial: InstanceHardware,
        target: InstanceRuntimeStateRequested,
    ) -> Result<InstanceRuntimeState, Error> {
        self.instances.ensure(instance_id, initial, target).await
    }

    /// Idempotently ensures that the given virtual disk is attached (or not) as
    /// specified.
    ///
    /// NOTE: Not yet implemented.
    pub async fn disk_ensure(
        &self,
        _disk_id: Uuid,
        _initial_state: DiskRuntimeState,
        _target: DiskStateRequested,
    ) -> Result<DiskRuntimeState, Error> {
        todo!("Disk attachment not yet implemented");
    }
}
