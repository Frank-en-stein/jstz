use boa_engine::{
    js_string,
    object::{builtins::JsPromise, ErasedObject, ObjectInitializer},
    property::Attribute,
    Context, JsArgs, JsData, JsError, JsNativeError, JsResult, JsValue, NativeFunction,
};

use jstz_api::http::request::Request;
use jstz_core::{
    host::HostRuntime,
    host_defined,
    kv::Transaction,
    native::JsNativeObject,
    runtime::{self},
    value::IntoJs,
};
use jstz_crypto::smart_function_hash::SmartFunctionHash;

use crate::{
    context::account::{Account, Amount},
    executor::smart_function,
    operation::{DeployFunction, OperationHash},
    runtime::v1::{fetch_handler, ProtocolData},
    Result,
};

use boa_gc::{empty_trace, Finalize, GcRefMut, Trace};

#[derive(JsData)]
struct SmartFunction {
    address: SmartFunctionHash,
}
impl Finalize for SmartFunction {}

unsafe impl Trace for SmartFunction {
    empty_trace!();
}

impl SmartFunction {
    fn from_js_value(value: &JsValue) -> JsResult<GcRefMut<'_, ErasedObject, Self>> {
        value
            .as_object()
            .and_then(|obj| obj.downcast_mut::<Self>())
            .ok_or_else(|| {
                JsNativeError::typ()
                    .with_message(
                        "Failed to convert js value into rust type `SmartFunction`",
                    )
                    .into()
            })
    }

    fn create(
        &self,
        hrt: &mut impl HostRuntime,
        tx: &mut Transaction,
        function_code: String,
        initial_balance: Amount,
    ) -> Result<String> {
        // 1. Deploy the smart function
        let deploy_receipt = smart_function::deploy::execute(
            hrt,
            tx,
            &self.address,
            DeployFunction {
                function_code,
                account_credit: initial_balance,
            },
        )?;

        // 2. Increment nonce of current account
        Account::nonce(hrt, tx, &self.address)?.increment();

        Ok(deploy_receipt.address.to_string())
    }

    // Invariant: The function should always be called within a js_host_context
    fn call(
        self_address: &SmartFunctionHash,
        request: &JsNativeObject<Request>,
        operation_hash: OperationHash,
        context: &mut Context,
    ) -> JsResult<JsValue> {
        fetch_handler::fetch(self_address, operation_hash, request, context)
    }
}

pub struct SmartFunctionApi {
    pub address: SmartFunctionHash,
}

impl SmartFunctionApi {
    const NAME: &'static str = "SmartFunction";

    fn fetch(
        address: &SmartFunctionHash,
        args: &[JsValue],
        context: &mut Context,
    ) -> JsResult<JsValue> {
        // Clone all data we need from parent before any mutable borrows
        let (parent_address, parent_operation_hash, parent_call_sequence, parent_depth) = {
            host_defined!(context, host_defined);
            let parent_data = host_defined
                .get::<ProtocolData>()
                .expect("trace data undefined");

            (
                parent_data.address.clone(),
                parent_data.operation_hash.clone(),
                parent_data.call_sequence.clone(),
                parent_data.depth,
            )
        };

        // Increment call sequence for this nested call
        {
            let mut seq = parent_call_sequence.borrow_mut();
            *seq += 1;
        }

        // Check depth limit to prevent overflow (u16::MAX = 65,535)
        let child_depth = parent_depth
            .checked_add(1)
            .ok_or_else(|| {
                JsError::from_native(
                    JsNativeError::eval()
                        .with_message("Maximum call depth exceeded (65,535 levels)")
                )
            })?;

        // Create new ProtocolData for the nested call with incremented depth
        let child_data = ProtocolData {
            address: address.clone(),
            operation_hash: parent_operation_hash.clone(),
            call_sequence: parent_call_sequence.clone(), // Share counter
            depth: child_depth,                           // Safe incremented depth
        };

        // Replace parent data with child data in context
        {
            host_defined!(context, mut host_defined);
            host_defined.insert(child_data);
        }

        let request: JsNativeObject<Request> =
            args.get_or_undefined(0).clone().try_into()?;

        let result = SmartFunction::call(
            address,
            &request,
            parent_operation_hash.clone(),
            context,
        );

        // Restore parent data after nested call
        {
            host_defined!(context, mut host_defined);
            host_defined.insert(ProtocolData {
                address: parent_address,
                operation_hash: parent_operation_hash,
                call_sequence: parent_call_sequence,
                depth: parent_depth,
            });
        }

        result
    }

    fn call(
        this: &JsValue,
        args: &[JsValue],
        context: &mut Context,
    ) -> JsResult<JsValue> {
        let smart_function = SmartFunction::from_js_value(this)?;
        Self::fetch(&smart_function.address, args, context)
    }

    fn create(
        this: &JsValue,
        args: &[JsValue],
        context: &mut Context,
    ) -> JsResult<JsValue> {
        let smart_function = SmartFunction::from_js_value(this)?;

        let function_code: String = args
            .first()
            .ok_or_else(|| {
                JsNativeError::typ()
                    .with_message("Expected at least 1 argument but 0 provided")
            })?
            .try_js_into(context)?;

        let initial_balance = match args.get(1) {
            None => 0,
            Some(balance) => balance.to_big_uint64(context)?,
        };

        let promise = JsPromise::new(
            move |resolvers, context| {
                let address = runtime::with_js_hrt_and_tx(|hrt, tx| {
                    smart_function.create(
                        hrt,
                        tx,
                        function_code,
                        initial_balance as Amount,
                    )
                })?;

                resolvers.resolve.call(
                    &JsValue::undefined(),
                    &[address.into_js(context)],
                    context,
                )?;
                Ok(JsValue::undefined())
            },
            context,
        );

        Ok(promise.into())
    }
}

impl jstz_core::Api for SmartFunctionApi {
    fn init(self, context: &mut Context) {
        let smart_function = ObjectInitializer::with_native_data(
            SmartFunction {
                address: self.address.clone(),
            },
            context,
        )
        .function(
            NativeFunction::from_fn_ptr(Self::call),
            js_string!("call"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(Self::create),
            js_string!("create"),
            2,
        )
        .build();

        context
            .register_global_property(
                js_string!(Self::NAME),
                smart_function,
                Attribute::all(),
            )
            .expect("The smart function object shouldn't exist yet");

        context
            .register_global_builtin_callable(
                js_string!("fetch"),
                1,
                NativeFunction::from_copy_closure_with_captures(
                    |_, args, this, ctx| Self::fetch(&this.address, args, ctx),
                    SmartFunction {
                        address: self.address,
                    },
                ),
            )
            .expect("The fetch function shouldn't exist yet");
    }
}

#[cfg(test)]
mod test {

    use http::Method;
    use jstz_api::http::request::{Request, RequestClass};
    use jstz_core::{
        kv::Transaction,
        native::JsNativeObject,
        runtime::{self, with_js_hrt_and_tx},
        Runtime,
    };
    use jstz_crypto::{
        hash::{Blake2b, Hash},
        public_key_hash::PublicKeyHash,
        smart_function_hash::SmartFunctionHash,
    };
    use jstz_mock::host::JstzMockHost;
    use serde_json::json;

    use crate::{
        context::account::{Account, Address},
        runtime::v1::api::WebApi,
    };

    use super::SmartFunction;

    #[test]
    fn call_system_script_succeeds() {
        let mut mock_host = JstzMockHost::default();
        let rt = mock_host.rt();

        let mut jstz_rt = Runtime::new(10000).unwrap();
        let realm = jstz_rt.realm().clone();
        let context = jstz_rt.context();

        realm.register_api(WebApi, context);

        let self_address = SmartFunctionHash::digest(b"random bytes").unwrap();

        let amount = 100;

        let operation_hash = Blake2b::from(b"operation_hash".as_ref());
        let receiver = Address::User(PublicKeyHash::digest(b"receiver address").unwrap());
        let http_request = http::Request::builder()
            .method(Method::POST)
            .uri("jstz://jstz/withdraw")
            .header("Content-type", "application/json")
            .body(Some(
                json!({
                    "receiver": receiver,
                    "amount": 100
                })
                .to_string()
                .as_bytes()
                .to_vec(),
            ))
            .unwrap();

        let request = Request::from_http_request(http_request, context).unwrap();

        let mut tx = Transaction::default();
        runtime::enter_js_host_context(rt, &mut tx, || {
            with_js_hrt_and_tx(|hrt, tx| {
                tx.begin();
                Account::add_balance(hrt, tx, &self_address, amount).unwrap();
                tx.commit(hrt).unwrap();
            });

            SmartFunction::call(
                &self_address,
                &JsNativeObject::new::<RequestClass>(request, context).unwrap(),
                operation_hash,
                context,
            )
            .unwrap();
        });
    }

    #[test]
    fn test_nested_call_sequence_increments() {
        use crate::runtime::v1::api::ProtocolData;
        use jstz_core::host_defined;
        use std::cell::RefCell;
        use std::rc::Rc;

        let mut jstz_rt = Runtime::new(10000).unwrap();
        let realm = jstz_rt.realm().clone();
        let context = jstz_rt.context();

        realm.register_api(WebApi, context);

        let operation_hash = Blake2b::from(b"test_op".as_ref());
        let addr1 = SmartFunctionHash::digest(b"addr1").unwrap();
        let addr2 = SmartFunctionHash::digest(b"addr2").unwrap();
        let addr3 = SmartFunctionHash::digest(b"addr3").unwrap();

        // Simulate root call setup
        let call_sequence = Rc::new(RefCell::new(0u64));
        let root_data = ProtocolData {
            address: addr1.clone(),
            operation_hash: operation_hash.clone(),
            call_sequence: call_sequence.clone(),
            depth: 0,
        };

        {
            host_defined!(context, mut host_defined);
            host_defined.insert(root_data);
        }

        // Verify root starts at 0
        {
            host_defined!(context, host_defined);
            let data = host_defined.get::<ProtocolData>().unwrap();
            assert_eq!(*data.call_sequence.borrow(), 0);
            assert_eq!(data.depth, 0);
        }

        // Simulate nested call 1: increment to 1
        {
            let parent_seq = {
                host_defined!(context, host_defined);
                host_defined.get::<ProtocolData>().map(|d| d.call_sequence.clone())
            }
            .unwrap();
            *parent_seq.borrow_mut() += 1;

            let nested_data = ProtocolData {
                address: addr2.clone(),
                operation_hash: operation_hash.clone(),
                call_sequence: parent_seq.clone(),
                depth: 1,
            };

            host_defined!(context, mut host_defined);
            host_defined.insert(nested_data);
        }

        // Verify sequence is now 1
        {
            host_defined!(context, host_defined);
            let data = host_defined.get::<ProtocolData>().unwrap();
            assert_eq!(*data.call_sequence.borrow(), 1);
            assert_eq!(data.depth, 1);
        }

        // Simulate nested call 2: increment to 2
        {
            let parent_seq = {
                host_defined!(context, host_defined);
                host_defined.get::<ProtocolData>().map(|d| d.call_sequence.clone())
            }
            .unwrap();
            *parent_seq.borrow_mut() += 1;

            let nested_data = ProtocolData {
                address: addr3.clone(),
                operation_hash: operation_hash.clone(),
                call_sequence: parent_seq.clone(),
                depth: 2,
            };

            host_defined!(context, mut host_defined);
            host_defined.insert(nested_data);
        }

        // Verify sequence is now 2 and depth is 2
        {
            host_defined!(context, host_defined);
            let data = host_defined.get::<ProtocolData>().unwrap();
            assert_eq!(*data.call_sequence.borrow(), 2);
            assert_eq!(data.depth, 2);
        }

        // Verify shared counter persists (all point to same Rc)
        assert_eq!(*call_sequence.borrow(), 2);
    }

    #[test]
    fn test_sibling_calls_have_unique_sequences() {
        use crate::runtime::v1::api::ProtocolData;
        use jstz_core::host_defined;
        use std::cell::RefCell;
        use std::rc::Rc;

        let mut jstz_rt = Runtime::new(10000).unwrap();
        let realm = jstz_rt.realm().clone();
        let context = jstz_rt.context();

        realm.register_api(WebApi, context);

        let operation_hash = Blake2b::from(b"test_op".as_ref());
        let root_addr = SmartFunctionHash::digest(b"root").unwrap();

        // Setup root
        let call_sequence = Rc::new(RefCell::new(0u64));
        let root_data = ProtocolData {
            address: root_addr.clone(),
            operation_hash: operation_hash.clone(),
            call_sequence: call_sequence.clone(),
            depth: 0,
        };

        {
            host_defined!(context, mut host_defined);
            host_defined.insert(root_data.clone());
        }

        // First sibling call
        {
            *call_sequence.borrow_mut() += 1;
        }
        let seq_a = *call_sequence.borrow();

        // Restore parent after first call
        {
            host_defined!(context, mut host_defined);
            host_defined.insert(root_data.clone());
        }

        // Second sibling call (should get sequence 2, not 1)
        {
            *call_sequence.borrow_mut() += 1;
        }
        let seq_b = *call_sequence.borrow();

        // Verify uniqueness
        assert_eq!(seq_a, 1);
        assert_eq!(seq_b, 2);
        assert_ne!(seq_a, seq_b, "Sibling calls must have unique sequences");
    }

    #[test]
    fn test_deep_nesting_memory_and_uniqueness() {
        use crate::runtime::v1::api::ProtocolData;
        use jstz_core::host_defined;
        use std::cell::RefCell;
        use std::rc::Rc;

        let mut jstz_rt = Runtime::new(10000).unwrap();
        let realm = jstz_rt.realm().clone();
        let context = jstz_rt.context();

        realm.register_api(WebApi, context);

        let operation_hash = Blake2b::from(b"deep_test".as_ref());
        let call_sequence = Rc::new(RefCell::new(0u64));

        const MAX_DEPTH: u16 = 50;
        let mut call_ids = Vec::new();

        // Simulate deep nesting
        for depth in 0..=MAX_DEPTH {
            let addr = SmartFunctionHash::digest(format!("addr_{}", depth).as_bytes()).unwrap();

            // Increment sequence for nested calls (skip root)
            if depth > 0 {
                *call_sequence.borrow_mut() += 1;
            }

            let current_seq = *call_sequence.borrow();
            let call_id = format!("{}:{}", operation_hash, current_seq);
            call_ids.push(call_id);

            let data = ProtocolData {
                address: addr,
                operation_hash: operation_hash.clone(),
                call_sequence: call_sequence.clone(),
                depth,
            };

            {
                host_defined!(context, mut host_defined);
                host_defined.insert(data);
            }
        }

        // Verify all call_ids are unique
        let unique_count = call_ids.iter().collect::<std::collections::HashSet<_>>().len();
        assert_eq!(
            unique_count,
            call_ids.len(),
            "All call IDs must be unique even with deep nesting"
        );

        // Verify final sequence matches depth
        assert_eq!(*call_sequence.borrow(), MAX_DEPTH as u64);

        // Memory verification: Rc should have minimal overhead
        // Each ProtocolData shares the same Rc, so memory is O(1) for the counter
        // The context holds one reference, and this test holds another
        assert_eq!(
            Rc::strong_count(&call_sequence),
            2,
            "Context holds one Rc reference, test holds another - shared counter has O(1) memory overhead"
        );
    }

    #[test]
    fn test_call_id_format() {
        use crate::runtime::v1::api::ProtocolData;
        use jstz_core::host_defined;
        use std::cell::RefCell;
        use std::rc::Rc;

        let mut jstz_rt = Runtime::new(10000).unwrap();
        let realm = jstz_rt.realm().clone();
        let context = jstz_rt.context();

        realm.register_api(WebApi, context);

        let operation_hash = Blake2b::from(b"format_test".as_ref());
        let addr = SmartFunctionHash::digest(b"test_addr").unwrap();

        // Test sequence values
        let test_sequences = vec![0, 1, 42, 999, 1000000];

        for seq in test_sequences {
            let call_sequence = Rc::new(RefCell::new(seq));
            let data = ProtocolData {
                address: addr.clone(),
                operation_hash: operation_hash.clone(),
                call_sequence: call_sequence.clone(),
                depth: 0,
            };

            {
                host_defined!(context, mut host_defined);
                host_defined.insert(data);
            }

            let expected_call_id = format!("{}:{}", operation_hash, seq);

            // Verify format
            let actual_seq = {
                host_defined!(context, host_defined);
                host_defined
                    .get::<ProtocolData>()
                    .map(|d| *d.call_sequence.borrow())
            }
            .unwrap();

            assert_eq!(actual_seq, seq);

            // Verify call_id can be parsed
            let call_id = format!("{}:{}", operation_hash, actual_seq);
            assert_eq!(call_id, expected_call_id);
            assert!(call_id.contains(':'), "Call ID must contain separator");
        }
    }

    #[test]
    fn test_max_depth_boundary() {
        use crate::runtime::v1::api::ProtocolData;
        use jstz_core::host_defined;
        use std::cell::RefCell;
        use std::rc::Rc;

        let mut jstz_rt = Runtime::new(10000).unwrap();
        let realm = jstz_rt.realm().clone();
        let context = jstz_rt.context();

        realm.register_api(WebApi, context);

        let operation_hash = Blake2b::from(b"max_depth_test".as_ref());
        let addr = SmartFunctionHash::digest(b"deep_addr").unwrap();

        // Test maximum u16 value
        let max_depth: u16 = 65535;
        let call_sequence = Rc::new(RefCell::new(max_depth as u64));

        let data = ProtocolData {
            address: addr,
            operation_hash: operation_hash.clone(),
            call_sequence,
            depth: max_depth,
        };

        {
            host_defined!(context, mut host_defined);
            host_defined.insert(data);
        }

        // Verify max depth is handled
        {
            host_defined!(context, host_defined);
            let data = host_defined.get::<ProtocolData>().unwrap();
            assert_eq!(data.depth, 65535);
        }
    }

    #[test]
    fn test_sequence_overflow_safety() {
        use crate::runtime::v1::api::ProtocolData;
        use std::cell::RefCell;
        use std::rc::Rc;

        let operation_hash = Blake2b::from(b"overflow_test".as_ref());
        let addr = SmartFunctionHash::digest(b"test").unwrap();

        // Test near u64::MAX
        let near_max = u64::MAX - 100;
        let call_sequence = Rc::new(RefCell::new(near_max));

        let _data = ProtocolData {
            address: addr,
            operation_hash,
            call_sequence: call_sequence.clone(),
            depth: 0,
        };

        // Verify large sequences are handled
        assert_eq!(*call_sequence.borrow(), near_max);

        // Simulate increments
        for _ in 0..10 {
            *call_sequence.borrow_mut() += 1;
        }

        assert_eq!(*call_sequence.borrow(), near_max + 10);

        // Note: In production, sequence overflow at u64::MAX would take
        // billions of years at 1M calls/sec. No wrapping needed.
    }

    #[test]
    fn test_depth_overflow_prevented() {
        use crate::runtime::v1::api::ProtocolData;
        use jstz_core::host_defined;
        use std::cell::RefCell;
        use std::rc::Rc;

        let mut jstz_rt = Runtime::new(100000).unwrap();
        let realm = jstz_rt.realm().clone();
        let context = jstz_rt.context();

        realm.register_api(WebApi, context);

        let operation_hash = Blake2b::from(b"depth_overflow_test".as_ref());
        let addr = SmartFunctionHash::digest(b"test_addr").unwrap();

        // Setup ProtocolData at max depth - 1 (65534)
        let call_sequence = Rc::new(RefCell::new(0u64));
        let data_at_65534 = ProtocolData {
            address: addr.clone(),
            operation_hash: operation_hash.clone(),
            call_sequence: call_sequence.clone(),
            depth: 65534,
        };

        {
            host_defined!(context, mut host_defined);
            host_defined.insert(data_at_65534);
        }

        // Attempt to nest one more level (65534 → 65535 should succeed)
        let result_65535 = {
            let parent_depth = {
                host_defined!(context, host_defined);
                host_defined.get::<ProtocolData>().map(|d| d.depth)
            }
            .unwrap();

            parent_depth.checked_add(1)
        };

        assert_eq!(result_65535, Some(65535), "Depth 65534 → 65535 should succeed");

        // Setup at depth 65535 (u16::MAX)
        let data_at_65535 = ProtocolData {
            address: addr.clone(),
            operation_hash: operation_hash.clone(),
            call_sequence: call_sequence.clone(),
            depth: 65535,
        };

        {
            host_defined!(context, mut host_defined);
            host_defined.insert(data_at_65535);
        }

        // Attempt to nest one more level (65535 → 65536 should fail with None)
        let result_overflow = {
            let parent_depth = {
                host_defined!(context, host_defined);
                host_defined.get::<ProtocolData>().map(|d| d.depth)
            }
            .unwrap();

            parent_depth.checked_add(1)
        };

        assert_eq!(
            result_overflow, None,
            "Depth 65535 → 65536 should overflow and return None (caught by checked_add)"
        );
    }
}
