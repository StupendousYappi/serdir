use criterion::{criterion_group, criterion_main, Criterion};
use rapidhash::quality::RapidHasher;
use std::collections::hash_map::DefaultHasher;
use std::hash::Hasher;
use std::hint::black_box;

const TEST_STRING: &str = "/var/lib/app/static/index.38sx230sk30a.html";

fn bench_hashing(c: &mut Criterion) {
    let mut group = c.benchmark_group("hashing");

    group.bench_function("DefaultHasher", |b| {
        b.iter(|| {
            let mut hasher = DefaultHasher::new();
            hasher.write(TEST_STRING.as_bytes());
            black_box(hasher.finish());
        })
    });

    group.bench_function("RapidHasher", |b| {
        b.iter(|| {
            let mut hasher = RapidHasher::default();
            hasher.write(TEST_STRING.as_bytes());
            black_box(hasher.finish());
        })
    });

    group.finish();
}

criterion_group!(benches, bench_hashing);
criterion_main!(benches);
