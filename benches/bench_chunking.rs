use criterion::{Criterion, criterion_group, criterion_main};
use xet_server::chunking::{ChunkConfig, Chunker};

fn bench_chunking_1mb(c: &mut Criterion) {
    let data = vec![0xABu8; 1024 * 1024];
    let config = ChunkConfig::default();

    c.bench_function("chunking_1mb", |b| {
        b.iter(|| {
            let mut chunker = Chunker::new(config.clone());
            chunker.chunk_data(&data)
        })
    });
}

fn bench_chunking_64mb(c: &mut Criterion) {
    let data = vec![0xABu8; 64 * 1024 * 1024];
    let config = ChunkConfig::default();

    c.bench_function("chunking_64mb", |b| {
        b.iter(|| {
            let mut chunker = Chunker::new(config.clone());
            chunker.chunk_data(&data)
        })
    });
}

criterion_group!(benches, bench_chunking_1mb, bench_chunking_64mb);
criterion_main!(benches);
