# Quality Gates

This document defines the quality gates that must pass before code can be merged to main or released.

## Definition of Done

A feature or fix is "done" when:

1. **Code Complete**: All acceptance criteria met
2. **Tests Pass**: Unit, integration, and smoke tests pass
3. **Security Reviewed**: No new vulnerabilities introduced
4. **Documentation Updated**: CLAUDE.md, API docs, or user docs updated if needed
5. **Linting Passes**: No clippy warnings, code formatted

## CI Pipeline Gates

### On Every Pull Request

| Gate | Tool | Requirement |
|------|------|-------------|
| Format | `cargo fmt --check` | No formatting issues |
| Lint | `cargo clippy -- -D warnings` | No warnings |
| Build | `cargo build` | Compiles successfully |
| Unit Tests | `cargo test` | All tests pass |
| Security Audit | `cargo audit` | No known vulnerabilities |

### On Merge to Main

All PR gates plus:

| Gate | Tool | Requirement |
|------|------|-------------|
| Release Build | `cargo build --release` | Optimized build succeeds |
| Binary Size | Manual check | Reasonable size (< 50MB) |

### On Release Tag

All main gates plus:

| Gate | Requirement |
|------|-------------|
| Version Bump | Cargo.toml version updated |
| Changelog | CHANGELOG.md updated |
| ISO Build | Bootable ISO builds successfully |

## Security Gates

### Code Execution Security

- [ ] All AI-generated code runs in sandbox
- [ ] Pattern validation blocks known dangerous patterns
- [ ] Bypass attempts detected and blocked
- [ ] Timeout and memory limits enforced
- [ ] Shell execution always requires sandbox

### IPC Security

- [ ] Socket permissions set to 0600
- [ ] Authentication token required for most operations
- [ ] Rate limiting enforced (100 req/min)
- [ ] Message size limit enforced (1MB)

### UI Security

- [ ] CSP headers present on all HTML surfaces
- [ ] User content HTML-escaped
- [ ] No inline scripts without CSP allowlist

## Test Coverage Requirements

### Minimum Coverage Targets

| Module | Minimum Coverage |
|--------|------------------|
| executor | 80% |
| ipc | 70% |
| policy | 90% |
| config | 70% |
| ai | 60% |

### Required Test Types

1. **Unit Tests**: All public functions have unit tests
2. **Integration Tests**: IPC round-trip, executor sandbox
3. **Security Tests**: Sandbox escape attempts, pattern bypass attempts
4. **Smoke Test**: `cargo run -- --dev` starts successfully

## Performance Gates

| Metric | Target |
|--------|--------|
| Startup time | < 2 seconds |
| Simple response | < 500ms (local LLM) |
| Memory at idle | < 100MB |
| IPC latency | < 10ms |

## Manual Review Checklist

Before approving a PR:

- [ ] Code is readable and follows Rust idioms
- [ ] No hardcoded secrets or credentials
- [ ] Error handling is appropriate
- [ ] Logging is at appropriate levels
- [ ] No unnecessary dependencies added
- [ ] Changes don't break existing functionality
- [ ] Security implications considered

## Breaking Change Policy

Breaking changes require:

1. Major version bump
2. Migration guide in docs
3. Deprecation warning in previous release (if applicable)
4. Team review approval

## Hotfix Process

For critical security issues:

1. Create fix on `hotfix/*` branch
2. Run full CI pipeline
3. Security team review
4. Merge to main and create patch release
5. Deploy advisory to affected users

## Measuring Quality

### Metrics to Track

- Test pass rate over time
- Security audit findings
- Build success rate
- Time to fix critical bugs
- Code coverage trends

### Regular Reviews

- **Weekly**: CI failure analysis
- **Monthly**: Security audit review
- **Quarterly**: Quality metrics review, coverage targets adjustment
