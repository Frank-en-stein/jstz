# Nested Smart Function Call Tracing

**Date:** 2024-11-15
**Status:** Design Complete
**Implementation:** ~150 lines of code

---

## Problem

Implicit smart function calls (SF-A → SF-B → SF-C) are **not uniquely identifiable** in kernel logs:

- All nested calls reuse the **same `operation_hash`** from the root operation
- If SF-A calls SF-B **twice**, both invocations log with identical identifiers
- **Indexer cannot reconstruct** the exact call tree or distinguish repeated calls

**Example:**
```javascript
// SF-A code
const resp1 = await SmartFunction.call(requestToB);  // ← Call #1
const resp2 = await SmartFunction.call(requestToB);  // ← Call #2 - looks identical in logs!
```

Current logs:
```
[REQUEST_START] {address: SF-B, request_id: op123}  <- Call #1
[REQUEST_END]   {address: SF-B, request_id: op123}
[REQUEST_START] {address: SF-B, request_id: op123}  <- Call #2 - SAME!
[REQUEST_END]   {address: SF-B, request_id: op123}
```

**Impact:** No complete audit trail, cannot debug nested calls, indexer cannot build accurate call trees.

---

## Solution

Add **two minimal fields** to enable 100% traceability:

1. **`call_sequence`** - Monotonic counter (0, 1, 2, 3...) shared across nested calls
2. **`depth`** - Transaction stack depth (0=root, 1=nested, 2=nested nested...)

**Unique identifier:** `{operation_hash}:{call_sequence}`

---

## Design

### Data Structure

```rust
// crates/jstz_proto/src/runtime/v1/api/mod.rs
pub struct ProtocolData {
    pub address: SmartFunctionHash,
    pub operation_hash: OperationHash,
    pub call_sequence: Rc<RefCell<u64>>,  // NEW: Shared counter
    pub depth: u8,                         // NEW: Call depth
}
```

### Logging Format

```rust
// crates/jstz_proto/src/logger.rs
pub enum RequestEvent {
    Start {
        call_id: String,          // "{operation_hash}:{sequence}"
        address: SmartFunctionHash,
        depth: u8,
    },
    End { /* same */ },
}
```

### Counter Lifecycle

```
Root operation (SF-A):
  ├─ call_sequence = Rc::new(RefCell::new(0))
  ├─ depth = 0
  ├─ Log: call_id=op123:0
  │
  └─ Nested call to SF-B (call #1):
      ├─ Increment: call_sequence = 1
      ├─ depth = 1
      ├─ Log: call_id=op123:1
      │
      └─ Nested call to SF-B (call #2):
          ├─ Increment: call_sequence = 2
          ├─ depth = 1 (same parent level)
          └─ Log: call_id=op123:2  ← NOW UNIQUE!
```

---

## Implementation

### 1. Extend ProtocolData (~10 lines)

```rust
// crates/jstz_proto/src/runtime/v1/api/mod.rs
#[derive(Trace, Finalize, JsData)]
pub struct ProtocolData {
    pub address: SmartFunctionHash,
    pub operation_hash: OperationHash,
    pub call_sequence: Rc<RefCell<u64>>,  // ADD
    pub depth: u8,                         // ADD
}
```

### 2. Initialize at Root (~5 lines)

```rust
// crates/jstz_proto/src/runtime/v1/fetch_handler.rs:182
ProtocolApi {
    operation_hash: operation_hash.clone(),
    address: dest_address.clone(),
    call_sequence: Rc::new(RefCell::new(0)),  // ADD
    depth: 0,                                  // ADD
}
```

### 3. Increment for Nested Calls (~15 lines)

```rust
// crates/jstz_proto/src/runtime/v1/api/smart_function.rs:95
fn fetch(address: &SmartFunctionHash, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    host_defined!(context, host_defined);
    let parent_data = host_defined.get::<ProtocolData>().unwrap();

    // Increment sequence
    let call_seq = {
        let mut seq = parent_data.call_sequence.borrow_mut();
        *seq += 1;
        *seq
    };

    // Create child context with incremented depth
    let child_data = ProtocolData {
        address: address.clone(),
        operation_hash: parent_data.operation_hash.clone(),
        call_sequence: parent_data.call_sequence.clone(), // Share counter
        depth: parent_data.depth + 1,                      // Increment depth
    };

    // Store in context and proceed with call...
}
```

### 4. Update Logging (~20 lines)

```rust
// crates/jstz_proto/src/logger.rs
#[derive(Serialize, Deserialize)]
pub enum RequestEvent {
    Start {
        call_id: String,          // ADD: "{op_hash}:{seq}"
        address: SmartFunctionHash,
        depth: u8,                // ADD
    },
    End {
        call_id: String,          // ADD
        address: SmartFunctionHash,
        depth: u8,                // ADD
    },
}

// crates/jstz_proto/src/runtime/v1/fetch_handler.rs:179
log_request_start(
    format!("{}:{}", operation_hash, call_seq),  // Unique call_id
    dest_address,
    depth,
);
```

---

## Result

### Logs After Implementation

```
[START] call_id=op123:0, addr=SF-A, depth=0
[START] call_id=op123:1, addr=SF-B, depth=1  <- Call #1
[END]   call_id=op123:1, addr=SF-B, depth=1
[START] call_id=op123:2, addr=SF-B, depth=1  <- Call #2 UNIQUE!
[END]   call_id=op123:2, addr=SF-B, depth=1
[END]   call_id=op123:0, addr=SF-A, depth=0
```

### Indexer Reconstruction

```python
def build_call_tree(events):
    """Reconstruct call tree from minimal logs"""
    calls = {}
    stack = []  # Track call hierarchy

    for event in events:
        if event.type == "Start":
            call = {
                'call_id': event.call_id,
                'address': event.address,
                'depth': event.depth,
                'children': []
            }

            # Link to parent
            if stack:
                stack[-1]['children'].append(call)

            stack.append(call)
            calls[event.call_id] = call

        elif event.type == "End":
            stack.pop()

    return calls
```

---

## Cost Analysis

| Metric | Impact |
|--------|--------|
| **Memory** | +16 bytes per operation (Rc counter, shared) |
| **CPU** | ~1ns per call (increment u64) |
| **Storage** | +12 bytes per log event (depth u8 + call_id overhead) |
| **Total Overhead** | <1% |

**vs Alternatives:**
- Transaction stack introspection: Complex, requires exposing internal state
- UUID generation: 128 bits, unnecessary overhead
- Timestamp-based: Non-deterministic, race conditions

---

## Validation

### Test Cases

1. **Single call:** Root only
   - Expected: `call_id=op:0, depth=0`

2. **Linear nesting:** SF-A → SF-B → SF-C
   - Expected: `op:0 (d=0) → op:1 (d=1) → op:2 (d=2)`

3. **Repeated calls:** SF-A calls SF-B twice
   - Expected: `op:0 (d=0) → op:1 (d=1), op:2 (d=1)` - different sequences!

4. **Complex tree:** SF-A → [SF-B → SF-C, SF-D]
   - Expected: Unique call_id for each invocation, correct depth

### Verification

```bash
# Run operation and parse kernel logs
cargo test test_nested_call_tracing

# Check indexer reconstruction
python scripts/verify_call_tree.py < kernel_logs.json
```

---

## Files to Modify

1. `crates/jstz_proto/src/runtime/v1/api/mod.rs` - ProtocolData struct
2. `crates/jstz_proto/src/runtime/v1/api/smart_function.rs` - Increment logic
3. `crates/jstz_proto/src/logger.rs` - RequestEvent format
4. `crates/jstz_proto/src/runtime/v1/fetch_handler.rs` - Root initialization, logging calls

**Total:** ~150 lines changed

---

## Key Principles

✅ **100% Traceability** - Every call uniquely identified
✅ **Minimal Overhead** - <1% performance impact
✅ **Deterministic** - Monotonic counter, reproducible
✅ **Simple** - Single shared counter, no complex state
✅ **Indexer-Friendly** - Easy off-chain reconstruction

---

## References

- **Problem Analysis:** `docs/JSTZ_SMART_FUNCTION_CALL_ANALYSIS.md` (original deep dive)
- **Code Locations:**
  - Operation hash generation: `crates/jstz_proto/src/operation.rs:68-108`
  - Fetch handler: `crates/jstz_proto/src/runtime/v1/fetch_handler.rs:132`
  - Current logging: `crates/jstz_proto/src/logger.rs:56-94`
- **Transaction stack:** `crates/jstz_core/src/kv/transaction.rs:65-67` (depth source)
