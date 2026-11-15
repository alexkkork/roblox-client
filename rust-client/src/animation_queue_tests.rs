use super::*;
use crate::animation_assets::AssetError;
use std::{
    sync::atomic::{AtomicUsize, Ordering},
    time::{Duration, Instant},
};

struct Backend(AtomicUsize);
impl AnimationBackend for Backend {
    fn load(&self, reference: &str) -> Result<Value, AssetError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        if reference == "rbxassetid://2" {
            return Err(AssetError::Http(403));
        }
        Ok(json!({"test_reference":reference}))
    }
}
fn drain(queue: &AnimationQueue, count: usize) -> Vec<Value> {
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut result = Vec::new();
    while result.len() < count {
        result.extend(queue.drain());
        assert!(Instant::now() < deadline, "loader did not complete");
        thread::sleep(Duration::from_millis(1));
    }
    result
}
fn actor(id: &str, generation: u64, reference: &str) -> Value {
    json!({"actor_id":id,"runtime_instance_generation":generation,"vm_instantiated":true,
        "world_effects":{"animation_requests":[{"reference":reference}]}})
}

#[test]
fn aliases_share_one_fetch_and_results_keep_vm_identity() {
    let backend = Arc::new(Backend(AtomicUsize::new(0)));
    let queue = AnimationQueue::new(backend.clone()).unwrap();
    queue
        .observe(
            &json!({"animation_requests":[{"reference":"rbxassetid://1"}],
        "actor_runtime":[actor("worker",7,"https://www.roblox.com/asset/?id=1")]}),
        )
        .unwrap();
    let replies = drain(&queue, 2);
    assert_eq!(backend.0.load(Ordering::SeqCst), 1);
    assert_eq!(replies[0]["reference"], "rbxassetid://1");
    assert_eq!(replies[1]["actor_id"], "worker");
    assert_eq!(replies[1]["runtime_instance_generation"], 7);
    assert_eq!(
        replies[1]["reference"],
        "https://www.roblox.com/asset/?id=1"
    );
    queue
        .observe(&json!({"actor_runtime":[actor("worker",8,"rbxassetid://1")]}))
        .unwrap();
    assert_eq!(drain(&queue, 1)[0]["runtime_instance_generation"], 8);
    assert_eq!(backend.0.load(Ordering::SeqCst), 1);
}

#[test]
fn real_errors_and_invalid_references_complete_without_a_fake_clip() {
    let backend = Arc::new(Backend(AtomicUsize::new(0)));
    let queue = AnimationQueue::new(backend.clone()).unwrap();
    queue.observe(&json!({"animation_requests":[{"reference":"rbxassetid://2"},{"reference":"https://example.com/asset"}]})).unwrap();
    let replies = drain(&queue, 2);
    assert!(replies
        .iter()
        .any(|r| r["error"] == "animation_asset_http_403"));
    assert!(replies
        .iter()
        .any(|r| r["error"] == "invalid_animation_reference"));
    assert!(replies.iter().all(|r| r.get("clip").is_none()));
    assert_eq!(backend.0.load(Ordering::SeqCst), 1);
}

#[test]
fn removed_actor_results_are_discarded_and_drain_is_bounded() {
    let queue = AnimationQueue::new(Arc::new(Backend(AtomicUsize::new(0)))).unwrap();
    queue
        .observe(&json!({"actor_runtime":[actor("old",1,"rbxassetid://1")]}))
        .unwrap();
    queue.observe(&json!({"actor_runtime":[],"animation_requests":(10..20).map(|id|json!({"reference":format!("rbxassetid://{id}")})).collect::<Vec<_>>()})).unwrap();
    let replies = drain(&queue, 10);
    assert_eq!(replies.len(), 10);
    assert!(replies.iter().all(|r| r.get("actor_id").is_none()));
    assert!(queue.drain().is_empty());
}

#[test]
fn close_does_not_wait_for_a_blocked_download() {
    struct Blocked {
        started: std::sync::mpsc::Sender<()>,
        resume: Mutex<std::sync::mpsc::Receiver<()>>,
    }
    impl AnimationBackend for Blocked {
        fn load(&self, _: &str) -> Result<Value, AssetError> {
            self.started.send(()).unwrap();
            self.resume
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(5))
                .unwrap();
            Ok(json!({}))
        }
    }
    let (started, waiting) = std::sync::mpsc::channel();
    let (resume, blocked) = std::sync::mpsc::channel();
    let queue = AnimationQueue::new(Arc::new(Blocked {
        started,
        resume: Mutex::new(blocked),
    }))
    .unwrap();
    queue
        .observe(&json!({"animation_requests":[{"reference":"rbxassetid://1"}]}))
        .unwrap();
    waiting.recv_timeout(Duration::from_secs(2)).unwrap();
    let now = Instant::now();
    queue.close();
    let elapsed = now.elapsed();
    resume.send(()).unwrap();
    assert!(elapsed < Duration::from_millis(250));
    assert!(queue.drain().is_empty());
    assert!(queue.observe(&json!({})).is_err());
}

#[test]
fn rejects_an_unbounded_request_batch_before_enqueuing() {
    let queue = AnimationQueue::new(Arc::new(Backend(AtomicUsize::new(0)))).unwrap();
    let rows = (1..=257)
        .map(|id| json!({"reference":format!("rbxassetid://{id}")}))
        .collect::<Vec<_>>();
    assert!(queue.observe(&json!({"animation_requests":rows})).is_err());
    assert!(queue.shared.0.lock().unwrap().pending.is_empty());
}

#[test]
fn large_results_apply_backpressure_and_cache_eviction_without_losing_replies() {
    struct Large;
    impl AnimationBackend for Large {
        fn load(&self, _: &str) -> Result<Value, AssetError> {
            Ok(json!({"data":"x".repeat(3*1024*1024)}))
        }
    }
    let queue = AnimationQueue::new(Arc::new(Large)).unwrap();
    queue.observe(&json!({"animation_requests":(1..=3).map(|id|json!({"reference":format!("rbxassetid://{id}")})).collect::<Vec<_>>()})).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut received = BTreeSet::new();
    while received.len() < 3 {
        let batch = queue.drain();
        assert!(batch.len() <= 1, "one oversized completion per frame");
        for row in batch {
            received.insert(row["reference"].as_str().unwrap().to_owned());
        }
        assert!(queue.shared.0.lock().unwrap().bytes <= MAX_CLIP_BYTES);
        assert!(
            Instant::now() < deadline,
            "loader failed to resume after drain"
        );
        thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(received.len(), 3);
}
