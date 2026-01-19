# Troubleshooting Guide

Common issues and solutions when developing Mycel OS.

---

## Compilation Errors

### "cannot find type `MycelConfig`"

**Problem:** Module imports missing

**Solution:**
```rust
// In the file with the error, add:
use crate::config::MycelConfig;
```

### "the trait `Clone` is not implemented"

**Problem:** Struct needs Clone for async operations

**Solution:**
```rust
// Add derive macro:
#[derive(Clone)]
pub struct MyStruct { ... }

// Or for types with non-Clone fields, wrap in Arc:
use std::sync::Arc;
pub struct MyStruct {
    inner: Arc<NonCloneType>,
}
```

### "borrowed value does not live long enough"

**Problem:** Lifetime issues with async

**Solution:**
```rust
// Option 1: Clone the value
let owned = borrowed.clone();
tokio::spawn(async move {
    use_value(owned).await;
});

// Option 2: Use Arc
let shared = Arc::new(value);
let shared_clone = shared.clone();
tokio::spawn(async move {
    use_value(&shared_clone).await;
});
```

### "future cannot be sent between threads safely"

**Problem:** Non-Send type in async block

**Solution:**
```rust
// Wrap in Arc<Mutex> or Arc<RwLock>
use tokio::sync::RwLock;
use std::sync::Arc;

let state = Arc::new(RwLock::new(my_state));
```

### "unresolved import"

**Problem:** Module not declared or wrong path

**Solution:**
```rust
// In parent mod.rs or main.rs:
mod mymodule;  // For mymodule/mod.rs or mymodule.rs

// Then import:
use crate::mymodule::MyType;
```

---

## Runtime Errors

### "Connection refused" (Ollama)

**Problem:** Ollama not running

**Solution:**
```bash
# Start Ollama
ollama serve

# Verify it's running
curl http://localhost:11434/api/tags

# If port conflict:
OLLAMA_HOST=127.0.0.1:11435 ollama serve
```

### "No such file or directory" (socket)

**Problem:** IPC socket directory doesn't exist

**Solution:**
```bash
# Dev mode
mkdir -p /tmp

# Production
sudo mkdir -p /run/mycel
sudo chown $USER /run/mycel
```

### "Permission denied" (socket)

**Problem:** Wrong permissions on socket or directory

**Solution:**
```bash
# Check permissions
ls -la /run/mycel/

# Fix
sudo chown -R $USER:$USER /run/mycel/
chmod 700 /run/mycel/
```

### "Address already in use"

**Problem:** Previous instance still running

**Solution:**
```bash
# Find and kill
pkill mycel-runtime

# Or find by port
lsof -i :11434  # Ollama
```

### "Config file not found"

**Problem:** Config path incorrect

**Solution:**
```bash
# Dev mode - use project config
cargo run -- --dev --config ./config/config.toml

# Or create default
mkdir -p /etc/mycel
cp config/config.toml /etc/mycel/
```

---

## Ollama Issues

### "model not found"

**Problem:** Model not pulled

**Solution:**
```bash
# List available models
ollama list

# Pull the model
ollama pull phi3:mini    # Small, fast
ollama pull phi3:medium  # Better quality
```

### "out of memory"

**Problem:** Model too large for RAM

**Solution:**
```bash
# Use smaller model
ollama pull phi3:mini      # ~2GB
# Instead of
ollama pull llama3:70b     # ~40GB

# Or set memory limit
OLLAMA_MAX_MEMORY=4G ollama serve
```

### Slow responses

**Problem:** No GPU acceleration

**Solution:**
```bash
# Check if GPU detected
ollama ps

# For NVIDIA:
# Ensure CUDA drivers installed
nvidia-smi

# For AMD:
# Ensure ROCm installed
rocminfo
```

### "context length exceeded"

**Problem:** Input too long for model

**Solution:**
```rust
// Truncate input
let max_chars = 4000;
let truncated = if input.len() > max_chars {
    &input[..max_chars]
} else {
    input
};
```

---

## IPC Issues

### "broken pipe"

**Problem:** Client disconnected mid-request

**Solution:**
```rust
// Handle gracefully
match stream.write_all(&response).await {
    Ok(_) => {},
    Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {
        // Client disconnected, ignore
    },
    Err(e) => return Err(e.into()),
}
```

### "invalid JSON"

**Problem:** Malformed request

**Solution:**
```rust
// Parse with error handling
let request: Request = match serde_json::from_str(&line) {
    Ok(r) => r,
    Err(e) => {
        let error_response = json!({
            "type": "error",
            "message": format!("Invalid JSON: {}", e)
        });
        stream.write_all(error_response.to_string().as_bytes()).await?;
        continue;
    }
};
```

### Python CLI can't connect

**Problem:** Wrong socket path

**Solution:**
```python
# In mycel-cli.py, check:
SOCKET_PATH = "/tmp/mycel-dev.sock"  # Dev mode
# or
SOCKET_PATH = "/run/mycel/runtime.sock"  # Production
```

---

## Sandbox Issues

### "firejail: command not found"

**Problem:** Firejail not installed

**Solution:**
```bash
# Void Linux
sudo xbps-install -S firejail

# Ubuntu/Debian
sudo apt install firejail

# Fedora
sudo dnf install firejail
```

### "operation not permitted" in sandbox

**Problem:** Trying to access restricted resource

**Solution:**
```bash
# Check firejail profile
firejail --list

# Debug sandbox
firejail --debug python3 script.py

# For specific needs, create custom profile
# ~/.config/firejail/mycel-sandbox.profile
```

### Code execution timeout

**Problem:** Code takes too long

**Solution:**
```rust
// Adjust timeout
use tokio::time::{timeout, Duration};

let result = timeout(
    Duration::from_secs(30),  // Increase if needed
    execute_code(&code)
).await;

match result {
    Ok(output) => output,
    Err(_) => Err(anyhow!("Execution timed out")),
}
```

---

## Docker Issues

### "no space left on device"

**Problem:** Docker cache full

**Solution:**
```bash
# Clean up
docker system prune -a

# Check space
docker system df
```

### Build fails on package install

**Problem:** Package name changed or unavailable

**Solution:**
```bash
# Search for correct package name
xbps-query -Rs <package>

# Update package list in Dockerfile
```

### Container can't reach network

**Problem:** DNS or network mode issue

**Solution:**
```yaml
# In docker-compose.yml:
services:
  mycel-dev:
    dns:
      - 8.8.8.8
      - 8.8.4.4
```

---

## Sync Issues

### WireGuard interface won't come up

**Problem:** Module not loaded or permissions

**Solution:**
```bash
# Load module
sudo modprobe wireguard

# Check
lsmod | grep wireguard

# If not available, kernel too old
# Need Linux 5.6+ or install wireguard-dkms
```

### Devices can't see each other

**Problem:** NAT or firewall blocking

**Solution:**
```bash
# Check firewall
sudo iptables -L -n | grep 51820

# Allow WireGuard port
sudo iptables -A INPUT -p udp --dport 51820 -j ACCEPT

# For NAT traversal, ensure at least one device has public IP
# Or use relay node
```

### Sync stuck

**Problem:** Deadlock or blocked async task

**Solution:**
```rust
// Add timeout to sync operations
let result = timeout(Duration::from_secs(10), sync_operation()).await;

// Use non-blocking locks
let guard = state.try_write()?;
```

---

## Common Mistakes

### Using blocking I/O in async

**Wrong:**
```rust
async fn bad() {
    let content = std::fs::read_to_string("file")?;  // BLOCKS!
}
```

**Right:**
```rust
async fn good() {
    let content = tokio::fs::read_to_string("file").await?;
}
```

### Holding lock across await

**Wrong:**
```rust
async fn bad(state: Arc<RwLock<State>>) {
    let guard = state.write().await;
    some_async_operation().await;  // Lock held across await!
}
```

**Right:**
```rust
async fn good(state: Arc<RwLock<State>>) {
    {
        let guard = state.write().await;
        // Do quick work
    }  // Lock released
    some_async_operation().await;
}
```

### Forgetting error propagation

**Wrong:**
```rust
fn process() {
    risky_operation();  // Error ignored!
}
```

**Right:**
```rust
fn process() -> Result<()> {
    risky_operation()?;  // Error propagated
    Ok(())
}
```

---

## Debug Techniques

### Enable verbose logging

```bash
RUST_LOG=debug cargo run -- --dev --verbose
```

### Print request/response

```rust
tracing::debug!("Request: {:?}", request);
tracing::debug!("Response: {:?}", response);
```

### Test individual function

```rust
#[cfg(test)]
mod tests {
    #[test]
    fn test_specific_thing() {
        // Isolate and test
    }
}
```

### Check Ollama directly

```bash
curl http://localhost:11434/api/generate -d '{
  "model": "phi3:mini",
  "prompt": "Hello",
  "stream": false
}'
```

### Trace IPC messages

```bash
# In one terminal, run runtime
cargo run -- --dev

# In another, watch socket
strace -e read,write nc -U /tmp/mycel-dev.sock
```

---

## Getting Help

1. **Check logs first:** `RUST_LOG=debug`
2. **Simplify:** Reproduce with minimal code
3. **Search errors:** Copy exact error message
4. **Check dependencies:** `cargo update`
5. **Fresh start:** `cargo clean && cargo build`
