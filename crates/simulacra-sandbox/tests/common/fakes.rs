use super::*;

#[derive(Default)]
pub struct SpyFs {
    pub inner: MemoryFs,
    pub reads: Mutex<Vec<String>>,
    pub writes: Mutex<Vec<(String, Vec<u8>)>>,
    pub lists: Mutex<Vec<String>>,
}

impl SpyFs {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn seed_file(&self, path: &str, data: &[u8]) {
        self.inner
            .write(path, data)
            .expect("seed write should succeed");
    }

    pub fn clear_observations(&self) {
        self.reads.lock().unwrap().clear();
        self.writes.lock().unwrap().clear();
        self.lists.lock().unwrap().clear();
    }

    pub fn read_count(&self) -> usize {
        self.reads.lock().unwrap().len()
    }

    pub fn write_count(&self) -> usize {
        self.writes.lock().unwrap().len()
    }

    #[allow(dead_code)]
    pub fn list_count(&self) -> usize {
        self.lists.lock().unwrap().len()
    }
}

impl VirtualFs for SpyFs {
    fn read(&self, path: &str) -> Result<Vec<u8>, VfsError> {
        self.reads.lock().unwrap().push(path.to_string());
        self.inner.read(path)
    }

    fn write(&self, path: &str, data: &[u8]) -> Result<(), VfsError> {
        self.writes
            .lock()
            .unwrap()
            .push((path.to_string(), data.to_vec()));
        self.inner.write(path, data)
    }

    fn exists(&self, path: &str) -> bool {
        self.inner.exists(path)
    }

    fn list_dir(&self, path: &str) -> Result<Vec<String>, VfsError> {
        self.lists.lock().unwrap().push(path.to_string());
        self.inner.list_dir(path)
    }

    fn mkdir(&self, path: &str) -> Result<(), VfsError> {
        self.inner.mkdir(path)
    }

    fn remove(&self, path: &str) -> Result<(), VfsError> {
        self.inner.remove(path)
    }

    fn metadata(&self, path: &str) -> Result<FsMetadata, VfsError> {
        self.inner.metadata(path)
    }

    fn snapshot(&self) -> Result<VfsSnapshot, VfsError> {
        self.inner.snapshot()
    }

    fn restore(&self, snapshot: &VfsSnapshot) -> Result<(), VfsError> {
        self.inner.restore(snapshot)
    }
}

pub struct SlowWriteFs {
    pub inner: MemoryFs,
    pub delay: Duration,
}

impl SlowWriteFs {
    pub fn new(delay: Duration) -> Self {
        Self {
            inner: MemoryFs::new(),
            delay,
        }
    }
}

impl VirtualFs for SlowWriteFs {
    fn read(&self, path: &str) -> Result<Vec<u8>, VfsError> {
        self.inner.read(path)
    }

    fn write(&self, path: &str, data: &[u8]) -> Result<(), VfsError> {
        thread::sleep(self.delay);
        self.inner.write(path, data)
    }

    fn exists(&self, path: &str) -> bool {
        self.inner.exists(path)
    }

    fn list_dir(&self, path: &str) -> Result<Vec<String>, VfsError> {
        self.inner.list_dir(path)
    }

    fn mkdir(&self, path: &str) -> Result<(), VfsError> {
        self.inner.mkdir(path)
    }

    fn remove(&self, path: &str) -> Result<(), VfsError> {
        self.inner.remove(path)
    }

    fn metadata(&self, path: &str) -> Result<FsMetadata, VfsError> {
        self.inner.metadata(path)
    }

    fn snapshot(&self) -> Result<VfsSnapshot, VfsError> {
        self.inner.snapshot()
    }

    fn restore(&self, snapshot: &VfsSnapshot) -> Result<(), VfsError> {
        self.inner.restore(snapshot)
    }
}

pub struct PanicWriteFs {
    pub inner: MemoryFs,
}

impl PanicWriteFs {
    pub fn new() -> Self {
        Self {
            inner: MemoryFs::new(),
        }
    }
}

impl VirtualFs for PanicWriteFs {
    fn read(&self, path: &str) -> Result<Vec<u8>, VfsError> {
        self.inner.read(path)
    }

    fn write(&self, path: &str, _data: &[u8]) -> Result<(), VfsError> {
        panic!("intentional panic while writing {path}");
    }

    fn exists(&self, path: &str) -> bool {
        self.inner.exists(path)
    }

    fn list_dir(&self, path: &str) -> Result<Vec<String>, VfsError> {
        self.inner.list_dir(path)
    }

    fn mkdir(&self, path: &str) -> Result<(), VfsError> {
        self.inner.mkdir(path)
    }

    fn remove(&self, path: &str) -> Result<(), VfsError> {
        self.inner.remove(path)
    }

    fn metadata(&self, path: &str) -> Result<FsMetadata, VfsError> {
        self.inner.metadata(path)
    }

    fn snapshot(&self) -> Result<VfsSnapshot, VfsError> {
        self.inner.snapshot()
    }

    fn restore(&self, snapshot: &VfsSnapshot) -> Result<(), VfsError> {
        self.inner.restore(snapshot)
    }
}

#[derive(Debug, Default)]
pub struct FakeJournalStorage {
    pub entries: Mutex<Vec<JournalEntry>>,
    pub fail_next_append: AtomicBool,
}

impl FakeJournalStorage {
    pub fn entries(&self) -> Vec<JournalEntry> {
        self.entries.lock().unwrap().clone()
    }

    pub fn fail_next_append(&self) {
        self.fail_next_append.store(true, Ordering::SeqCst);
    }
}

impl JournalStorage for FakeJournalStorage {
    fn append(&self, entry: JournalEntry) -> Result<(), JournalError> {
        if self.fail_next_append.swap(false, Ordering::SeqCst) {
            return Err(JournalError::Storage("injected append failure".into()));
        }

        self.entries.lock().unwrap().push(entry);
        Ok(())
    }

    fn read_all(&self, agent_id: &AgentId) -> Result<Vec<JournalEntry>, JournalError> {
        Ok(self
            .entries
            .lock()
            .unwrap()
            .iter()
            .filter(|entry| &entry.agent_id == agent_id)
            .cloned()
            .collect())
    }

    fn query_token_usage(&self, _agent_id: &AgentId) -> Result<TokenUsage, JournalError> {
        Ok(TokenUsage::default())
    }

    fn save_checkpoint(
        &self,
        agent_id: &AgentId,
        _after_entry: usize,
        data: CheckpointData,
    ) -> Result<(), JournalError> {
        let snapshot_data =
            serde_json::to_vec(&data).map_err(|error| JournalError::Storage(error.to_string()))?;
        self.append(JournalEntry {
            schema_version: JOURNAL_SCHEMA_VERSION,
            agent_id: agent_id.clone(),
            timestamp_ms: 0,
            entry: JournalEntryKind::Checkpoint { snapshot_data },
        })
    }

    fn fork_from(
        &self,
        agent_id: &AgentId,
        checkpoint_idx: usize,
    ) -> Result<Vec<JournalEntry>, JournalError> {
        let entries = self.read_all(agent_id)?;
        if checkpoint_idx >= entries.len() {
            return Err(JournalError::InvalidCheckpointIndex(checkpoint_idx));
        }
        Ok(entries[..=checkpoint_idx].to_vec())
    }

    fn read_from(
        &self,
        agent_id: &AgentId,
        start_index: usize,
    ) -> Result<Vec<JournalEntry>, JournalError> {
        let entries = self.read_all(agent_id)?;
        if start_index > entries.len() {
            return Err(JournalError::InvalidCheckpointIndex(start_index));
        }
        Ok(entries[start_index..].to_vec())
    }
}
