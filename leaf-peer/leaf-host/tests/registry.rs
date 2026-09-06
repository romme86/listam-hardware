use std::sync::Arc;
use leaf_core::{MirrorStorage, Registry};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_announcements_open_a_core_once() {
    let registry = Registry::new(MirrorStorage::Memory);
    let key = [7_u8; 32];
    let barrier = Arc::new(tokio::sync::Barrier::new(16));
    let mut tasks = Vec::new();
    for _ in 0..16 {
        let registry = registry.clone();
        let barrier = barrier.clone();
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            registry.add_core(key, false).await.unwrap().is_some()
        }));
    }
    let mut opened = 0;
    for task in tasks {
        opened += usize::from(task.await.unwrap());
    }
    assert_eq!(opened, 1);
    assert_eq!(registry.keys().await, vec![key]);
}
