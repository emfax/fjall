use crate::{
    config::Config,
    file::{FJALL_MARKER, JOURNALS_FOLDER, PARTITIONS_FOLDER},
    //_journal::Journal,
    version::Version,
    PartitionHandle,
};
use lsm_tree::{SequenceNumberCounter, Tree as LsmTree};
use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

type Partitions = HashMap<Arc<str>, LsmTree>;

#[allow(clippy::module_name_repetitions)]
pub struct KeyspaceInner {
    pub(crate) partitions: Arc<RwLock<Partitions>>,
    // pub(crate) journal: Journal,
    pub(crate) config: Config,
    pub(crate) seqno: SequenceNumberCounter,
}

/// The keyspace houses multiple partitions (column families).
#[derive(Clone)]
#[doc(alias = "database")]
#[doc(alias = "table")]
#[doc(alias = "collection")]
pub struct Keyspace(pub(crate) Arc<KeyspaceInner>);

impl std::ops::Deref for Keyspace {
    type Target = KeyspaceInner;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

pub struct PartitionConfig {}

// TODO: flush thread

impl Keyspace {
    /// Flushes the active journal, making sure recently written data is durable
    ///
    /// This has a dramatic, negative performance impact by 100-1000x.
    ///
    /// Persisting only affects durability, NOT consistency! Even without flushing
    /// the journal (and all other parts) are (or should be) crash-safe.
    ///
    /// # Errors
    ///
    /// Returns error, if an IO error occured.
    pub fn persist(&self) -> crate::Result<()> {
        // TODO:
        Ok(())
    }

    /// Opens a keyspace in the given directory.
    ///
    /// # Errors
    ///
    /// Returns error, if an IO error occured.
    pub fn open(config: Config) -> crate::Result<Self> {
        log::debug!("Opening keyspace at {}", config.path.display());

        let keyspace = if config.path.join(FJALL_MARKER).try_exists()? {
            Self::recover(config)
        } else {
            Self::create_new(config)
        }?;

        Ok(keyspace)
    }

    /// Gives access to a keyspace partition.
    ///
    /// # Errors
    ///
    /// Returns error, if an IO error occured.
    pub fn open_partition(
        &self,
        name: &str,
        // config: PartitionConfig,
    ) -> crate::Result<PartitionHandle> {
        // TODO: limit naming of partition to a-zA-Z0-9_-

        let partitions = self.partitions.write().expect("lock is poisoned");

        let tree = if let Some(tree) = partitions.get(name) {
            tree.clone()
        } else {
            log::debug!("Opening partition {name}");

            let partitions_folder = self.config.path.join(PARTITIONS_FOLDER);
            let path = partitions_folder.join(name);

            let tree = lsm_tree::Config::new(path)
                .block_cache(self.config.block_cache.clone())
                .open()?;

            #[cfg(not(target_os = "windows"))]
            {
                // fsync folder on Unix
                let folder = std::fs::File::open(&partitions_folder)?;
                folder.sync_all()?;
            }

            // TODO: 0.3.0 hmmm... unless all partitions are loaded
            // TODO: the seqno may be wrong
            // TODO: so a simple user error could make the db inconsistent (not broken, but inconsistent...)

            // TODO: another big problem... all partitions need to be loaded for
            // TODO: journal GC to work... so we NEED to load all partitions... FUCK
            // TODO:
            // TODO: split open_partition and create_partition
            // TODO: open_partition will have a Runtime config, create will have a disk-backed, immutable PartitionConfig

            let tree_next_seqno = tree.get_next_seqno();
            self.seqno
                .fetch_max(tree_next_seqno, std::sync::atomic::Ordering::AcqRel);

            tree
        };

        Ok(PartitionHandle {
            keyspace: self.clone(),
            tree,
        })
    }

    /// Recovers existing keyspace from directory
    fn recover(config: Config) -> crate::Result<Self> {
        let inner = KeyspaceInner {
            //  journal: Journal::recover(config.path.join(JOURNALS_FOLDER).join("active")),
            partitions: Arc::default(),
            config,
            seqno: SequenceNumberCounter::default(),
        };

        Ok(Self(Arc::new(inner)))
    }

    /// Lists all partitions
    ///
    /// # Errors
    ///
    /// Returns error, if an IO error occured.
    pub fn list_partitions(&self) -> crate::Result<Vec<Arc<str>>> {
        let path = self.config.path.join(PARTITIONS_FOLDER);

        let dirents = std::fs::read_dir(path)?.collect::<std::io::Result<Vec<_>>>()?;

        Ok(dirents
            .into_iter()
            .map(|x| x.file_name().to_str().expect("should be valid name").into())
            .collect())
    }

    fn create_new(config: Config) -> crate::Result<Self> {
        let path = config.path.clone();
        log::info!("Creating keyspace at {}", path.display());

        std::fs::create_dir_all(&path)?;

        let marker_path = path.join(FJALL_MARKER);
        assert!(!marker_path.try_exists()?);

        std::fs::create_dir_all(path.join(JOURNALS_FOLDER))?;
        std::fs::create_dir_all(path.join(PARTITIONS_FOLDER))?;

        let inner = KeyspaceInner {
            //  journal: Journal::create_new(path.join(JOURNALS_FOLDER).join("active")),
            partitions: Arc::default(),
            config,
            seqno: SequenceNumberCounter::default(),
        };

        // NOTE: Lastly, fsync .fjall marker, which contains the version
        // -> the DB is fully initialized

        let mut file = std::fs::File::create(marker_path)?;
        Version::V0.write_file_header(&mut file)?;
        file.sync_all()?;

        #[cfg(not(target_os = "windows"))]
        {
            // fsync folders on Unix

            let folder = std::fs::File::open(path.join(JOURNALS_FOLDER))?;
            folder.sync_all()?;

            let folder = std::fs::File::open(path.join(PARTITIONS_FOLDER))?;
            folder.sync_all()?;

            let folder = std::fs::File::open(&path)?;
            folder.sync_all()?;
        }

        Ok(Self(Arc::new(inner)))
    }
}
