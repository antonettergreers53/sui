// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use crate::authority::authority_store_pruner::{
    AuthorityStorePruner, AuthorityStorePruningMetrics,
};
use crate::authority::authority_store_tables::AuthorityPerpetualTables;
use crate::checkpoints::CheckpointStore;
use anyhow::{Context, Result};
use bytes::Bytes;
use futures::future::try_join_all;
use object_store::path::Path;
use object_store::{DynObjectStore, Error};
use oneshot::channel;
use prometheus::{register_int_gauge_with_registry, IntGauge, Registry};
use std::collections::BTreeMap;
use std::fs;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use sui_config::node::AuthorityStorePruningConfig;
use sui_storage::mutex_table::RwLockTable;
use sui_storage::object_store::util::{copy_recursively, path_to_filesystem, put};
use sui_storage::object_store::{ObjectStoreConfig, ObjectStoreType};
use tokio::sync::oneshot;
use tokio::sync::oneshot::Sender;
use tracing::{debug, error, info, warn};
use typed_store::rocks::MetricConf;

pub const SUCCESS_MARKER: &str = "_SUCCESS";
pub const TEST_MARKER: &str = "_TEST";
pub const UPLOAD_COMPLETED_MARKER: &str = "_UPLOAD_COMPLETED";

pub struct DBCheckpointMetrics {
    pub first_missing_db_checkpoint_epoch: IntGauge,
}

impl DBCheckpointMetrics {
    pub fn new(registry: &Registry) -> Arc<Self> {
        let this = Self {
            first_missing_db_checkpoint_epoch: register_int_gauge_with_registry!(
                "first_missing_db_checkpoint_epoch",
                "First epoch for which we have no db checkpoint in remote store",
                registry
            )
            .unwrap(),
        };
        Arc::new(this)
    }
}

pub struct DBCheckpointHandler {
    /// Directory on local disk where db checkpoints are stored
    input_object_store: Arc<DynObjectStore>,
    /// DB checkpoint directory on local filesystem
    input_root_path: PathBuf,
    /// Bucket on cloud object store where db checkpoints will be copied
    output_object_store: Arc<DynObjectStore>,
    /// Time interval to check for presence of new db checkpoint
    interval: Duration,
    /// File markers which signal that local db checkpoint can be garbage collected
    gc_markers: Vec<String>,
    /// Boolean flag to enable/disable object pruning and manual compaction before upload
    prune_and_compact_before_upload: bool,
    /// Indirect object config for pruner
    indirect_objects_threshold: usize,
    /// Pruning objects
    pruning_config: AuthorityStorePruningConfig,
    metrics: Arc<DBCheckpointMetrics>,
}

impl DBCheckpointHandler {
    pub fn new(
        input_path: &std::path::Path,
        output_object_store_config: &ObjectStoreConfig,
        interval_s: u64,
        prune_and_compact_before_upload: bool,
        indirect_objects_threshold: usize,
        pruning_config: AuthorityStorePruningConfig,
        registry: &Registry,
    ) -> Result<Self> {
        let input_store_config = ObjectStoreConfig {
            object_store: Some(ObjectStoreType::File),
            directory: Some(input_path.to_path_buf()),
            ..Default::default()
        };
        Ok(DBCheckpointHandler {
            input_object_store: input_store_config.make()?,
            input_root_path: input_path.to_path_buf(),
            output_object_store: output_object_store_config.make()?,
            interval: Duration::from_secs(interval_s),
            gc_markers: vec![UPLOAD_COMPLETED_MARKER.to_string()],
            prune_and_compact_before_upload,
            indirect_objects_threshold,
            pruning_config,
            metrics: DBCheckpointMetrics::new(registry),
        })
    }
    pub fn new_for_test(
        input_object_store_config: &ObjectStoreConfig,
        output_object_store_config: &ObjectStoreConfig,
        interval_s: u64,
        prune_and_compact_before_upload: bool,
    ) -> Result<Self> {
        Ok(DBCheckpointHandler {
            input_object_store: input_object_store_config.make()?,
            input_root_path: input_object_store_config
                .directory
                .as_ref()
                .unwrap()
                .clone(),
            output_object_store: output_object_store_config.make()?,
            interval: Duration::from_secs(interval_s),
            gc_markers: vec![UPLOAD_COMPLETED_MARKER.to_string(), TEST_MARKER.to_string()],
            prune_and_compact_before_upload,
            indirect_objects_threshold: 0,
            pruning_config: AuthorityStorePruningConfig::default(),
            metrics: DBCheckpointMetrics::new(&Registry::default()),
        })
    }
    pub fn start(self) -> Sender<()> {
        let (sender, mut recv) = channel::<()>();
        let mut interval = tokio::time::interval(self.interval);
        let mut gc_interval = tokio::time::interval(Duration::from_secs(30));
        tokio::task::spawn(async move {
            info!("DB checkpoint handler loop started");
            loop {
                tokio::select! {
                    _now = interval.tick() => {
                        if let Ok(epochs) = self.find_all_missing_checkpoint_epochs().await {
                            self.metrics.first_missing_db_checkpoint_epoch.set(epochs.first().cloned().map(|x| x as i64).unwrap_or(0));
                            if let Err(err) = self.upload_db_checkpoints_to_object_store(epochs).await {
                                error!("Failed to upload db checkpoint to remote store with err: {:?}", err);
                            }
                        } else {
                            error!("Failed to find missing db checkpoints");
                        }
                    },
                    _ = gc_interval.tick() => {
                        if let Ok(deleted) = self.garbage_collect_old_db_checkpoints().await {
                            if !deleted.is_empty() {
                                info!("Garbage collected local db checkpoints: {:?}", deleted);
                            }
                        }
                    },
                    _ = &mut recv => break,
                }
            }
        });
        sender
    }
    async fn prune_and_compact(&self, db_path: PathBuf, epoch: u32) -> Result<()> {
        let perpetual_db = Arc::new(AuthorityPerpetualTables::open(&db_path.join("store"), None));
        let checkpoint_store = Arc::new(CheckpointStore::open_tables_read_write(
            db_path.join("checkpoints"),
            MetricConf::default(),
            None,
            None,
        ));
        let metrics = AuthorityStorePruningMetrics::new(&Registry::default());
        let lock_table = Arc::new(RwLockTable::new(1));
        info!(
            "Pruning db checkpoint in {:?} for epoch: {epoch}",
            db_path.display()
        );
        AuthorityStorePruner::prune_objects_for_eligible_epochs(
            &perpetual_db,
            &checkpoint_store,
            &lock_table,
            self.pruning_config,
            metrics,
            self.indirect_objects_threshold,
        )
        .await?;
        info!(
            "Compacting db checkpoint in {:?} for epoch: {epoch}",
            db_path.display()
        );
        AuthorityStorePruner::compact(&perpetual_db)?;
        Ok(())
    }
    async fn find_all_missing_checkpoint_epochs(&self) -> Result<Vec<u32>> {
        let remote_checkpoints_by_epoch = self
            .read_checkpoint_dir(self.output_object_store.clone())
            .await?;
        let mut dirs: Vec<_> = remote_checkpoints_by_epoch.iter().collect();
        dirs.sort_by_key(|(epoch_num, _path)| *epoch_num);
        let mut candidate_epoch: u32 = 0;
        let mut missing_epochs = Vec::new();
        for (epoch_num, path) in dirs {
            while candidate_epoch < *epoch_num {
                // The whole epoch directory is missing
                missing_epochs.push(candidate_epoch);
                candidate_epoch += 1;
                continue;
            }
            let success_marker = path.child(SUCCESS_MARKER);
            let get_result = self.output_object_store.get(&success_marker).await;
            match get_result {
                Err(Error::NotFound { .. }) => {
                    error!("No success marker found in db checkpoint for epoch: {epoch_num}");
                    missing_epochs.push(*epoch_num);
                }
                Err(_) => {
                    // Probably a transient error
                    warn!("Failed while trying to read success marker in db checkpoint for epoch: {epoch_num}");
                }
                Ok(_) => {
                    // Nothing to do
                }
            }
            candidate_epoch += 1
        }
        missing_epochs.push(candidate_epoch);
        Ok(missing_epochs)
    }
    async fn upload_db_checkpoints_to_object_store(&self, missing_epochs: Vec<u32>) -> Result<()> {
        let last_missing_epoch = missing_epochs.last().cloned().unwrap_or(0);
        let local_checkpoints_by_epoch = self
            .read_checkpoint_dir(self.input_object_store.clone())
            .await?;
        let mut dirs: Vec<_> = local_checkpoints_by_epoch.iter().collect();
        dirs.sort_by_key(|(epoch_num, _path)| *epoch_num);
        for (epoch, db_path) in dirs {
            if missing_epochs.contains(epoch) || *epoch >= last_missing_epoch {
                if self.prune_and_compact_before_upload {
                    // Convert `db_path` to the local filesystem path to where db checkpoint is stored
                    let local_db_path = path_to_filesystem(self.input_root_path.clone(), db_path)?;
                    // Invoke pruning and compaction on the db checkpoint
                    self.prune_and_compact(local_db_path, *epoch).await?;
                }
                info!("Copying db checkpoint for epoch: {epoch} to remote storage");
                copy_recursively(
                    db_path,
                    self.input_object_store.clone(),
                    self.output_object_store.clone(),
                    NonZeroUsize::new(20).unwrap(),
                )
                .await?;
                // Drop marker in the output directory that upload completed successfully
                let bytes = Bytes::from_static(b"success");
                let success_marker = db_path.child(SUCCESS_MARKER);
                put(
                    &success_marker,
                    bytes.clone(),
                    self.output_object_store.clone(),
                )
                .await?;
            }
            let bytes = Bytes::from_static(b"success");
            let upload_completed_marker = db_path.child(UPLOAD_COMPLETED_MARKER);
            put(
                &upload_completed_marker,
                bytes.clone(),
                self.input_object_store.clone(),
            )
            .await?;
        }
        Ok(())
    }
    async fn garbage_collect_old_db_checkpoints(&self) -> Result<Vec<u32>> {
        let local_checkpoints_by_epoch = self
            .read_checkpoint_dir(self.input_object_store.clone())
            .await?;
        let mut deleted = Vec::new();
        for (epoch, path) in local_checkpoints_by_epoch.iter() {
            let marker_paths: Vec<Path> = self
                .gc_markers
                .iter()
                .map(|marker| path.child(marker.clone()))
                .collect();
            let all_markers_present = try_join_all(
                marker_paths
                    .iter()
                    .map(|path| self.input_object_store.get(path)),
            )
            .await;
            match all_markers_present {
                // After state snapshots, gc will also need to wait for a state snapshot
                // upload completed marker
                Ok(_) => {
                    info!("Deleting db checkpoint dir: {path} for epoch: {epoch}");
                    deleted.push(*epoch);
                    let local_fs_path = path_to_filesystem(self.input_root_path.clone(), path)?;
                    fs::remove_dir_all(&local_fs_path)?;
                }
                Err(_) => {
                    debug!("Not ready for deletion yet: {path}");
                }
            }
        }
        Ok(deleted)
    }
    async fn read_checkpoint_dir(&self, store: Arc<DynObjectStore>) -> Result<BTreeMap<u32, Path>> {
        let mut checkpoints_by_epoch = BTreeMap::new();
        let entries = store.list_with_delimiter(None).await?;
        for entry in entries.common_prefixes {
            if let Some(filename) = entry.filename() {
                if !filename.starts_with("epoch_") {
                    continue;
                }
                let epoch = filename
                    .split_once('_')
                    .context("Failed to split dir name")
                    .map(|(_, epoch)| epoch.parse::<u32>())??;
                checkpoints_by_epoch.insert(epoch, entry);
            }
        }
        Ok(checkpoints_by_epoch)
    }
}

#[cfg(test)]
mod tests {
    use crate::db_checkpoint_handler::{
        DBCheckpointHandler, SUCCESS_MARKER, TEST_MARKER, UPLOAD_COMPLETED_MARKER,
    };
    use itertools::Itertools;
    use std::fs;
    use sui_storage::object_store::util::path_to_filesystem;
    use sui_storage::object_store::{ObjectStoreConfig, ObjectStoreType};
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_basic() -> anyhow::Result<()> {
        let checkpoint_dir = TempDir::new()?;
        let checkpoint_dir_path = checkpoint_dir.path();
        let local_epoch0_checkpoint = checkpoint_dir_path.join("epoch_0");
        fs::create_dir(&local_epoch0_checkpoint)?;
        let file1 = local_epoch0_checkpoint.join("file1");
        fs::write(file1, b"Lorem ipsum")?;
        let file2 = local_epoch0_checkpoint.join("file2");
        fs::write(file2, b"Lorem ipsum")?;
        let nested_dir = local_epoch0_checkpoint.join("data");
        fs::create_dir(&nested_dir)?;
        let file3 = nested_dir.join("file3");
        fs::write(file3, b"Lorem ipsum")?;

        let remote_checkpoint_dir = TempDir::new()?;
        let remote_checkpoint_dir_path = remote_checkpoint_dir.path();
        let remote_epoch0_checkpoint = remote_checkpoint_dir_path.join("epoch_0");

        let input_store_config = ObjectStoreConfig {
            object_store: Some(ObjectStoreType::File),
            directory: Some(checkpoint_dir_path.to_path_buf()),
            ..Default::default()
        };
        let output_store_config = ObjectStoreConfig {
            object_store: Some(ObjectStoreType::File),
            directory: Some(remote_checkpoint_dir_path.to_path_buf()),
            ..Default::default()
        };
        let db_checkpoint_handler = DBCheckpointHandler::new_for_test(
            &input_store_config,
            &output_store_config,
            10,
            false,
        )?;
        let local_checkpoints_by_epoch = db_checkpoint_handler
            .read_checkpoint_dir(db_checkpoint_handler.input_object_store.clone())
            .await?;
        assert!(!local_checkpoints_by_epoch.is_empty());
        assert_eq!(*local_checkpoints_by_epoch.first_key_value().unwrap().0, 0);
        assert_eq!(
            path_to_filesystem(
                db_checkpoint_handler.input_root_path.clone(),
                local_checkpoints_by_epoch.first_key_value().unwrap().1
            )
            .unwrap(),
            std::fs::canonicalize(local_epoch0_checkpoint.clone()).unwrap()
        );
        let missing_epochs = db_checkpoint_handler
            .find_all_missing_checkpoint_epochs()
            .await?;
        db_checkpoint_handler
            .upload_db_checkpoints_to_object_store(missing_epochs)
            .await?;

        assert!(remote_epoch0_checkpoint.join("file1").exists());
        assert!(remote_epoch0_checkpoint.join("file2").exists());
        assert!(remote_epoch0_checkpoint.join("data").join("file3").exists());
        assert!(remote_epoch0_checkpoint.join(SUCCESS_MARKER).exists());
        assert!(local_epoch0_checkpoint
            .join(UPLOAD_COMPLETED_MARKER)
            .exists());

        // Drop an extra gc marker meant only for gc to trigger
        let test_marker = local_epoch0_checkpoint.join(TEST_MARKER);
        fs::write(test_marker, b"Lorem ipsum")?;
        db_checkpoint_handler
            .garbage_collect_old_db_checkpoints()
            .await?;

        assert!(!local_epoch0_checkpoint.join("file1").exists());
        assert!(!local_epoch0_checkpoint.join("file1").exists());
        assert!(!local_epoch0_checkpoint.join("file2").exists());
        assert!(!local_epoch0_checkpoint.join("data").join("file3").exists());

        Ok(())
    }

    #[tokio::test]
    async fn test_upload_resumes() -> anyhow::Result<()> {
        let checkpoint_dir = TempDir::new()?;
        let checkpoint_dir_path = checkpoint_dir.path();
        let local_epoch0_checkpoint = checkpoint_dir_path.join("epoch_0");

        let remote_checkpoint_dir = TempDir::new()?;
        let remote_checkpoint_dir_path = remote_checkpoint_dir.path();
        let remote_epoch0_checkpoint = remote_checkpoint_dir_path.join("epoch_0");

        let input_store_config = ObjectStoreConfig {
            object_store: Some(ObjectStoreType::File),
            directory: Some(checkpoint_dir_path.to_path_buf()),
            ..Default::default()
        };
        let output_store_config = ObjectStoreConfig {
            object_store: Some(ObjectStoreType::File),
            directory: Some(remote_checkpoint_dir_path.to_path_buf()),
            ..Default::default()
        };
        let db_checkpoint_handler = DBCheckpointHandler::new_for_test(
            &input_store_config,
            &output_store_config,
            10,
            false,
        )?;

        fs::create_dir(&local_epoch0_checkpoint)?;
        let file1 = local_epoch0_checkpoint.join("file1");
        fs::write(file1, b"Lorem ipsum")?;
        let file2 = local_epoch0_checkpoint.join("file2");
        fs::write(file2, b"Lorem ipsum")?;
        let nested_dir = local_epoch0_checkpoint.join("data");
        fs::create_dir(&nested_dir)?;
        let file3 = nested_dir.join("file3");
        fs::write(file3, b"Lorem ipsum")?;

        let missing_epochs = db_checkpoint_handler
            .find_all_missing_checkpoint_epochs()
            .await?;
        db_checkpoint_handler
            .upload_db_checkpoints_to_object_store(missing_epochs)
            .await?;
        assert!(remote_epoch0_checkpoint.join("file1").exists());
        assert!(remote_epoch0_checkpoint.join("file2").exists());
        assert!(remote_epoch0_checkpoint.join("data").join("file3").exists());
        assert!(remote_epoch0_checkpoint.join(SUCCESS_MARKER).exists());
        assert!(local_epoch0_checkpoint
            .join(UPLOAD_COMPLETED_MARKER)
            .exists());

        // Add a new db checkpoint to the local checkpoint directory
        let local_epoch1_checkpoint = checkpoint_dir_path.join("epoch_1");
        fs::create_dir(&local_epoch1_checkpoint)?;
        let file1 = local_epoch1_checkpoint.join("file1");
        fs::write(file1, b"Lorem ipsum")?;
        let file2 = local_epoch1_checkpoint.join("file2");
        fs::write(file2, b"Lorem ipsum")?;
        let nested_dir = local_epoch1_checkpoint.join("data");
        fs::create_dir(&nested_dir)?;
        let file3 = nested_dir.join("file3");
        fs::write(file3, b"Lorem ipsum")?;

        // Now delete the success marker from remote checkpointed directory
        // This is the scenario where uploads stops mid way because system stopped
        fs::remove_file(remote_epoch0_checkpoint.join(SUCCESS_MARKER))?;

        // Checkpoint handler should copy checkpoint for epoch_0 first before copying
        // epoch_1
        let missing_epochs = db_checkpoint_handler
            .find_all_missing_checkpoint_epochs()
            .await?;
        db_checkpoint_handler
            .upload_db_checkpoints_to_object_store(missing_epochs)
            .await?;
        assert!(remote_epoch0_checkpoint.join("file1").exists());
        assert!(remote_epoch0_checkpoint.join("file2").exists());
        assert!(remote_epoch0_checkpoint.join("data").join("file3").exists());
        assert!(remote_epoch0_checkpoint.join(SUCCESS_MARKER).exists());
        assert!(local_epoch0_checkpoint
            .join(UPLOAD_COMPLETED_MARKER)
            .exists());

        let remote_epoch1_checkpoint = remote_checkpoint_dir_path.join("epoch_1");
        assert!(remote_epoch1_checkpoint.join("file1").exists());
        assert!(remote_epoch1_checkpoint.join("file2").exists());
        assert!(remote_epoch1_checkpoint.join("data").join("file3").exists());
        assert!(remote_epoch1_checkpoint.join(SUCCESS_MARKER).exists());
        assert!(local_epoch1_checkpoint
            .join(UPLOAD_COMPLETED_MARKER)
            .exists());

        // Drop an extra gc marker meant only for gc to trigger
        let test_marker = local_epoch0_checkpoint.join(TEST_MARKER);
        fs::write(test_marker, b"Lorem ipsum")?;
        let test_marker = local_epoch1_checkpoint.join(TEST_MARKER);
        fs::write(test_marker, b"Lorem ipsum")?;

        db_checkpoint_handler
            .garbage_collect_old_db_checkpoints()
            .await?;
        assert!(!local_epoch0_checkpoint.join("file1").exists());
        assert!(!local_epoch0_checkpoint.join("file1").exists());
        assert!(!local_epoch0_checkpoint.join("file2").exists());
        assert!(!local_epoch0_checkpoint.join("data").join("file3").exists());
        assert!(!local_epoch1_checkpoint.join("file1").exists());
        assert!(!local_epoch1_checkpoint.join("file1").exists());
        assert!(!local_epoch1_checkpoint.join("file2").exists());
        assert!(!local_epoch1_checkpoint.join("data").join("file3").exists());
        Ok(())
    }

    #[tokio::test]
    async fn test_missing_epochs() -> anyhow::Result<()> {
        let checkpoint_dir = TempDir::new()?;
        let checkpoint_dir_path = checkpoint_dir.path();
        let local_epoch0_checkpoint = checkpoint_dir_path.join("epoch_0");
        fs::create_dir(&local_epoch0_checkpoint)?;
        let local_epoch1_checkpoint = checkpoint_dir_path.join("epoch_1");
        fs::create_dir(&local_epoch1_checkpoint)?;
        // Missing epoch 2
        let local_epoch3_checkpoint = checkpoint_dir_path.join("epoch_3");
        fs::create_dir(&local_epoch3_checkpoint)?;
        let remote_checkpoint_dir = TempDir::new()?;
        let remote_checkpoint_dir_path = remote_checkpoint_dir.path();

        let input_store_config = ObjectStoreConfig {
            object_store: Some(ObjectStoreType::File),
            directory: Some(checkpoint_dir_path.to_path_buf()),
            ..Default::default()
        };

        let output_store_config = ObjectStoreConfig {
            object_store: Some(ObjectStoreType::File),
            directory: Some(remote_checkpoint_dir_path.to_path_buf()),
            ..Default::default()
        };
        let db_checkpoint_handler = DBCheckpointHandler::new_for_test(
            &input_store_config,
            &output_store_config,
            10,
            false,
        )?;

        let missing_epochs = db_checkpoint_handler
            .find_all_missing_checkpoint_epochs()
            .await?;
        db_checkpoint_handler
            .upload_db_checkpoints_to_object_store(missing_epochs)
            .await?;

        let first_missing_epoch = db_checkpoint_handler
            .find_all_missing_checkpoint_epochs()
            .await?
            .first()
            .cloned()
            .unwrap();
        assert_eq!(first_missing_epoch, 2);

        let remote_epoch0_checkpoint = remote_checkpoint_dir_path.join("epoch_0");
        fs::remove_file(remote_epoch0_checkpoint.join(SUCCESS_MARKER))?;

        let first_missing_epoch = db_checkpoint_handler
            .find_all_missing_checkpoint_epochs()
            .await?
            .first()
            .cloned()
            .unwrap();
        assert_eq!(first_missing_epoch, 0);

        Ok(())
    }

    #[tokio::test]
    async fn test_range_missing_epochs() -> anyhow::Result<()> {
        let checkpoint_dir = TempDir::new()?;
        let checkpoint_dir_path = checkpoint_dir.path();
        let local_epoch100_checkpoint = checkpoint_dir_path.join("epoch_100");
        fs::create_dir(&local_epoch100_checkpoint)?;
        let local_epoch200_checkpoint = checkpoint_dir_path.join("epoch_200");
        fs::create_dir(&local_epoch200_checkpoint)?;
        let remote_checkpoint_dir = TempDir::new()?;
        let remote_checkpoint_dir_path = remote_checkpoint_dir.path();

        let input_store_config = ObjectStoreConfig {
            object_store: Some(ObjectStoreType::File),
            directory: Some(checkpoint_dir_path.to_path_buf()),
            ..Default::default()
        };

        let output_store_config = ObjectStoreConfig {
            object_store: Some(ObjectStoreType::File),
            directory: Some(remote_checkpoint_dir_path.to_path_buf()),
            ..Default::default()
        };
        let db_checkpoint_handler = DBCheckpointHandler::new_for_test(
            &input_store_config,
            &output_store_config,
            10,
            false,
        )?;

        let missing_epochs = db_checkpoint_handler
            .find_all_missing_checkpoint_epochs()
            .await?;
        assert_eq!(missing_epochs, vec![0]);
        db_checkpoint_handler
            .upload_db_checkpoints_to_object_store(missing_epochs)
            .await?;

        let missing_epochs = db_checkpoint_handler
            .find_all_missing_checkpoint_epochs()
            .await?;
        let mut expected_missing_epochs: Vec<u32> = (0..100).collect();
        expected_missing_epochs.extend((101..200).collect_vec().iter());
        expected_missing_epochs.push(201);
        assert_eq!(missing_epochs, expected_missing_epochs);
        Ok(())
    }
}
