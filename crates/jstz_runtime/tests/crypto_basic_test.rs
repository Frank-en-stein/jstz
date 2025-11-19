use jstz_runtime::init_test_setup;

/// Test that crypto global object is accessible
#[test]
fn test_crypto_global_exists() {
    init_test_setup! {
        runtime = runtime;
    };

    let code = r#"
        // Verify crypto global exists
        if (typeof crypto === 'undefined') {
            throw new Error('crypto is not defined');
        }

        // Verify crypto.subtle exists
        if (typeof crypto.subtle === 'undefined') {
            throw new Error('crypto.subtle is not defined');
        }

        "crypto_available"
    "#;

    let result = runtime.execute_with_result::<String>(code).unwrap();
    assert_eq!(result, "crypto_available");
}

/// Test SHA-256 digest functionality
#[test]
fn test_crypto_subtle_digest_sha256() {
    init_test_setup! {
        runtime = runtime;
    };

    let code = r#"
        (async () => {
            // Test data
            const data = new TextEncoder().encode("hello world");

            // Compute SHA-256 hash
            const hashBuffer = await crypto.subtle.digest("SHA-256", data);

            // Convert to hex string
            const hashArray = Array.from(new Uint8Array(hashBuffer));
            const hashHex = hashArray.map(b => b.toString(16).padStart(2, '0')).join('');

            // Expected SHA-256 hash of "hello world"
            const expected = "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9";

            if (hashHex !== expected) {
                throw new Error(`Hash mismatch: got ${hashHex}, expected ${expected}`);
            }

            return "sha256_works";
        })()
    "#;

    let result = jstz_utils::test_util::TOKIO.block_on(async {
        let module_id = runtime
            .execute_main_module(&deno_core::ModuleSpecifier::parse("test://test").unwrap())
            .await
            .unwrap();

        runtime
            .run_event_loop(deno_core::PollEventLoopOptions::default())
            .await
            .unwrap();

        runtime.execute_with_result::<String>(code)
    });

    assert!(result.is_ok(), "SHA-256 digest test failed: {:?}", result);
    assert_eq!(result.unwrap(), "sha256_works");
}

/// Test that crypto methods return Promises
#[test]
fn test_crypto_methods_return_promises() {
    init_test_setup! {
        runtime = runtime;
    };

    let code = r#"
        const data = new Uint8Array([1, 2, 3]);
        const digestPromise = crypto.subtle.digest("SHA-256", data);

        // Verify it's a Promise
        if (!(digestPromise instanceof Promise)) {
            throw new Error('digest() did not return a Promise');
        }

        "promises_work"
    "#;

    let result = runtime.execute_with_result::<String>(code).unwrap();
    assert_eq!(result, "promises_work");
}
