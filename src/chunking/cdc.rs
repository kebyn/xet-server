use gearhash::Hasher;

/// CDC chunking configuration
#[derive(Clone)]
pub struct ChunkConfig {
    target_chunk_size: usize,
    min_chunk_divisor: usize,
    max_chunk_multiplier: usize,
}

impl ChunkConfig {
    /// Default config: target=64KB, min=8KB, max=128KB
    pub fn new() -> Self {
        Self {
            target_chunk_size: 64 * 1024,
            min_chunk_divisor: 8,
            max_chunk_multiplier: 2,
        }
    }

    pub fn min_chunk_size(&self) -> usize {
        self.target_chunk_size / self.min_chunk_divisor
    }

    pub fn max_chunk_size(&self) -> usize {
        self.target_chunk_size * self.max_chunk_multiplier
    }

    pub fn target_chunk_size(&self) -> usize {
        self.target_chunk_size
    }
}

impl Default for ChunkConfig {
    fn default() -> Self {
        Self::new()
    }
}

/// Chunk information
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    /// Byte offset in original data
    pub offset: usize,
    /// Chunk size in bytes
    pub size: usize,
}

/// GearHash CDC chunker
pub struct Chunker {
    hasher: Hasher<'static>,
    config: ChunkConfig,
    mask: u64,
    min_chunk: usize,
    max_chunk: usize,
}

impl Chunker {
    /// Create a new chunker
    pub fn new(config: ChunkConfig) -> Self {
        let target = config.target_chunk_size();
        let min_chunk = config.min_chunk_size();
        let max_chunk = config.max_chunk_size();

        // Compute mask: hash & mask == 0 with probability ~1/target
        // Shift (target-1) left to the highest bits
        let mask = ((target - 1) as u64) << ((target - 1) as u64).leading_zeros();

        Self {
            hasher: Hasher::default(),
            config,
            mask,
            min_chunk,
            max_chunk,
        }
    }

    /// Chunk the data
    ///
    /// Returns list of Chunk with offset and size
    pub fn chunk_data(&mut self, data: &[u8]) -> Vec<Chunk> {
        if data.is_empty() {
            return Vec::new();
        }

        let mut chunks = Vec::new();
        let mut offset = 0;
        let hasher_window_size = 64; // GearHash window size

        while offset < data.len() {
            let remaining = data.len() - offset;

            // If remaining data is smaller than min chunk, make it one chunk
            if remaining < self.min_chunk {
                chunks.push(Chunk {
                    offset,
                    size: remaining,
                });
                break;
            }

            // Search for chunk boundary
            let search_start = offset + self.min_chunk.saturating_sub(hasher_window_size + 1);
            let search_end = (offset + self.max_chunk).min(data.len());

            if search_start >= data.len() || search_start >= search_end {
                chunks.push(Chunk {
                    offset,
                    size: remaining,
                });
                break;
            }

            let search_data = &data[search_start..search_end];

            // Use GearHash to find boundary
            let boundary = self.hasher.next_match(search_data, self.mask);

            let chunk_size = match boundary {
                Some(pos) => search_start + pos - offset,
                None => search_end - offset,
            };

            // Ensure minimum chunk size
            let chunk_size = chunk_size.max(self.min_chunk.min(remaining));

            chunks.push(Chunk {
                offset,
                size: chunk_size,
            });

            // Reset hasher state for next chunk
            self.hasher.set_hash(0);

            offset += chunk_size;
        }

        chunks
    }

    pub fn config(&self) -> &ChunkConfig {
        &self.config
    }
}