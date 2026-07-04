use simulacra_types::{Locator, MemoryCapability, MemoryPath, MemoryVersion, TenantId};

struct NoopMemoryReceiver;

impl simulacra_memory::MemoryEventReceiver for NoopMemoryReceiver {
    fn recv<'a>(
        &'a mut self,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = simulacra_memory::MemoryRecvOutcome> + Send + 'a>,
    > {
        Box::pin(async { simulacra_memory::MemoryRecvOutcome::Closed })
    }

    fn recv_blocking(&mut self) -> Option<simulacra_memory::MemoryEvent> {
        None
    }
}

struct NoopMemoryStore;

impl simulacra_memory::MemoryStore for NoopMemoryStore {
    fn put(
        &self,
        _tenant: &TenantId,
        _path: &MemoryPath,
        _data: &[u8],
    ) -> Result<MemoryVersion, simulacra_memory::MemoryError> {
        Err(simulacra_memory::MemoryError::Internal(
            "noop memory store".into(),
        ))
    }

    fn get(
        &self,
        _tenant: &TenantId,
        _path: &MemoryPath,
    ) -> Result<(Vec<u8>, MemoryVersion), simulacra_memory::MemoryError> {
        Err(simulacra_memory::MemoryError::NotFound("noop".into()))
    }

    fn exists(
        &self,
        _tenant: &TenantId,
        _path: &MemoryPath,
    ) -> Result<bool, simulacra_memory::MemoryError> {
        Ok(false)
    }

    fn list_prefix(
        &self,
        _tenant: &TenantId,
        _prefix: &MemoryPath,
    ) -> Result<Vec<simulacra_memory::MemoryEntry>, simulacra_memory::MemoryError> {
        Ok(Vec::new())
    }

    fn current_version(
        &self,
        _tenant: &TenantId,
        _path: &MemoryPath,
    ) -> Result<Option<MemoryVersion>, simulacra_memory::MemoryError> {
        Ok(None)
    }

    fn delete(
        &self,
        _tenant: &TenantId,
        _path: &MemoryPath,
    ) -> Result<MemoryVersion, simulacra_memory::MemoryError> {
        Err(simulacra_memory::MemoryError::NotFound("noop".into()))
    }

    fn delete_prefix(
        &self,
        _tenant: &TenantId,
        _prefix: &MemoryPath,
    ) -> Result<u64, simulacra_memory::MemoryError> {
        Ok(0)
    }

    fn subscribe(
        &self,
    ) -> Result<Box<dyn simulacra_memory::MemoryEventReceiver>, simulacra_memory::MemoryError> {
        Ok(Box::new(NoopMemoryReceiver))
    }
}

struct NoopVectorIndex;

impl simulacra_memory::VectorIndex for NoopVectorIndex {
    fn upsert(
        &self,
        _tenant: &TenantId,
        _path: &MemoryPath,
        _version: MemoryVersion,
        _embedder_id: &simulacra_memory::EmbedderId,
        _chunks: &[simulacra_memory::IndexedChunk],
    ) -> Result<simulacra_memory::UpsertOutcome, simulacra_memory::MemoryError> {
        Ok(simulacra_memory::UpsertOutcome::Applied)
    }

    fn delete_path(
        &self,
        _tenant: &TenantId,
        _path: &MemoryPath,
        _tombstone_version: MemoryVersion,
    ) -> Result<(), simulacra_memory::MemoryError> {
        Ok(())
    }

    fn delete_prefix(
        &self,
        _tenant: &TenantId,
        _prefix: &MemoryPath,
    ) -> Result<u64, simulacra_memory::MemoryError> {
        Ok(0)
    }

    fn search(
        &self,
        _tenant: &TenantId,
        _scope: &MemoryPath,
        _query_embedding: &[f32],
        _embedder_id: &simulacra_memory::EmbedderId,
        _k: usize,
        _min_cosine: Option<f32>,
    ) -> Result<Vec<simulacra_memory::SearchHit>, simulacra_memory::MemoryError> {
        Ok(Vec::new())
    }

    fn embedder_fingerprint(
        &self,
        _tenant: &TenantId,
    ) -> Result<Option<simulacra_memory::EmbedderId>, simulacra_memory::MemoryError> {
        Ok(None)
    }

    fn mark_tenant_stale(&self, _tenant: &TenantId) -> Result<u64, simulacra_memory::MemoryError> {
        Ok(0)
    }

    fn get_chunk(
        &self,
        _tenant: &TenantId,
        _path: &MemoryPath,
        _version: MemoryVersion,
        _chunk_index: usize,
    ) -> Result<Option<(Locator, String)>, simulacra_memory::MemoryError> {
        Ok(None)
    }
}
