# Testing Strategy

How to verify Mycel OS components work correctly.

---

## Test Levels

```
┌─────────────────────────────────────────────┐
│           End-to-End Tests                  │
│    (Full system, user scenarios)            │
├─────────────────────────────────────────────┤
│         Integration Tests                   │
│    (Multiple components together)           │
├─────────────────────────────────────────────┤
│            Unit Tests                       │
│    (Individual functions/structs)           │
└─────────────────────────────────────────────┘
```

---

## Unit Tests

### Config Module

```rust
// mycel-runtime/src/config/mod.rs

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_default_config() {
        let config = MycelConfig::default();
        assert_eq!(config.ollama_url, "http://localhost:11434");
        assert!(config.sandbox_enabled);
    }
    
    #[test]
    fn test_load_config_file() {
        let toml = r#"
            ollama_url = "http://custom:11434"
            local_model = "llama3"
        "#;
        std::fs::write("/tmp/test-config.toml", toml).unwrap();
        
        let config = MycelConfig::load("/tmp/test-config.toml", false).unwrap();
        assert_eq!(config.ollama_url, "http://custom:11434");
        assert_eq!(config.local_model, "llama3");
        
        std::fs::remove_file("/tmp/test-config.toml").ok();
    }
    
    #[test]
    fn test_env_override() {
        std::env::set_var("ANTHROPIC_API_KEY", "test-key");
        let config = MycelConfig::load("/nonexistent", true).unwrap();
        assert_eq!(config.anthropic_api_key, "test-key");
        std::env::remove_var("ANTHROPIC_API_KEY");
    }
}
```

### Intent Module

```rust
// mycel-runtime/src/intent/mod.rs

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_action_type_from_str() {
        assert!(matches!(
            ActionType::from_str("simple_response"),
            Some(ActionType::SimpleResponse)
        ));
        assert!(matches!(
            ActionType::from_str("generate_code"),
            Some(ActionType::GenerateCode)
        ));
        assert!(ActionType::from_str("invalid").is_none());
    }
    
    #[test]
    fn test_intent_creation() {
        let intent = Intent {
            action: "list files".to_string(),
            action_type: ActionType::GenerateCode,
            confidence: 0.9,
            parameters: serde_json::json!({"path": "/home"}),
            requires_cloud: false,
        };
        
        assert_eq!(intent.action, "list files");
        assert!(intent.confidence > 0.8);
    }
}
```

### Context Module

```rust
// mycel-runtime/src/context/mod.rs

#[cfg(test)]
mod tests {
    use super::*;
    
    #[tokio::test]
    async fn test_session_creation() {
        let config = MycelConfig::default();
        let manager = ContextManager::new(&config).await.unwrap();
        
        let ctx = manager.get_context("test-session").await.unwrap();
        assert_eq!(ctx.session_id, "test-session");
        assert!(ctx.conversation_history.is_empty());
    }
    
    #[tokio::test]
    async fn test_session_update() {
        let config = MycelConfig::default();
        let manager = ContextManager::new(&config).await.unwrap();
        
        // First interaction
        manager.get_context("test").await.unwrap();
        manager.update_session("test", "hello", "hi there").await.unwrap();
        
        // Check history
        let ctx = manager.get_context("test").await.unwrap();
        assert_eq!(ctx.conversation_history.len(), 1);
        assert_eq!(ctx.conversation_history[0].user, "hello");
    }
    
    #[tokio::test]
    async fn test_file_access_tracking() {
        let config = MycelConfig::default();
        let manager = ContextManager::new(&config).await.unwrap();
        
        manager.get_context("test").await.unwrap();
        manager.record_file_access("test", "/etc/passwd").await.unwrap();
        manager.record_file_access("test", "/etc/hosts").await.unwrap();
        
        let ctx = manager.get_context("test").await.unwrap();
        assert_eq!(ctx.recent_files[0], "/etc/hosts"); // Most recent first
        assert_eq!(ctx.recent_files[1], "/etc/passwd");
    }
}
```

### CRDT Module (when created)

```rust
// mycel-runtime/src/sync/crdt.rs

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_lww_register_merge_newer_wins() {
        let mut a = LWWRegister::new("first", "node_a");
        std::thread::sleep(std::time::Duration::from_millis(10));
        let b = LWWRegister::new("second", "node_b");
        
        a.merge(&b);
        assert_eq!(a.get(), "second");
    }
    
    #[test]
    fn test_gset_merge() {
        let mut a = GSet::new();
        a.add("one");
        a.add("two");
        
        let mut b = GSet::new();
        b.add("two");
        b.add("three");
        
        a.merge(&b);
        assert!(a.contains(&"one"));
        assert!(a.contains(&"two"));
        assert!(a.contains(&"three"));
    }
    
    #[test]
    fn test_orset_add_remove() {
        let mut set = ORSet::new();
        
        set.add("item", "node_a");
        assert!(set.contains(&"item"));
        
        set.remove(&"item", "node_a");
        assert!(!set.contains(&"item"));
    }
    
    #[test]
    fn test_orset_concurrent_add_remove() {
        let mut a = ORSet::new();
        let mut b = ORSet::new();
        
        // A adds
        a.add("item", "node_a");
        
        // B removes (without seeing A's add)
        // This should NOT remove A's add
        
        // Merge
        a.merge(&b);
        
        // Item should still exist (add wins over concurrent remove)
        assert!(a.contains(&"item"));
    }
}
```

---

## Integration Tests

### AI Router + Ollama

```rust
// tests/integration/ai_test.rs

use mycel_runtime::{config::MycelConfig, ai::AiRouter};

#[tokio::test]
#[ignore]  // Requires running Ollama
async fn test_ollama_integration() {
    let config = MycelConfig::default();
    let router = AiRouter::new(&config).await.unwrap();
    
    let context = create_test_context();
    let response = router.generate_response("What is 2+2?", &context).await;
    
    assert!(response.is_ok());
    let text = response.unwrap();
    assert!(text.contains("4") || text.contains("four"));
}

#[tokio::test]
#[ignore]  // Requires running Ollama
async fn test_intent_parsing() {
    let config = MycelConfig::default();
    let router = AiRouter::new(&config).await.unwrap();
    
    let context = create_test_context();
    
    // Simple question should be SimpleResponse
    let intent = router.parse_intent("What time is it?", &context).await.unwrap();
    assert!(matches!(intent.action_type, ActionType::SimpleResponse));
    
    // File operation should be GenerateCode
    let intent = router.parse_intent("List files in /home", &context).await.unwrap();
    assert!(matches!(intent.action_type, ActionType::GenerateCode));
}
```

### IPC + Runtime

```rust
// tests/integration/ipc_test.rs

use std::os::unix::net::UnixStream;
use std::io::{Write, BufRead, BufReader};

#[tokio::test]
async fn test_ipc_ping() {
    // Start runtime in background
    let _runtime = start_test_runtime().await;
    
    // Connect to socket
    let mut stream = UnixStream::connect("/tmp/mycel-test.sock").unwrap();
    
    // Send ping
    stream.write_all(b"{\"type\":\"ping\"}\n").unwrap();
    
    // Read response
    let mut reader = BufReader::new(&stream);
    let mut response = String::new();
    reader.read_line(&mut response).unwrap();
    
    assert!(response.contains("pong"));
}

#[tokio::test]
async fn test_ipc_chat() {
    let _runtime = start_test_runtime().await;
    
    let mut stream = UnixStream::connect("/tmp/mycel-test.sock").unwrap();
    
    let request = serde_json::json!({
        "type": "chat",
        "session_id": "test",
        "input": "hello"
    });
    
    stream.write_all(format!("{}\n", request).as_bytes()).unwrap();
    
    let mut reader = BufReader::new(&stream);
    let mut response = String::new();
    reader.read_line(&mut response).unwrap();
    
    let parsed: serde_json::Value = serde_json::from_str(&response).unwrap();
    assert!(parsed.get("content").is_some() || parsed.get("text").is_some());
}
```

### Executor + Sandbox

```rust
// tests/integration/executor_test.rs

use mycel_runtime::{config::MycelConfig, executor::CodeExecutor};

#[tokio::test]
async fn test_python_execution() {
    let config = MycelConfig::default();
    let executor = CodeExecutor::new(&config).unwrap();
    
    let code = r#"
print("Hello from sandbox")
print(2 + 2)
"#;
    
    let output = executor.run(code).await.unwrap();
    assert!(output.contains("Hello from sandbox"));
    assert!(output.contains("4"));
}

#[tokio::test]
async fn test_sandbox_network_blocked() {
    let config = MycelConfig::default();
    let executor = CodeExecutor::new(&config).unwrap();
    
    let code = r#"
import socket
s = socket.socket()
s.connect(("google.com", 80))
"#;
    
    let result = executor.run(code).await;
    // Should fail because network is blocked
    assert!(result.is_err() || result.unwrap().contains("error"));
}

#[tokio::test]
async fn test_execution_timeout() {
    let config = MycelConfig::default();
    let executor = CodeExecutor::new(&config).unwrap();
    
    let code = r#"
import time
time.sleep(60)  # Should timeout before this completes
"#;
    
    let start = std::time::Instant::now();
    let result = executor.run(code).await;
    let elapsed = start.elapsed();
    
    // Should timeout in ~30 seconds, not 60
    assert!(elapsed.as_secs() < 40);
    assert!(result.is_err());
}
```

---

## End-to-End Tests

### Manual Test Script

```bash
#!/bin/bash
# tests/e2e/test_full_flow.sh

set -e

echo "=== Mycel OS End-to-End Test ==="

# 1. Start Ollama (if not running)
if ! curl -s http://localhost:11434/api/tags > /dev/null; then
    echo "Starting Ollama..."
    ollama serve &
    sleep 5
fi

# 2. Ensure model is available
echo "Checking model..."
ollama pull phi3:mini 2>/dev/null || true

# 3. Build runtime
echo "Building runtime..."
cargo build --release

# 4. Start runtime
echo "Starting runtime..."
./target/release/mycel-runtime --dev &
RUNTIME_PID=$!
sleep 2

# 5. Test ping
echo "Testing ping..."
RESPONSE=$(echo '{"type":"ping"}' | nc -U /tmp/mycel-dev.sock)
echo "Ping response: $RESPONSE"
[[ "$RESPONSE" == *"pong"* ]] || { echo "FAIL: ping"; exit 1; }

# 6. Test chat
echo "Testing chat..."
RESPONSE=$(echo '{"type":"chat","session_id":"test","input":"What is 2+2?"}' | nc -U /tmp/mycel-dev.sock)
echo "Chat response: $RESPONSE"
[[ "$RESPONSE" == *"4"* ]] || [[ "$RESPONSE" == *"four"* ]] || { echo "FAIL: chat"; exit 1; }

# 7. Test code execution
echo "Testing code execution..."
RESPONSE=$(echo '{"type":"chat","session_id":"test","input":"Run Python code to print hello world"}' | nc -U /tmp/mycel-dev.sock)
echo "Code response: $RESPONSE"
[[ "$RESPONSE" == *"hello"* ]] || [[ "$RESPONSE" == *"Hello"* ]] || { echo "FAIL: code"; exit 1; }

# 8. Cleanup
echo "Cleaning up..."
kill $RUNTIME_PID 2>/dev/null || true

echo "=== All tests passed! ==="
```

### CLI Integration Test

```python
#!/usr/bin/env python3
# tests/e2e/test_cli.py

import subprocess
import time
import sys

def test_cli():
    # Start runtime
    runtime = subprocess.Popen(
        ['cargo', 'run', '--', '--dev'],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE
    )
    time.sleep(3)
    
    try:
        # Test status
        result = subprocess.run(
            ['python3', 'tools/mycel-cli.py', 'status'],
            capture_output=True,
            text=True,
            timeout=10
        )
        assert 'Running' in result.stdout or 'error' not in result.stdout.lower()
        print("✓ Status check passed")
        
        # Test chat (non-interactive)
        result = subprocess.run(
            ['python3', 'tools/mycel-cli.py', 'run', 'What is 2+2?'],
            capture_output=True,
            text=True,
            timeout=30
        )
        assert '4' in result.stdout or 'four' in result.stdout.lower()
        print("✓ Chat test passed")
        
    finally:
        runtime.terminate()
        runtime.wait()
    
    print("\n=== All CLI tests passed! ===")

if __name__ == '__main__':
    test_cli()
```

---

## Test Configuration

### Cargo.toml Test Settings

```toml
[dev-dependencies]
tokio-test = "0.4"
tempfile = "3.10"
mockall = "0.12"

[[test]]
name = "integration"
path = "tests/integration/mod.rs"
```

### Test Fixtures

```rust
// tests/common/mod.rs

pub fn create_test_config() -> MycelConfig {
    MycelConfig {
        ollama_url: "http://localhost:11434".to_string(),
        local_model: "phi3:mini".to_string(),
        ipc_socket_path: "/tmp/mycel-test.sock".to_string(),
        context_path: tempfile::tempdir().unwrap().path().to_string_lossy().to_string(),
        ..Default::default()
    }
}

pub fn create_test_context() -> Context {
    Context {
        session_id: "test".to_string(),
        working_directory: "/tmp".to_string(),
        recent_files: vec![],
        conversation_history: vec![],
        timestamp: chrono::Utc::now(),
        user_name: None,
        user_preferences: std::collections::HashMap::new(),
    }
}

pub async fn start_test_runtime() -> tokio::task::JoinHandle<()> {
    let config = create_test_config();
    tokio::spawn(async move {
        // Start runtime
    })
}
```

---

## Running Tests

```bash
# All unit tests
cargo test

# With output
cargo test -- --nocapture

# Specific test
cargo test test_lww_register

# Integration tests (requires Ollama)
cargo test --test integration

# Ignored tests (require external services)
cargo test -- --ignored

# E2E tests
./tests/e2e/test_full_flow.sh
python3 tests/e2e/test_cli.py
```

---

## CI/CD Pipeline

```yaml
# .github/workflows/test.yml

name: Test

on: [push, pull_request]

jobs:
  unit-tests:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - run: cargo test --lib

  integration-tests:
    runs-on: ubuntu-latest
    services:
      ollama:
        image: ollama/ollama
        ports:
          - 11434:11434
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - run: |
          curl http://localhost:11434/api/pull -d '{"name":"phi3:mini"}'
          cargo test --test integration

  lint:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with:
          components: clippy, rustfmt
      - run: cargo clippy -- -D warnings
      - run: cargo fmt -- --check
```

---

## Test Coverage Goals

| Component | Target | Notes |
|-----------|--------|-------|
| Config | 90% | Critical for correctness |
| Intent | 80% | Parsing edge cases |
| Context | 80% | State management |
| AI Router | 60% | External dependency |
| Executor | 70% | Sandbox security critical |
| IPC | 70% | Protocol correctness |
| Sync | 90% | CRDT correctness critical |
