# Architectural Decision Records (ADRs)

This document captures key decisions made during Mycel OS design. Claude Code should follow these decisions, not revisit them.

---

## ADR-001: Base Operating System

**Decision:** Fork Void Linux (musl), not Ubuntu or other distros

**Context:** Needed a base OS that is:
- Independent (no upstream politics)
- Simple to understand and modify
- Fast booting
- Minimal

**Options Considered:**
| Option | Pros | Cons | Verdict |
|--------|------|------|---------|
| Ubuntu | Popular, lots of docs | Heavy, systemd, Canonical control | ❌ |
| Alpine | Small, musl | Container-focused, weak desktop | ❌ |
| Arch | Rolling, AUR | Complex, moving target | ❌ |
| NixOS | Declarative, reproducible | Too complex to fork | ❌ |
| Gentoo | Flexible | Compile times | ❌ |
| **Void Linux** | Independent, runit, musl, simple | Smaller community | ✅ |

**Consequences:**
- Use XBPS package manager
- Use runit for init (not systemd)
- Compile against musl libc
- Can't use glibc-only software directly

---

## ADR-002: Primary Language

**Decision:** Rust for all core components

**Context:** Need safety, performance, and good async support for:
- Long-running daemon
- Network operations
- Sandboxing
- Concurrent AI requests

**Options Considered:**
| Option | Pros | Cons | Verdict |
|--------|------|------|---------|
| Go | Simple, fast compile | GC pauses, less control | ❌ |
| C++ | Fast, existing libs | Memory safety issues | ❌ |
| Python | Fast development | Too slow for daemon | ❌ |
| **Rust** | Safe, fast, good async | Steeper learning curve | ✅ |

**Consequences:**
- Use tokio for async runtime
- Use serde for serialization
- Longer compile times but safer code

---

## ADR-003: Local AI Backend

**Decision:** Ollama for local LLM inference

**Context:** Need to run LLMs locally for:
- Privacy (data never leaves device)
- Speed (no network latency)
- Offline operation

**Options Considered:**
| Option | Pros | Cons | Verdict |
|--------|------|------|---------|
| llama.cpp direct | Fast, no overhead | Complex integration | ❌ |
| vLLM | Fast, production-grade | Heavy, server-focused | ❌ |
| LocalAI | OpenAI-compatible | Less mature | ❌ |
| **Ollama** | Easy setup, good API, model management | Extra process | ✅ |

**Consequences:**
- Ollama runs as separate service
- HTTP API at localhost:11434
- Models managed via `ollama pull`
- Support phi3, mistral, llama models

---

## ADR-004: Cloud AI Backend

**Decision:** Anthropic Claude API for complex reasoning

**Context:** Local models have limits. Need cloud for:
- Complex reasoning
- Long context
- High-quality code generation
- When local model is uncertain

**Options Considered:**
| Option | Pros | Cons | Verdict |
|--------|------|------|---------|
| OpenAI | Popular, good models | API instability, company concerns | ❌ |
| Google | Gemini is capable | Privacy concerns | ❌ |
| **Anthropic** | Best reasoning, safety-focused | Cost | ✅ |

**Consequences:**
- Requires ANTHROPIC_API_KEY
- Use claude-sonnet-4-20250514 model
- Implement smart routing (local first, cloud for complex)
- Handle API errors gracefully

---

## ADR-005: IPC Mechanism

**Decision:** Unix domain socket with JSON protocol

**Context:** Need communication between:
- CLI tools and runtime
- GUI and runtime
- Other system components

**Options Considered:**
| Option | Pros | Cons | Verdict |
|--------|------|------|---------|
| D-Bus | Standard on Linux | Complex, heavy | ❌ |
| gRPC | Fast, typed | Requires protobuf tooling | ❌ |
| REST over HTTP | Familiar | Overhead, port management | ❌ |
| **Unix socket + JSON** | Simple, fast, secure | Custom protocol | ✅ |

**Consequences:**
- Socket at /run/mycel/runtime.sock (prod) or /tmp/mycel-dev.sock (dev)
- JSON messages, newline-delimited
- Easy to test with netcat
- No network exposure by default

---

## ADR-006: Code Sandbox

**Decision:** Firejail as primary sandbox, bubblewrap as fallback

**Context:** AI-generated code must run safely:
- No filesystem damage
- No network access (by default)
- Resource limits
- Timeout enforcement

**Options Considered:**
| Option | Pros | Cons | Verdict |
|--------|------|------|---------|
| Docker | Strong isolation | Heavy, startup time | ❌ |
| systemd-nspawn | Good isolation | Requires systemd | ❌ |
| chroot | Simple | Weak isolation | ❌ |
| seccomp only | Lightweight | Complex to configure | ❌ |
| **Firejail** | Easy, comprehensive | Extra dependency | ✅ |
| bubblewrap | Minimal, Flatpak uses it | Less features | Fallback |

**Consequences:**
- Generated code runs via `firejail --quiet --private --net=none`
- 30 second default timeout
- 512MB memory limit
- Capture stdout/stderr

---

## ADR-007: Device Sync Transport

**Decision:** WireGuard mesh network

**Context:** Users want devices to sync:
- Encrypted end-to-end
- Works on LAN and internet
- No central server
- NAT traversal

**Options Considered:**
| Option | Pros | Cons | Verdict |
|--------|------|------|---------|
| SSH tunnels | Familiar | Complex key management | ❌ |
| Tailscale | Easy setup | Relies on coordination server | ❌ |
| ZeroTier | Similar to Tailscale | Same issue | ❌ |
| **WireGuard** | Fast, simple, audited | Manual config | ✅ |

**Consequences:**
- Each device has WireGuard keypair
- Mesh topology (all devices connect to all)
- mDNS for local discovery
- Relay nodes for NAT traversal

---

## ADR-008: Sync Data Structure

**Decision:** Hypercore + CRDTs

**Context:** Need conflict-free sync:
- Devices may be offline
- Changes happen concurrently
- Must merge without conflicts

**Options Considered:**
| Option | Pros | Cons | Verdict |
|--------|------|------|---------|
| Git | Familiar, robust | Conflicts require resolution | ❌ |
| Syncthing | Works well | Not programmable | ❌ |
| rsync | Simple | Last-write-wins only | ❌ |
| **Hypercore + CRDT** | Append-only + conflict-free | More complex | ✅ |

**Consequences:**
- Hypercore for append-only event log
- CRDTs for state: LWW registers, G-Sets, OR-Sets
- Deterministic merge on all devices
- Slightly more storage overhead

---

## ADR-009: Coordination Layer

**Decision:** NEAR Protocol for registry and payments

**Context:** Need decentralized coordination:
- Pattern registry (who created what)
- Micropayments (pay for patterns)
- Identity (human-readable names)
- Governance (protocol upgrades)

**Options Considered:**
| Option | Pros | Cons | Verdict |
|--------|------|------|---------|
| Ethereum | Most ecosystem | High gas fees | ❌ |
| Solana | Fast, cheap | Less decentralized, outages | ❌ |
| Polygon | Cheap L2 | Still Ethereum complexity | ❌ |
| **NEAR** | $0.001 fees, human names, Rust SDK | Smaller ecosystem | ✅ |

**Consequences:**
- Accounts like alice.mycel.near
- Smart contracts in Rust
- ~$0.001 per transaction
- 1-2 second finality

---

## ADR-010: AI Quality Evaluation

**Decision:** Bittensor subnet for pattern evaluation

**Context:** Need to evaluate pattern quality:
- Decentralized (no central authority)
- Economic incentives (good = rewards)
- AI-native (understands patterns)

**Options Considered:**
| Option | Pros | Cons | Verdict |
|--------|------|------|---------|
| Manual curation | Simple | Doesn't scale | ❌ |
| Voting | Democratic | Sybil attacks | ❌ |
| Reputation | Works | Bootstrap problem | ❌ |
| **Bittensor** | AI evaluation, economic incentives | Complex setup | ✅ |

**Consequences:**
- Create Mycel subnet on Bittensor
- Miners evaluate patterns
- Validators verify evaluations
- TAO rewards for quality contributions

---

## ADR-011: Privacy Protection

**Decision:** Differential privacy + PII stripping

**Context:** Share patterns WITHOUT leaking private data:
- Patterns useful to collective
- Individual data stays private
- Can't reverse-engineer source

**Approach:**
1. **PII Detection** - Scan for names, emails, paths, credentials
2. **PII Removal** - Replace with placeholders [NAME], [EMAIL], etc.
3. **Differential Privacy** - Add calibrated noise to any aggregates
4. **User Approval** - Always ask before sharing

**Consequences:**
- Privacy epsilon budget tracking
- Some patterns may be too sensitive to share
- Quality slightly reduced by noise

---

## ADR-012: Configuration Format

**Decision:** TOML for configuration files

**Context:** Need human-readable, editable config

**Options Considered:**
| Option | Pros | Cons | Verdict |
|--------|------|------|---------|
| JSON | Universal | No comments, verbose | ❌ |
| YAML | Readable | Whitespace sensitive, complex | ❌ |
| INI | Simple | Limited structure | ❌ |
| **TOML** | Readable, comments, typed | Less known | ✅ |

**Consequences:**
- Config at /etc/mycel/config.toml
- Rust `toml` crate for parsing
- Environment variables can override

---

## Decisions NOT to Revisit

These are settled. Don't propose alternatives:

1. ✅ Void Linux as base (not Ubuntu, Alpine, etc.)
2. ✅ Rust as primary language
3. ✅ Ollama for local AI
4. ✅ Claude for cloud AI
5. ✅ Unix socket for IPC
6. ✅ WireGuard for sync transport
7. ✅ NEAR for coordination
8. ✅ Bittensor for AI evaluation
9. ✅ TOML for config

Focus implementation effort on these decisions, not reconsidering them.
