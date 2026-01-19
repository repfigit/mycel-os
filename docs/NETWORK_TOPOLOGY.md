# Collective Intelligence Network Topology

This document provides visual representations of how Mycel OS instances
form a collective intelligence network using NEAR Protocol and Bittensor.

## Network Overview

```
                              ┌─────────────────────────────────────┐
                              │         BITTENSOR NETWORK           │
                              │  ┌─────────────────────────────┐    │
                              │  │      Clay Subnet (SN32)     │    │
                              │  │                             │    │
                              │  │  Miners: Pattern evaluation │    │
                              │  │  Validators: Quality check  │    │
                              │  │  Consensus: Yuma            │    │
                              │  │  Rewards: TAO tokens        │    │
                              │  └─────────────────────────────┘    │
                              └───────────────┬─────────────────────┘
                                              │
                    ┌─────────────────────────┼─────────────────────────┐
                    │                         │                         │
                    ▼                         ▼                         ▼
    ┌───────────────────────┐ ┌───────────────────────┐ ┌───────────────────────┐
    │     CLAY INSTANCE A   │ │     CLAY INSTANCE B   │ │     CLAY INSTANCE C   │
    │                       │ │                       │ │                       │
    │  ┌─────────────────┐  │ │  ┌─────────────────┐  │ │  ┌─────────────────┐  │
    │  │   Local LLM     │  │ │  │   Local LLM     │  │ │  │   Local LLM     │  │
    │  │   (Phi-3)       │  │ │  │   (Mistral)     │  │ │  │   (Llama)       │  │
    │  └────────┬────────┘  │ │  └────────┬────────┘  │ │  └────────┬────────┘  │
    │           │           │ │           │           │ │           │           │
    │  ┌────────▼────────┐  │ │  ┌────────▼────────┐  │ │  ┌────────▼────────┐  │
    │  │ Pattern Store   │  │ │  │ Pattern Store   │  │ │  │ Pattern Store   │  │
    │  │ - Local: 150    │  │ │  │ - Local: 89     │  │ │  │ - Local: 230    │  │
    │  │ - Network: 45   │  │ │  │ - Network: 120  │  │ │  │ - Network: 67   │  │
    │  └────────┬────────┘  │ │  └────────┬────────┘  │ │  └────────┬────────┘  │
    │           │           │ │           │           │ │           │           │
    │  ┌────────▼────────┐  │ │  ┌────────▼────────┐  │ │  ┌────────▼────────┐  │
    │  │ NEAR Wallet     │  │ │  │ NEAR Wallet     │  │ │  │ NEAR Wallet     │  │
    │  │ alice.clay.near │  │ │  │ bob.clay.near   │  │ │  │ carol.clay.near │  │
    │  │ Balance: 12 NEAR│  │ │  │ Balance: 45 NEAR│  │ │  │ Balance: 8 NEAR │  │
    │  └─────────────────┘  │ │  └─────────────────┘  │ │  └─────────────────┘  │
    └───────────┬───────────┘ └───────────┬───────────┘ └───────────┬───────────┘
                │                         │                         │
                └─────────────────────────┼─────────────────────────┘
                                          │
                              ┌───────────▼───────────────────────┐
                              │         NEAR PROTOCOL             │
                              │  ┌─────────────────────────────┐  │
                              │  │    Pattern Registry         │  │
                              │  │    - Total patterns: 12,450 │  │
                              │  │    - Daily transactions: 8k │  │
                              │  └─────────────────────────────┘  │
                              │  ┌─────────────────────────────┐  │
                              │  │    Reputation System        │  │
                              │  │    - Active creators: 2,100 │  │
                              │  │    - Avg reputation: 0.73   │  │
                              │  └─────────────────────────────┘  │
                              └───────────────────────────────────┘
```

## Pattern Lifecycle

```
┌──────────────────────────────────────────────────────────────────────────────┐
│                           PATTERN LIFECYCLE                                   │
└──────────────────────────────────────────────────────────────────────────────┘

1. CREATION                    2. EXTRACTION                3. SHARING
┌─────────────────┐           ┌─────────────────┐         ┌─────────────────┐
│ User interacts  │           │ Extract insight │         │ Register on     │
│ with Clay       │           │ from successful │         │ NEAR registry   │
│                 │    ──►    │ interaction     │   ──►   │                 │
│ "Find duplicates│           │                 │         │ Pattern hash    │
│ in my photos"   │           │ Sanitize PII    │         │ stored on-chain │
│                 │           │ Add DP noise    │         │                 │
└─────────────────┘           └─────────────────┘         └─────────────────┘
                                                                   │
                                                                   ▼
6. IMPROVEMENT                 5. REWARDS                  4. EVALUATION
┌─────────────────┐           ┌─────────────────┐         ┌─────────────────┐
│ Federated       │           │ Creator earns   │         │ Bittensor miners│
│ learning from   │    ◄──    │ NEAR per use    │   ◄──   │ evaluate pattern│
│ usage patterns  │           │ TAO for quality │         │ quality         │
│                 │           │                 │         │                 │
│ Model gets      │           │ Reputation      │         │ Score: 0.85     │
│ collectively    │           │ increases       │         │ Relevance: High │
│ smarter         │           │                 │         │ Safety: OK      │
└─────────────────┘           └─────────────────┘         └─────────────────┘
```

## Token Flow

```
┌──────────────────────────────────────────────────────────────────────────────┐
│                              TOKEN ECONOMICS                                  │
└──────────────────────────────────────────────────────────────────────────────┘

                         ┌────────────────────┐
                         │   Pattern Creator  │
                         │   (alice.clay.near)│
                         └─────────┬──────────┘
                                   │
                    Creates pattern│"Code review template"
                                   │
                                   ▼
                    ┌──────────────────────────┐
                    │    NEAR Pattern Registry │
                    │    patterns.clay.near    │
                    └──────────────┬───────────┘
                                   │
                    ┌──────────────┴───────────┐
                    │                          │
                    ▼                          ▼
          ┌──────────────────┐      ┌──────────────────┐
          │   User B uses    │      │   User C uses    │
          │   pattern        │      │   pattern        │
          │                  │      │                  │
          │   Pays: 0.01 NEAR│      │   Pays: 0.01 NEAR│
          └────────┬─────────┘      └────────┬─────────┘
                   │                         │
                   └────────────┬────────────┘
                                │
                                ▼
                    ┌──────────────────────────┐
                    │     Payment Split        │
                    │                          │
                    │  Creator: 0.019 NEAR     │
                    │  Protocol: 0.001 NEAR    │
                    └──────────────────────────┘
                                │
                    ┌───────────┴───────────┐
                    │                       │
                    ▼                       ▼
          ┌──────────────────┐   ┌──────────────────┐
          │  alice.clay.near │   │ Protocol Treasury│
          │  +0.019 NEAR     │   │ +0.001 NEAR      │
          └──────────────────┘   └──────────────────┘


         ┌─────────────────────────────────────────────┐
         │             BITTENSOR REWARDS               │
         └─────────────────────────────────────────────┘

    ┌────────────────┐                    ┌────────────────┐
    │  Miner Node    │                    │  Validator     │
    │                │                    │                │
    │  Evaluates     │  ◄── Stakes ──►    │  Verifies      │
    │  patterns      │      TAO           │  evaluations   │
    │                │                    │                │
    │  Earns TAO for │                    │  Earns TAO for │
    │  accurate      │                    │  accurate      │
    │  scoring       │                    │  validation    │
    └────────────────┘                    └────────────────┘
              │                                   │
              └─────────────┬─────────────────────┘
                            │
                            ▼
              ┌──────────────────────────┐
              │   Yuma Consensus         │
              │                          │
              │   Determines reward      │
              │   distribution based on  │
              │   contribution quality   │
              └──────────────────────────┘
```

## Discovery Flow

```
┌──────────────────────────────────────────────────────────────────────────────┐
│                         PATTERN DISCOVERY FLOW                                │
└──────────────────────────────────────────────────────────────────────────────┘

User Request: "Help me parse this legal document"
                            │
                            ▼
              ┌──────────────────────────┐
              │   Intent Parser          │
              │   Domain: legal          │
              │   Action: parse/analyze  │
              └──────────────┬───────────┘
                             │
            ┌────────────────┼────────────────┐
            │                │                │
            ▼                ▼                ▼
   ┌─────────────┐  ┌─────────────┐  ┌─────────────┐
   │ LOCAL STORE │  │ NEAR QUERY  │  │ BITTENSOR   │
   │             │  │             │  │ SEMANTIC    │
   │ domain:legal│  │ domain:legal│  │             │
   │ limit: 10   │  │ min_rep: 0.6│  │ embedding   │
   │             │  │ limit: 20   │  │ search k=10 │
   │ Found: 3    │  │ Found: 12   │  │ Found: 8    │
   │ Time: 1ms   │  │ Time: 150ms │  │ Time: 200ms │
   └──────┬──────┘  └──────┬──────┘  └──────┬──────┘
          │                │                │
          └────────────────┼────────────────┘
                           │
                           ▼
              ┌──────────────────────────┐
              │   Merge & Deduplicate    │
              │   23 patterns → 18 unique│
              └──────────────┬───────────┘
                             │
                             ▼
              ┌──────────────────────────┐
              │   Ranking Algorithm      │
              │                          │
              │   Factors:               │
              │   - Relevance: 40%       │
              │   - Quality: 30%         │
              │   - Source: 15%          │
              │   - Success rate: 15%    │
              └──────────────┬───────────┘
                             │
                             ▼
              ┌──────────────────────────┐
              │   Top 5 Results          │
              │                          │
              │   1. legal_doc_parser    │
              │      Score: 0.92         │
              │      Source: Local       │
              │      Price: Free         │
              │                          │
              │   2. contract_analyzer   │
              │      Score: 0.87         │
              │      Source: NEAR        │
              │      Price: 0.005 NEAR   │
              │                          │
              │   3. clause_extractor    │
              │      Score: 0.84         │
              │      Source: Bittensor   │
              │      Price: N/A          │
              │   ...                    │
              └──────────────────────────┘
```

## Privacy Layer

```
┌──────────────────────────────────────────────────────────────────────────────┐
│                           PRIVACY ARCHITECTURE                                │
└──────────────────────────────────────────────────────────────────────────────┘

Private Interaction:
┌────────────────────────────────────────────────────────────────────────────┐
│ User: "Find all invoices from Acme Corp over $10,000 in /home/john/docs"  │
│ Response: [specific code with file paths and company names]                │
└────────────────────────────────────────────────────────────────────────────┘
                                    │
                                    ▼
                    ┌───────────────────────────┐
                    │    PRIVACY PIPELINE       │
                    │                           │
                    │  1. PII Detection         │
                    │     - "Acme Corp" → FOUND │
                    │     - "/home/john" → FOUND│
                    │     - "$10,000" → FOUND   │
                    │                           │
                    │  2. Sanitization          │
                    │     - "Acme Corp" → [COMPANY]
                    │     - "/home/john" → [PATH]
                    │     - "$10,000" → [AMOUNT]│
                    │                           │
                    │  3. Generalization        │
                    │     - Specific → General  │
                    │     - "invoice" → "document"
                    │                           │
                    │  4. Differential Privacy  │
                    │     - ε = 1.0, δ = 1e-5  │
                    │     - Add calibrated noise│
                    │                           │
                    │  5. Utility Check         │
                    │     - Score: 0.72         │
                    │     - Threshold: 0.5 ✓    │
                    └───────────────┬───────────┘
                                    │
                                    ▼
Shareable Pattern:
┌────────────────────────────────────────────────────────────────────────────┐
│ Trigger: "Find documents matching [CRITERIA] with [FILTER] in [PATH]"     │
│ Solution: Generic code template for document filtering                     │
│ Domain: "document_search"                                                  │
└────────────────────────────────────────────────────────────────────────────┘
```

## Federated Learning

```
┌──────────────────────────────────────────────────────────────────────────────┐
│                        FEDERATED LEARNING FLOW                                │
└──────────────────────────────────────────────────────────────────────────────┘

    Instance A              Instance B              Instance C
    ┌────────┐              ┌────────┐              ┌────────┐
    │Private │              │Private │              │Private │
    │Data    │              │Data    │              │Data    │
    └───┬────┘              └───┬────┘              └───┬────┘
        │                       │                       │
        ▼                       ▼                       ▼
    ┌────────┐              ┌────────┐              ┌────────┐
    │Local   │              │Local   │              │Local   │
    │Training│              │Training│              │Training│
    └───┬────┘              └───┬────┘              └───┬────┘
        │                       │                       │
        ▼                       ▼                       ▼
    ┌────────┐              ┌────────┐              ┌────────┐
    │Compute │              │Compute │              │Compute │
    │Gradients│             │Gradients│             │Gradients│
    └───┬────┘              └───┬────┘              └───┬────┘
        │                       │                       │
        ▼                       ▼                       ▼
    ┌────────┐              ┌────────┐              ┌────────┐
    │Add DP  │              │Add DP  │              │Add DP  │
    │Noise   │              │Noise   │              │Noise   │
    │ε=1.0   │              │ε=1.0   │              │ε=1.0   │
    └───┬────┘              └───┬────┘              └───┬────┘
        │                       │                       │
        └───────────────────────┼───────────────────────┘
                                │
                                ▼
                    ┌───────────────────────┐
                    │   BITTENSOR SUBNET    │
                    │                       │
                    │   Aggregate Gradients │
                    │   Secure Averaging    │
                    │   (No single instance │
                    │    sees raw data)     │
                    │                       │
                    └───────────┬───────────┘
                                │
                                ▼
                    ┌───────────────────────┐
                    │   Updated Global      │
                    │   Model Weights       │
                    │                       │
                    │   clay-base-v1.2.3    │
                    └───────────┬───────────┘
                                │
        ┌───────────────────────┼───────────────────────┐
        │                       │                       │
        ▼                       ▼                       ▼
    ┌────────┐              ┌────────┐              ┌────────┐
    │Download│              │Download│              │Download│
    │Updated │              │Updated │              │Updated │
    │Model   │              │Model   │              │Model   │
    └────────┘              └────────┘              └────────┘

    Instance A              Instance B              Instance C
    
    All instances benefit from collective learning
    without any single instance exposing private data
```

## Governance Model

```
┌──────────────────────────────────────────────────────────────────────────────┐
│                          GOVERNANCE STRUCTURE                                 │
└──────────────────────────────────────────────────────────────────────────────┘

                    ┌───────────────────────────┐
                    │      CLAY DAO             │
                    │                           │
                    │  Token: CLAY (on NEAR)    │
                    │  Voting: Quadratic        │
                    │  Quorum: 10% of supply    │
                    └───────────────┬───────────┘
                                    │
            ┌───────────────────────┼───────────────────────┐
            │                       │                       │
            ▼                       ▼                       ▼
    ┌───────────────┐       ┌───────────────┐       ┌───────────────┐
    │  Protocol     │       │  Economics    │       │  Technical    │
    │  Committee    │       │  Committee    │       │  Committee    │
    │               │       │               │       │               │
    │  - Privacy    │       │  - Fee rates  │       │  - Upgrades   │
    │    settings   │       │  - Rewards    │       │  - Standards  │
    │  - Safety     │       │  - Stakes     │       │  - Integrations│
    │    policies   │       │  - Burns      │       │               │
    └───────────────┘       └───────────────┘       └───────────────┘

                    Proposal Flow:
                    
    Community Member                    DAO
    ┌────────────┐                ┌────────────┐
    │  Submit    │  ────────►     │  Review    │
    │  Proposal  │                │  Period    │
    └────────────┘                │  (7 days)  │
                                  └─────┬──────┘
                                        │
                                        ▼
                                  ┌────────────┐
                                  │  Voting    │
                                  │  Period    │
                                  │  (14 days) │
                                  └─────┬──────┘
                                        │
                          ┌─────────────┴─────────────┐
                          │                           │
                          ▼                           ▼
                    ┌──────────┐                ┌──────────┐
                    │ Approved │                │ Rejected │
                    │          │                │          │
                    │ Execute  │                │ Archive  │
                    │ on-chain │                │          │
                    └──────────┘                └──────────┘
```
