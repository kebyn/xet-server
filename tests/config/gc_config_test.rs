use xet_server::config::GcConfig;
use serial_test::serial;

#[test]
fn test_gc_config_defaults() {
    let config = GcConfig::default();
    assert!(!config.enabled);
    assert!(config.dry_run);
    assert_eq!(config.bloom.expected_items, 10_000_000);
    assert_eq!(config.bloom.false_positive_rate, 0.001);
    assert_eq!(config.scanner.page_size, 1000);
    assert_eq!(config.grace.absolute_seconds, 3600);
    assert_eq!(config.grace.soft_cycles, 2);
}

#[test]
#[serial]
fn test_gc_config_from_env() {
    // SAFETY: `#[serial]` ensures no other test mutates env vars concurrently.
    unsafe {
        std::env::set_var("GC_ENABLED", "true");
        std::env::set_var("GC_BLOOM_EXPECTED_ITEMS", "5000000");
    }

    let config = GcConfig::from_env();
    assert!(config.enabled);
    assert_eq!(config.bloom.expected_items, 5_000_000);

    unsafe {
        std::env::remove_var("GC_ENABLED");
        std::env::remove_var("GC_BLOOM_EXPECTED_ITEMS");
    }
}
