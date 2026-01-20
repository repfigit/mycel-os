# Security Policy

## Supported Versions

| Version | Supported          |
| ------- | ------------------ |
| 0.1.x   | :white_check_mark: |

## Reporting a Vulnerability

We take security seriously at Mycel OS. If you discover a security vulnerability, please report it responsibly.

### How to Report

1. **Do NOT** create a public GitHub issue for security vulnerabilities
2. Email security details to the maintainers (add contact email here when available)
3. Include:
   - Description of the vulnerability
   - Steps to reproduce
   - Potential impact
   - Suggested fix (if any)

### What to Expect

- **Acknowledgment**: Within 48 hours
- **Initial Assessment**: Within 7 days
- **Resolution Timeline**: Varies by severity
  - Critical: 24-72 hours
  - High: 1-2 weeks
  - Medium: 2-4 weeks
  - Low: Next release cycle

### Scope

Security issues in scope:
- Sandbox escape vulnerabilities
- Authentication/authorization bypasses
- Code injection vulnerabilities
- Information disclosure
- Denial of service

Out of scope:
- Social engineering attacks
- Physical access attacks
- Issues in third-party dependencies (report upstream)

## Security Hardening

Mycel OS implements multiple security layers:

### Executor Sandbox
- Sandboxed execution using firejail or bubblewrap
- Network isolation
- Filesystem isolation
- Memory and time limits
- Pattern validation

### IPC Security
- Socket permissions (0600)
- Token authentication
- Rate limiting
- Message size limits

### UI Security
- Content Security Policy headers
- HTML escaping
- Frame ancestors restrictions

### Code Validation
- Dangerous pattern detection
- Bypass attempt detection
- Language-specific validation

## Security Configuration

Key security settings in `config.toml`:

```toml
# Enable sandbox (required for security)
sandbox_enabled = true

# Execution limits
execution_timeout_secs = 30
execution_memory_mb = 512
```

**Warning**: Disabling the sandbox removes critical security protections and should only be done in trusted development environments.

## Threat Model

For a detailed threat model, see [docs/THREAT_MODEL.md](docs/THREAT_MODEL.md).
