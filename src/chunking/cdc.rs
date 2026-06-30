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
        assert!(
            target.is_power_of_two(),
            "chunk target size must be a power of two, got {}",
            target
        );
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

/// Streaming CDC chunker for processing data in blocks.
///
/// Instead of loading the entire data into memory, data is fed block-by-block
/// via `next_block()`, which returns any chunks completed within that block.
/// `finalize()` returns the last chunk(s) when all data has been consumed.
///
/// Memory usage is bounded to O(max_chunk_size + block_size) regardless of
/// total input size.
pub struct StreamingChunker {
    hasher: Hasher<'static>,
    config: ChunkConfig,
    mask: u64,
    min_chunk: usize,
    max_chunk: usize,
    /// Buffer holding data that hasn't yet been assigned to a complete chunk.
    buffer: Vec<u8>,
    /// Total bytes consumed so far (including data already emitted as chunks).
    total_offset: usize,
}

impl StreamingChunker {
    /// Create a new streaming chunker with the given config.
    pub fn new(config: ChunkConfig) -> Self {
        let target = config.target_chunk_size();
        let min_chunk = config.min_chunk_size();
        let max_chunk = config.max_chunk_size();

        assert!(
            target.is_power_of_two(),
            "chunk target size must be a power of two, got {}",
            target
        );
        let mask = ((target - 1) as u64) << ((target - 1) as u64).leading_zeros();

        Self {
            hasher: Hasher::default(),
            config,
            mask,
            min_chunk,
            max_chunk,
            buffer: Vec::new(),
            total_offset: 0,
        }
    }

    /// Feed a block of data and return any chunks that are now complete.
    ///
    /// Chunks are emitted as soon as a boundary is found. The last partial
    /// chunk stays in the internal buffer until more data arrives or
    /// `finalize()` is called.
    pub fn next_block(&mut self, data: &[u8]) -> Vec<Chunk> {
        self.buffer.extend_from_slice(data);
        self.drain_complete_chunks()
    }

    /// Flush all remaining data as the final chunk(s).
    ///
    /// Call this after all input has been fed via `next_block()`.
    /// Uses the same boundary-finding logic as the oneshot chunker,
    /// with `data.len()` = remaining buffer size.
    pub fn finalize(mut self) -> Vec<Chunk> {
        if self.buffer.is_empty() {
            return Vec::new();
        }

        let mut chunks = Vec::new();
        let hasher_window_size = 64;
        let data_len = self.buffer.len();

        // Process remaining buffer using oneshot-style logic
        // (search_end is capped at remaining data, not max_chunk)
        let mut buf_offset = 0;
        while buf_offset < self.buffer.len() {
            let remaining = self.buffer.len() - buf_offset;

            if remaining < self.min_chunk {
                // Final small chunk
                chunks.push(Chunk {
                    offset: self.total_offset + buf_offset,
                    size: remaining,
                });
                break;
            }

            let search_start_in_buf =
                buf_offset + self.min_chunk.saturating_sub(hasher_window_size + 1);
            let search_end_in_buf = (buf_offset + self.max_chunk).min(self.buffer.len());

            if search_start_in_buf >= self.buffer.len() || search_start_in_buf >= search_end_in_buf
            {
                // Can't search — emit rest as one chunk
                chunks.push(Chunk {
                    offset: self.total_offset + buf_offset,
                    size: remaining,
                });
                break;
            }

            let search_data = &self.buffer[search_start_in_buf..search_end_in_buf];
            let boundary = self.hasher.next_match(search_data, self.mask);

            let chunk_size = match boundary {
                Some(pos) => search_start_in_buf + pos - buf_offset,
                None => search_end_in_buf - buf_offset,
            };

            let chunk_size = chunk_size.max(self.min_chunk.min(remaining));

            chunks.push(Chunk {
                offset: self.total_offset + buf_offset,
                size: chunk_size,
            });

            self.hasher.set_hash(0);
            buf_offset += chunk_size;
        }

        self.total_offset += data_len;
        self.buffer.clear();

        chunks
    }

    /// Total bytes consumed (emitted + buffered).
    pub fn total_bytes(&self) -> usize {
        self.total_offset + self.buffer.len()
    }

    /// Extract complete chunks from the buffer.
    ///
    /// Matches the oneshot `Chunker::chunk_data` logic exactly:
    /// - Wait until buffer has at least max_chunk bytes (or is the final data)
    /// - Then search for boundary in [search_start..search_end]
    /// - search_end is capped at max_chunk (same as oneshot)
    ///
    /// Since `drain_complete_chunks` doesn't know if more data is coming,
    /// the caller must ensure enough data is buffered before calling, or
    /// use `finalize()` for the last chunk.
    fn drain_complete_chunks(&mut self) -> Vec<Chunk> {
        let mut chunks = Vec::new();
        let hasher_window_size = 64;

        loop {
            let remaining = self.buffer.len();

            // Not enough data to form any chunk — wait for more input
            if remaining < self.min_chunk {
                break;
            }

            // Only search when we have enough data for a meaningful search.
            // The oneshot chunker searches [search_start..min(offset+max_chunk, data.len())].
            // In streaming, we can't know data.len(), so we wait until we have
            // at least max_chunk bytes before searching. This ensures the search
            // window matches what the oneshot chunker would see.
            //
            // Exception: if remaining < max_chunk, we're near the end (caller
            // should use finalize() for this case), so we don't emit yet.
            if remaining < self.max_chunk {
                break;
            }

            let search_start = self.min_chunk.saturating_sub(hasher_window_size + 1);
            let search_end = self.max_chunk;

            let search_data = &self.buffer[search_start..search_end];
            let boundary = self.hasher.next_match(search_data, self.mask);

            let chunk_size = match boundary {
                Some(pos) => search_start + pos,
                None => search_end,
            };

            // Ensure minimum chunk size (same logic as oneshot chunker)
            let chunk_size = chunk_size.max(self.min_chunk.min(remaining));

            chunks.push(Chunk {
                offset: self.total_offset,
                size: chunk_size,
            });

            self.total_offset += chunk_size;
            self.buffer.drain(..chunk_size);
            self.hasher.set_hash(0);
        }

        chunks
    }

    pub fn config(&self) -> &ChunkConfig {
        &self.config
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify StreamingChunker produces the same chunks as the oneshot Chunker
    /// when fed data block-by-block.
    #[test]
    fn streaming_chunker_matches_oneshot() {
        let config = ChunkConfig::default();
        let data: Vec<u8> = (0..500_000).map(|i| (i % 256) as u8).collect();

        // Oneshot chunker
        let mut oneshot = Chunker::new(config.clone());
        let oneshot_chunks = oneshot.chunk_data(&data);

        // Streaming chunker fed 8KB blocks
        let mut streaming = StreamingChunker::new(config);
        let mut streaming_chunks = Vec::new();
        for block in data.chunks(8192) {
            streaming_chunks.extend(streaming.next_block(block));
        }
        streaming_chunks.extend(streaming.finalize());

        assert_eq!(
            oneshot_chunks.len(),
            streaming_chunks.len(),
            "chunk count mismatch"
        );
        for (o, s) in oneshot_chunks.iter().zip(streaming_chunks.iter()) {
            assert_eq!(o.offset, s.offset, "offset mismatch");
            assert_eq!(o.size, s.size, "size mismatch");
        }
    }

    /// StreamingChunker should handle empty input.
    #[test]
    fn streaming_chunker_empty() {
        let mut streaming = StreamingChunker::new(ChunkConfig::default());
        assert!(streaming.next_block(b"").is_empty());
        assert!(streaming.finalize().is_empty());
    }

    /// StreamingChunker should handle input smaller than min_chunk_size.
    #[test]
    fn streaming_chunker_tiny_input() {
        let mut streaming = StreamingChunker::new(ChunkConfig::default());
        // Input smaller than min_chunk (8KB) — should be held until finalize
        let small = vec![42u8; 100];
        assert!(streaming.next_block(&small).is_empty());
        let final_chunks = streaming.finalize();
        assert_eq!(final_chunks.len(), 1);
        assert_eq!(final_chunks[0].size, 100);
    }
}
