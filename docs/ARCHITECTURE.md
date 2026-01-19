# Clay OS - Architecture Document

## Vision

Clay OS reimagines the operating system as a fluid, AI-driven experience. Instead of fixed applications with rigid interfaces, users interact with a local AI that dynamically generates interfaces, writes programs on-the-fly, and seamlessly escalates to cloud AI when deeper reasoning is needed.

The metaphor is **clay, not windows** - you shape your computing experience through conversation and intent, not by navigating menus and clicking buttons.

## Core Principles

1. **Intent-driven interaction** - Users express what they want, not how to do it
2. **Generative UI** - Interfaces are created on-demand, tailored to the task
3. **Hybrid intelligence** - Local LLM for speed, cloud LLM for depth
4. **Code as ephemera** - Programs are generated, used, and discarded (or saved if valuable)
5. **Context is king** - The OS maintains rich context about user, tasks, and history

## System Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                        USER LAYER                                │
│  ┌─────────────────────────────────────────────────────────┐    │
│  │              Fluid Canvas (Wayland Compositor)           │    │
│  │   - Dynamic UI rendering (HTML/Canvas/Native)            │    │
│  │   - Voice input/output                                   │    │
│  │   - Gesture recognition                                  │    │
│  └─────────────────────────────────────────────────────────┘    │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│                     CLAY RUNTIME LAYER                          │
│  ┌──────────────┐  ┌──────────────┐  ┌───────────────────────┐ │
│  │   Intent     │  │   Context    │  │    Code Generator     │ │
│  │   Parser     │  │   Manager    │  │    & Executor         │ │
│  └──────────────┘  └──────────────┘  └───────────────────────┘ │
│  ┌──────────────┐  ┌──────────────┐  ┌───────────────────────┐ │
│  │  UI Factory  │  │  Tool/API    │  │   Memory & Learning   │ │
│  │  (Gen UI)    │  │  Bridge      │  │   Subsystem           │ │
│  └──────────────┘  └──────────────┘  └───────────────────────┘ │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│                    INTELLIGENCE LAYER                           │
│  ┌─────────────────────────┐    ┌─────────────────────────────┐│
│  │      Local LLM          │◄──►│      Cloud Router           ││
│  │  (llama.cpp/Ollama)     │    │  (Anthropic/OpenAI/etc)     ││
│  │  - Phi-3 / Mistral 7B   │    │  - Complex reasoning        ││
│  │  - Fast responses       │    │  - Large context tasks      ││
│  │  - Privacy-first        │    │  - Specialized knowledge    ││
│  └─────────────────────────┘    └─────────────────────────────┘│
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│                      BASE OS LAYER                              │
│  ┌──────────────┐  ┌──────────────┐  ┌───────────────────────┐ │
│  │ Linux Kernel │  │  Filesystem  │  │   Hardware Abstraction│ │
│  │  (minimal)   │  │  (AI-aware)  │  │   (GPU/NPU for LLM)   │ │
│  └──────────────┘  └──────────────┘  └───────────────────────┘ │
└─────────────────────────────────────────────────────────────────┘
```

## Component Deep Dives

### 1. Fluid Canvas

The display layer abandons traditional window management for a **fluid canvas**:

- Based on a minimal Wayland compositor (wlroots)
- Renders "surfaces" that the AI generates dynamically
- Surfaces can be:
  - Web views (Chromium Embedded Framework or WebKitGTK)
  - Native widgets (GTK4/Qt for performance-critical UI)
  - Direct canvas drawing (for custom visualizations)
- Supports morphing, merging, and transforming surfaces
- No fixed window chrome - context determines presentation

### 2. Intent Parser

Converts user input (text, voice, gesture) into structured intents:

```json
{
  "intent": "create_document",
  "parameters": {
    "type": "letter",
    "recipient": "constituent",
    "topic": "blockchain legislation update"
  },
  "context": {
    "recent_files": ["HB302_draft.md"],
    "time_pressure": "low"
  }
}
```

### 3. Context Manager

Maintains rich state across interactions:

- **Session context** - Current task, open surfaces, conversation history
- **User context** - Preferences, expertise level, common patterns
- **System context** - Available resources, connected services, file system state
- **World context** - Time, location, calendar, external events

### 4. Code Generator & Executor

The AI writes and runs code in real-time:

- **Sandboxed execution** via gVisor or Firecracker microVMs
- **Language support**: Python, JavaScript, Rust (compiled on-demand)
- **Capability-based security** - Generated code requests permissions
- **Hot-reloading** - Modify running programs without restart

### 5. UI Factory

Generates interfaces on-demand:

```
User: "I need to compare these three bills side by side"

UI Factory generates:
- Three-column layout
- Synchronized scrolling
- Diff highlighting
- Annotation tools
- Export options
```

### 6. Intelligence Router

Decides when to use local vs cloud AI:

| Scenario | Route | Reason |
|----------|-------|--------|
| "What time is it?" | Local | Simple, fast |
| "Summarize this email" | Local | Privacy, speed |
| "Analyze implications of HB302 for DeFi" | Cloud | Complex reasoning |
| "Write a speech about..." | Hybrid | Local draft, cloud polish |
| "Debug this code" | Local first, escalate | Try fast, get help if stuck |

## File System Design

### AI-Aware File System (ClayFS)

Traditional hierarchical filesystems don't fit the fluid paradigm. ClayFS adds:

- **Semantic tagging** - Files have AI-generated metadata
- **Intent-based retrieval** - "Find that bill I was working on Tuesday"
- **Automatic versioning** - Every meaningful change tracked
- **Relationship mapping** - Files know their connections to other files
- **Ephemeral space** - For generated content that may not persist

```
/clay
  /persistent      # User's permanent files
  /ephemeral       # Generated content, auto-cleaned
  /context         # AI context and memory
  /generated       # AI-written programs
  /cache           # Cloud response cache
```

## Security Model

### Capability-Based Security

Generated code runs with minimal permissions by default:

```python
# AI-generated program requests capabilities
@requires(["read:~/documents", "network:api.weather.com"])
def show_weather_with_calendar():
    ...
```

### Trust Levels

1. **Core OS** - Full trust (kernel, Clay Runtime)
2. **User-approved** - Installed/verified programs
3. **AI-generated** - Sandboxed, limited capabilities
4. **External** - Maximum isolation

### Cloud Communication

- All cloud requests go through local proxy
- User can inspect/approve sensitive data leaving device
- Local model can redact before sending to cloud
- Response caching to minimize data exposure

## Boot Sequence

1. Kernel loads (minimal Linux)
2. Init system starts Clay Runtime
3. Local LLM loads into memory (GPU if available)
4. Fluid Canvas initializes
5. Context Manager loads user profile
6. System presents itself: "Good morning. What would you like to work on?"

## Development Phases

### Phase 1: Foundation (Current)
- [ ] Fork base OS (Void Linux)
- [ ] Integrate llama.cpp with small model
- [ ] Basic CLI interface to AI
- [ ] Simple code generation and execution

### Phase 2: Runtime
- [ ] Build Clay Runtime daemon
- [ ] Implement Context Manager
- [ ] Create sandboxed code executor
- [ ] Add cloud API integration

### Phase 3: Visual Layer
- [ ] Minimal Wayland compositor
- [ ] Web-based UI generation
- [ ] Basic surface management
- [ ] Voice input/output

### Phase 4: Intelligence
- [ ] Intent parsing improvements
- [ ] Smart local/cloud routing
- [ ] Learning from user patterns
- [ ] Memory and recall system

### Phase 5: Polish
- [ ] ClayFS implementation
- [ ] Security hardening
- [ ] Performance optimization
- [ ] Documentation and tools

## Technology Choices

| Component | Choice | Rationale |
|-----------|--------|-----------|
| Base OS | Void Linux | Minimal, independent, simple init |
| Local LLM | llama.cpp | Best performance, active development |
| Default Model | Phi-3 Medium (14B) or Mistral 7B | Good balance of capability/speed |
| Cloud API | Anthropic Claude | Best reasoning, tool use |
| Compositor | wlroots | Modern, minimal, hackable |
| UI Runtime | WebKitGTK + Custom | Flexible, familiar tech |
| Sandbox | gVisor | Strong isolation, Linux compatible |
| Language | Rust + Python | Rust for core, Python for flexibility |

## Open Questions

1. **How much should the AI remember?** Balance utility vs privacy
2. **What's the right model size for local?** Hardware dependent
3. **How to handle offline mode?** Graceful degradation
4. **Multi-user support?** Separate contexts, shared hardware
5. **Update mechanism?** Traditional packages vs AI-managed

---

*This is a living document. Last updated: January 2026*
