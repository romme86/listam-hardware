use hypercore::{HypercoreBuilder, Manifest, ManifestSigner, Storage, VerifyingKey};
use hypercore_schema::{DEFAULT_NAMESPACE, RequestBlock, RequestUpgrade};

// A raw-key, file-backed mirror receives a manifest + data via proofs, then is
// reopened by raw key — the leaf's exact persistence lifecycle.
#[tokio::test]
async fn mirror_with_data_reopen() {
    let mut src = HypercoreBuilder::new(Storage::new_memory().await.unwrap())
        .build().await.unwrap();
    src.append(b"alpha").await.unwrap();
    src.append(b"bravo").await.unwrap();
    src.append(b"charlie").await.unwrap();
    let public: VerifyingKey = src.key_pair().unwrap().public;
    let key = public.to_bytes();
    let manifest = Manifest {
        version: 1, allow_patch: false, quorum: 1,
        signers: vec![ManifestSigner { namespace: DEFAULT_NAMESPACE, public_key: key }],
        prologue: None, linked: None, user_data: None,
    };
    let dir = std::env::temp_dir().join(format!("hc-mirror-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    {
        let mut mirror = HypercoreBuilder::new(Storage::new_file_storage(&dir).await.unwrap())
            .raw_key(key).build().await.unwrap();
        mirror.set_manifest(manifest.clone()).await.unwrap();
        let nodes = mirror.missing_nodes(0).await.unwrap();
        let proof = src.create_proof(
            Some(RequestBlock { index: 0, nodes }), None, None,
            Some(RequestUpgrade { start: 0, length: 3 }),
        ).await.unwrap().unwrap();
        assert!(mirror.verify_and_apply_proof(&proof).await.unwrap());
        // fetch a second block so the oplog gets a nodes+bitfield-only entry
        let nodes = mirror.missing_nodes(1).await.unwrap();
        let proof = src.create_proof(Some(RequestBlock { index: 1, nodes }), None, None, None)
            .await.unwrap().unwrap();
        assert!(mirror.verify_and_apply_proof(&proof).await.unwrap());
        assert_eq!(mirror.info().length, 3);
    }
    {
        let mut mirror = HypercoreBuilder::new(Storage::new_file_storage(&dir).await.unwrap())
            .raw_key(key).build().await
            .expect("reopen of file-backed mirror with data must succeed");
        assert_eq!(mirror.info().length, 3, "length not restored from disk");
        assert!(mirror.manifest().is_some(), "manifest not restored");
        assert_eq!(&mirror.get(1).await.unwrap().unwrap(), b"bravo", "block not readable after reopen");
    }
    let _ = std::fs::remove_dir_all(&dir);
}
