# Mycel OS - Prioritized Task List

Work through these in order. Each task should be completable in one session.

---

## Priority 1: Get It Compiling (BLOCKING)

### Task 1.1: Verify Compilation
```bash
cd mycel-runtime
cargo build 2>&1
```

**Expected:** Clean compilation with possible warnings
**If errors:** Fix them before proceeding

**Common fixes needed:**
- [ ] Missing imports in modules
- [ ] Type mismatches between modules
- [ ] Lifetime annotations
- [ ] Feature flags for optional deps

### Task 1.2: Run Basic Test
```bash
cargo run -- --dev --verbose --no-local-llm
```

**Expected:** Starts, shows banner, waits for IPC
**If crashes:** Check error, fix, retry

### Task 1.3: Add Basic Tests
```bash
cargo test
```

Create tests for:
- [ ] `config/mod.rs` - Config loading, defaults
- [ ] `intent/mod.rs` - ActionType parsing
- [ ] `context/mod.rs` - Session creation

---

## Priority 2: Ollama Integration (Core Feature)

### Task 2.1: Test Ollama Connectivity
```rust
// In ai/mod.rs, add test:
#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn test_ollama_available() {
        let client = reqwest::Client::new();
        let resp = client.get("http://localhost:11434/api/tags").send().await;
        assert!(resp.is_ok());
    }
}
```

### Task 2.2: Test Generation
```rust
#[tokio::test]
async fn test_ollama_generate() {
    // Test actual generation
}
```

### Task 2.3: Handle Streaming (Optional Enhancement)
Current implementation uses `stream: false`. For better UX:
- [ ] Implement streaming responses
- [ ] Yield tokens as they arrive
- [ ] Show typing indicator in CLI

---

## Priority 3: IPC Server (Required for CLI)

### Task 3.1: Verify Socket Creation
```bash
cargo run -- --dev &
ls -la /tmp/mycel-dev.sock
```

### Task 3.2: Test Basic Message
```bash
echo '{"type":"ping"}' | nc -U /tmp/mycel-dev.sock
```

**Expected:** `{"type":"pong"}`

### Task 3.3: Test Chat Message
```bash
echo '{"type":"chat","session_id":"test","input":"hello"}' | nc -U /tmp/mycel-dev.sock
```

### Task 3.4: Implement Missing Handlers
Check `ipc/mod.rs` for TODO items:
- [ ] `handle_chat` - Route to AI
- [ ] `handle_status` - Return runtime status
- [ ] `handle_exec` - Execute code
- [ ] Error responses

---

## Priority 4: Code Executor (AI Superpower)

### Task 4.1: Test Sandbox Availability
```bash
which firejail    # Should return path
which bwrap       # Fallback
```

### Task 4.2: Test Manual Sandbox
```bash
firejail --quiet --private --net=none python3 -c "print('hello')"
```

### Task 4.3: Implement Executor
In `executor/mod.rs`:
- [ ] Write code to temp file
- [ ] Run via firejail
- [ ] Capture output
- [ ] Enforce timeout (tokio::time::timeout)
- [ ] Clean up temp file

### Task 4.4: Test End-to-End
```
mycel> count lines in /etc/passwd
# Should: generate Python, execute, return count
```

---

## Priority 5: CLI Integration

### Task 5.1: Test Python CLI
```bash
python3 tools/mycel-cli.py status
```

### Task 5.2: Test Interactive Mode
```bash
python3 tools/mycel-cli.py
mycel> hello
mycel> quit
```

### Task 5.3: Fix Any Issues
- [ ] Socket connection errors
- [ ] JSON parsing errors
- [ ] Response formatting

---

## Priority 6: Context Persistence

### Task 6.1: Verify Context Saves
```bash
# After some interactions
ls -la ./mycel-data/
cat ./mycel-data/user_context.json
```

### Task 6.2: Test Session Continuity
```
mycel> my name is Alice
mycel> what's my name?
# Should remember "Alice"
```

### Task 6.3: Test Across Restarts
```bash
# Stop runtime, restart
cargo run -- --dev
# Verify context persisted
```

---

## Priority 7: Error Handling

### Task 7.1: Handle Ollama Down
```bash
# Stop Ollama, try to chat
# Should: graceful error, suggest starting Ollama
```

### Task 7.2: Handle Bad Input
```
mycel> [send malformed JSON via nc]
# Should: error response, not crash
```

### Task 7.3: Handle Timeout
```
mycel> [request that takes too long]
# Should: timeout, return error
```

---

## Priority 8: Device Sync (New Module)

### Task 8.1: Create Module Structure
```bash
mkdir -p mycel-runtime/src/sync
touch mycel-runtime/src/sync/mod.rs
touch mycel-runtime/src/sync/mesh.rs
touch mycel-runtime/src/sync/crdt.rs
touch mycel-runtime/src/sync/pairing.rs
```

### Task 8.2: Implement CRDT Types
```rust
// sync/crdt.rs
pub enum CrdtValue {
    LWWRegister { value: Vec<u8>, timestamp: u64, node_id: String },
    GSet { elements: HashSet<String> },
    ORSet { /* ... */ },
}

impl CrdtValue {
    pub fn merge(&mut self, other: &Self) { /* ... */ }
}
```

### Task 8.3: Implement WireGuard Mesh
```rust
// sync/mesh.rs
pub struct MeshNetwork {
    interface: String,
    peers: Vec<Peer>,
}
```

### Task 8.4: Implement Pairing
```rust
// sync/pairing.rs
pub fn generate_pairing_code() -> String { /* ... */ }
pub fn parse_pairing_code(code: &str) -> Result<PairingInfo> { /* ... */ }
```

---

## Priority 9: Collective Intelligence

### Task 9.1: NEAR Testnet Setup
- [ ] Create testnet account
- [ ] Deploy pattern registry contract
- [ ] Test registration

### Task 9.2: IPFS Integration
- [ ] Add IPFS client
- [ ] Store pattern
- [ ] Retrieve by CID

### Task 9.3: Privacy Layer
- [ ] PII detection regex
- [ ] Replacement with placeholders
- [ ] Differential privacy noise

---

## Priority 10: ISO Build

### Task 10.1: Test Docker Build
```bash
# Verify Docker works
docker run hello-world
```

### Task 10.2: Build Minimal ISO
```bash
./scripts/build-iso.sh quick
```

**Expected output:**
- `output/mycel-os-minimal-*.iso` (~700MB-1GB)
- Build time: 5-10 minutes

### Task 10.3: Test ISO in QEMU
```bash
./scripts/test-iso.sh
```

**In the VM:**
- Login: `root` (no password or `voidlinux`)
- Test networking: `ping google.com`
- Exit QEMU: `Ctrl+A, X`

### Task 10.4: Build Full ISO with Runtime
```bash
# First build the runtime
cd mycel-runtime
cargo build --release

# Then build full ISO
cd ..
./scripts/build-iso.sh full
```

### Task 10.5: Verify Runtime in ISO
```bash
# Boot ISO
./scripts/test-iso.sh

# In VM, check for runtime
which mycel-runtime
mycel-runtime --version
```

---

## Stretch Goals (After Core Complete)

### S1: Streaming Responses
- [ ] Ollama streaming
- [ ] Token-by-token output
- [ ] Progress indicators

### S2: Multiple Models
- [ ] Model selection in config
- [ ] Runtime model switching
- [ ] Model recommendations

### S3: GUI Shell
- [ ] Wayland compositor research
- [ ] Basic window management
- [ ] Conversation panel

### S4: Windows App Support
- [ ] Wine integration
- [ ] Bottles for prefix management
- [ ] AI-assisted installation

---

## Done Checklist

When complete, you should be able to:

- [ ] `cargo build --release` - No errors
- [ ] `cargo test` - All tests pass
- [ ] Start runtime in dev mode
- [ ] Chat via CLI
- [ ] Execute generated code safely
- [ ] Context persists across restarts
- [ ] Two instances sync (stretch)
- [ ] ISO boots in VM (stretch)

---

## Session Workflow

Each coding session:

1. **Pick next uncompleted task**
2. **Understand the goal**
3. **Implement minimally**
4. **Test it works**
5. **Commit with clear message**
6. **Move to next task**

Don't over-engineer. Get it working, then improve.
