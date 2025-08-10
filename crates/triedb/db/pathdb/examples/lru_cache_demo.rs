use reth_triedb_pathdb::{PathDB, PathProviderConfig, PathProvider};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== PathDB LRU Cache Demo ===");

    // Create a database with small cache size for demonstration
    let mut config = PathProviderConfig::default();
    config.cache_size = 5; // Only 5 entries for demo

    let db = PathDB::new("/tmp/pathdb_lru_demo", config)?;

    println!("Created PathDB with LRU cache (max 5 entries)");
    println!("Cache stats: {:?}", db.cache_stats());

    // Test 1: Write operations update cache first
    println!("\n1. Testing write operations (cache-first):");
    db.put(b"key1", b"value1")?;
    db.put(b"key2", b"value2")?;
    db.put(b"key3", b"value3")?;

    println!("   After writing 3 keys, cache stats: {:?}", db.cache_stats());

    // Test 2: Read operations check cache first
    println!("\n2. Testing read operations (cache-first):");
    let value1 = db.get(b"key1")?;
    let value2 = db.get(b"key2")?;
    let value3 = db.get(b"key3")?;

    println!("   Retrieved: key1={:?}, key2={:?}, key3={:?}",
             String::from_utf8_lossy(&value1.unwrap()),
             String::from_utf8_lossy(&value2.unwrap()),
             String::from_utf8_lossy(&value3.unwrap()));

    println!("   After reading, cache stats: {:?}", db.cache_stats());

    // Test 3: Cache eviction when capacity is exceeded
    println!("\n3. Testing cache eviction (LRU behavior):");
    db.put(b"key4", b"value4")?;
    db.put(b"key5", b"value5")?;
    db.put(b"key6", b"value6")?; // This should evict key1

    println!("   After writing 3 more keys, cache stats: {:?}", db.cache_stats());

    // Read key1 again - should come from DB (cache miss)
    let value1_again = db.get(b"key1")?;
    println!("   Reading key1 again: {:?}", String::from_utf8_lossy(&value1_again.unwrap()));
    println!("   After reading key1 from DB, cache stats: {:?}", db.cache_stats());

    // Test 4: Delete operations clear cache first
    println!("\n4. Testing delete operations (cache-first):");
    db.delete(b"key2")?;
    println!("   After deleting key2, cache stats: {:?}", db.cache_stats());

    let deleted_value = db.get(b"key2")?;
    println!("   Reading deleted key2: {:?}", deleted_value);

    // Test 5: Multi-operations with cache
    println!("\n5. Testing multi-operations with cache:");
    let kvs = vec![
        (b"multi_key1".to_vec(), b"multi_value1".to_vec()),
        (b"multi_key2".to_vec(), b"multi_value2".to_vec()),
    ];

    db.put_multi(&kvs)?;
    println!("   After multi-put, cache stats: {:?}", db.cache_stats());

    let keys = vec![b"multi_key1".to_vec(), b"multi_key2".to_vec()];
    let retrieved = db.get_multi(&keys)?;
    println!("   Retrieved {} values from multi-get", retrieved.len());

    // Test 6: Clear cache
    println!("\n6. Testing cache clear:");
    db.clear_cache();
    println!("   After clearing cache, cache stats: {:?}", db.cache_stats());

    // Data should still be accessible from DB
    let value6 = db.get(b"key6")?;
    println!("   Reading key6 after cache clear: {:?}", String::from_utf8_lossy(&value6.unwrap()));

    println!("\n=== LRU Cache Demo Completed Successfully! ===");
    Ok(())
}
