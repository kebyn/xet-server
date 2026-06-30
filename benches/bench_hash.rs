use criterion::{Criterion, criterion_group, criterion_main};
use xet_server::hash::{compute_data_hash, xorb_hash};
use xet_server::types::MerkleHash;

fn bench_data_hash_1kb(c: &mut Criterion) {
    let data = vec![0xABu8; 1024];
    c.bench_function("data_hash_1kb", |b| b.iter(|| compute_data_hash(&data)));
}

fn bench_data_hash_64kb(c: &mut Criterion) {
    let data = vec![0xABu8; 64 * 1024];
    c.bench_function("data_hash_64kb", |b| b.iter(|| compute_data_hash(&data)));
}

fn bench_xorb_hash_1024_chunks(c: &mut Criterion) {
    let chunks: Vec<(MerkleHash, u64)> = (0..1024)
        .map(|i| (MerkleHash::from([i as u8; 32]), 65536))
        .collect();

    c.bench_function("xorb_hash_1024", |b| b.iter(|| xorb_hash(&chunks)));
}

criterion_group!(
    benches,
    bench_data_hash_1kb,
    bench_data_hash_64kb,
    bench_xorb_hash_1024_chunks
);
criterion_main!(benches);
