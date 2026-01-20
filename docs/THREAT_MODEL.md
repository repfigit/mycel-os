# Mycel OS Threat Model

This document describes the security boundaries, threat vectors, and mitigations for Mycel OS.

## Overview

Mycel OS is an AI-native operating system where users interact through natural language. The AI generates interfaces, writes code, and executes commands on behalf of the user. This creates unique security challenges that require defense-in-depth.

## Trust Boundaries

### 1. User ↔ Runtime
- **Boundary**: IPC socket (`/run/mycel/runtime.sock` or `/tmp/mycel-dev.sock`)
- **Trust Level**: Authenticated local user
- **Authentication**: Token-based (generated at runtime startup)
- **Protections**:
  - Socket permissions set to 0600 (owner only)
  - Rate limiting (100 requests/minute)
  - Message size limit (1MB)

### 2. Runtime ↔ Executor (Sandbox)
- **Boundary**: Process isolation via firejail/bubblewrap
- **Trust Level**: Untrusted (AI-generated code)
- **Protections**:
  - Network isolation (`--net=none`)
  - Filesystem isolation (`--private`, `--ro-bind`)
  - Memory limits (`--rlimit-as`)
  - Execution timeout (configurable, default 30s)
  - Pattern validation before execution

### 3. Runtime ↔ Cloud AI (Anthropic)
- **Boundary**: HTTPS to api.anthropic.com
- **Trust Level**: Trusted external service
- **Protections**:
  - TLS encryption
  - API key authentication
  - No PII sent to cloud (privacy layer)

### 4. Runtime ↔ Collective Network
- **Boundary**: HTTPS to NEAR RPC, Bittensor subnet
- **Trust Level**: Semi-trusted (blockchain consensus)
- **Protections**:
  - Cryptographic verification of patterns
  - Reputation system for pattern sources
  - Differential privacy for shared patterns

## Threat Vectors

### T1: Malicious User Input
**Description**: User provides input designed to make the AI execute harmful actions.

**Mitigations**:
- Policy layer evaluates actions before execution
- Dangerous patterns require explicit confirmation
- Blocked file patterns prevent access to sensitive paths
- Code execution sandboxed

### T2: AI-Generated Malicious Code
**Description**: AI generates code that escapes sandbox or causes harm.

**Mitigations**:
- Pattern validation blocks known dangerous patterns
- Bypass detection (obfuscation attempts blocked)
- Sandbox isolation (firejail/bubblewrap)
- No network access in sandbox
- Read-only filesystem bindings

### T3: IPC Socket Hijacking
**Description**: Attacker gains access to the IPC socket.

**Mitigations**:
- Socket permissions 0600
- Token authentication required
- Rate limiting prevents brute force
- Only runs on localhost

### T4: Session Hijacking
**Description**: Attacker takes over an existing session.

**Mitigations**:
- Session IDs are cryptographically random UUIDs
- Sessions expire after 24 hours of inactivity
- No session tokens stored on disk

### T5: Data Exfiltration via Collective
**Description**: Sensitive data leaked through pattern sharing.

**Mitigations**:
- Privacy layer removes PII before sharing
- Differential privacy adds noise to patterns
- User controls what gets shared
- Blocked categories configurable

### T6: Supply Chain Attack
**Description**: Malicious patterns injected into collective.

**Mitigations**:
- Pattern reputation system
- Cryptographic signatures on patterns
- Local validation before use
- User confirmation for high-risk patterns

### T7: Resource Exhaustion (DoS)
**Description**: Attacker exhausts system resources.

**Mitigations**:
- Execution timeout limits
- Memory limits on sandbox
- Rate limiting on IPC
- Message size limits

### T8: Code Injection via UI
**Description**: XSS attacks through generated UI surfaces.

**Mitigations**:
- Content Security Policy (CSP) headers
- HTML escaping for user content
- No inline script execution without CSP allowlist
- Frame ancestors restricted

## Security Assumptions

1. The host operating system is trusted
2. The Rust runtime is not compromised
3. The sandbox tool (firejail/bubblewrap) is correctly implemented
4. The user's home directory permissions are correct
5. Network traffic to external services is encrypted (HTTPS/TLS)

## Out of Scope

These threats are not addressed by Mycel OS:
- Physical access to the machine
- Kernel exploits
- Hardware attacks (rowhammer, spectre, etc.)
- Social engineering of the user
- Compromise of external services (Anthropic, NEAR, Bittensor)

## Security Contact

To report security vulnerabilities, see [SECURITY.md](../SECURITY.md).
