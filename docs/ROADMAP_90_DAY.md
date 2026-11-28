# Mycel 90-Day Roadmap (Dev + GTM)

This roadmap captures the locked 90-day engineering and go-to-market plan for Mycel as a **distro-agnostic Linux AI agent governance & cost-routing sidecar**.

## 3-Layer Spend Governance Model

Mycel enforces deterministic cost control before any cloud or paid LLM request leaves the host:

1. **Per-Request Cap**: Hard maximum spend allowed on a single inference request; requests exceeding the cap are blocked or downgraded to a cheaper/local tier.
2. **Daily Soft & Hard Budget**:
   - **Soft Budget**: Emits structured warnings and prefers local/cheap tiers (`Local -> Cheap -> Frontier`).
   - **Hard Budget**: Blocks further paid cloud calls for the remainder of the UTC day unless explicitly overridden.
3. **Monthly Ceiling**: Absolute monthly spend limit across all configured providers; once reached, Mycel fails closed to local inference only.

## 90-Day Execution Gates

### Day 30 — Trust Boundary & Governance Hardening
- **Security & Isolation**: Bubblewrap (`bwrap`) sandbox enforcement, `0700` `$XDG_RUNTIME_DIR/mycel/runtime.sock` parent directory, explicit structured policy confirmations for dangerous actions, and removal of stale/duplicate tooling.
- **Quarantine**: Isolate experimental subsystems (`--features wine`, `--features collective`, compositor/shell) behind compile-time feature flags, off by default.
- **Spend Governance**: Ship 3-layer spend governance (per-request cap, daily soft/hard budget, monthly ceiling) with tiered `Local -> Cheap -> Frontier` routing.
- **CI Gate**: Enforce `cargo clippy --all-targets -- -D warnings`, `cargo clippy --all-features --all-targets -- -D warnings`, `cargo test --verbose`, and `cargo test --all-features --verbose`.

### Day 60 — Distro-Agnostic Packaging & Developer Adoption
- **Packaging**: Ship reproducible Linux packages/binaries (musl static binary, `.deb`, `.rpm`, Arch/Void packages) for the `mycel-runtime` sidecar.
- **Provider & Routing DX**: Streamlined setup for 45+ LLM providers, Venice.ai, and native `llama-cpp` / Ollama local backends with clear cost-attribution telemetry.
- **Sync**: Transport-agnostic personal device sync (Tailscale/LAN direct TCP default; opt-in kernel WireGuard + STUN + zero-knowledge relay).
- **GTM**: Developer preview launch targeting Linux AI agent builders needing local-first cost routing and `bwrap` execution governance.

### Day 90 — Production Sidecar & Reference Appliance
- **Production Readiness**: Stable IPC protocol, audited threat model, and documented operator runbooks for teams running `mycel-runtime` as a systemd/runit user service.
- **Reference Appliance**: Optional offline-capable Void Linux (musl) ISO reference appliance maintained as a showcase, not a prerequisite.
- **GTM**: General availability of Mycel v1.0 sidecar with team-ready spend governance policies and verified multi-distro install paths.
