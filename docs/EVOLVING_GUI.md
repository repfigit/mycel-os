# Evolving GUI Architecture

## Overview

The Mycel OS graphical interface is not designed - it **emerges**. Starting from a minimal conversational interface, the GUI evolves based on:

1. **Personal usage patterns** - What you do frequently becomes easier to access
2. **Collective intelligence** - UI patterns that work well for others spread to your instance
3. **Context awareness** - The interface adapts to your current task and time of day
4. **AI synthesis** - The local/cloud AI generates new UI elements on demand

This document details the technical architecture that makes this possible.

---

## The Three Evolutionary Pressures

### 1. Personal Learning

```
┌─────────────────────────────────────────────────────────────────┐
│                    PERSONAL LEARNING LOOP                        │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│   User Action                                                    │
│       │                                                          │
│       ▼                                                          │
│   ┌─────────────────────┐                                       │
│   │  Interaction Logger │  Records:                             │
│   │                     │  - Clicks and targets                 │
│   │                     │  - Dwell time on surfaces             │
│   │                     │  - Scroll patterns                    │
│   │                     │  - Dismissed suggestions              │
│   │                     │  - Task completion paths              │
│   └──────────┬──────────┘                                       │
│              │                                                   │
│              ▼                                                   │
│   ┌─────────────────────┐                                       │
│   │  Pattern Extractor  │  Identifies:                          │
│   │                     │  - Frequent action sequences          │
│   │                     │  - Time-of-day preferences            │
│   │                     │  - Surface arrangements used          │
│   │                     │  - Shortcuts discovered               │
│   └──────────┬──────────┘                                       │
│              │                                                   │
│              ▼                                                   │
│   ┌─────────────────────┐                                       │
│   │  Personal Model     │  Updates:                             │
│   │                     │  - Layout preferences                 │
│   │                     │  - Feature importance                 │
│   │                     │  - Accessibility needs                │
│   │                     │  - Workflow optimizations             │
│   └──────────┬──────────┘                                       │
│              │                                                   │
│              ▼                                                   │
│   ┌─────────────────────┐                                       │
│   │  UI Adaptation      │  Changes:                             │
│   │                     │  - Surface positions                  │
│   │                     │  - Quick action suggestions           │
│   │                     │  - Default layouts                    │
│   │                     │  - Animation timing                   │
│   └─────────────────────┘                                       │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

### 2. Collective Intelligence

UI patterns that prove effective spread across the network:

```rust
/// A shareable UI pattern
#[derive(Serialize, Deserialize)]
pub struct UiPattern {
    /// Unique identifier
    pub id: PatternId,
    
    /// What context triggers this pattern
    pub trigger: UiTrigger,
    
    /// The layout specification
    pub layout: LayoutSpec,
    
    /// Effectiveness metrics
    pub metrics: UiMetrics,
    
    /// Privacy level (what data is needed)
    pub privacy_requirements: PrivacyLevel,
}

#[derive(Serialize, Deserialize)]
pub struct UiTrigger {
    /// Domain (coding, writing, analysis, etc.)
    pub domain: Option<String>,
    
    /// Time of day preference
    pub time_range: Option<TimeRange>,
    
    /// Required context elements
    pub context_requirements: Vec<String>,
    
    /// User expertise level
    pub expertise_level: Option<ExpertiseLevel>,
}

#[derive(Serialize, Deserialize)]
pub struct UiMetrics {
    /// Task completion rate with this layout
    pub completion_rate: f32,
    
    /// Average time to complete tasks
    pub avg_completion_time_secs: f32,
    
    /// User satisfaction (from feedback)
    pub satisfaction_score: f32,
    
    /// Number of instances using this pattern
    pub adoption_count: u64,
}
```

**Discovery and Adoption Flow:**

```
Local Instance                    Network                      Other Instances
      │                              │                              │
      │  User completes task         │                              │
      │  unusually fast              │                              │
      │         │                    │                              │
      │         ▼                    │                              │
      │  ┌─────────────┐             │                              │
      │  │ Detect good │             │                              │
      │  │ UI pattern  │             │                              │
      │  └──────┬──────┘             │                              │
      │         │                    │                              │
      │         │  Share pattern     │                              │
      │         ├───────────────────►│                              │
      │         │                    │                              │
      │         │                    │  Evaluate via Bittensor      │
      │         │                    │         │                    │
      │         │                    │         ▼                    │
      │         │                    │  ┌─────────────┐             │
      │         │                    │  │ Miners test │             │
      │         │                    │  │ pattern     │             │
      │         │                    │  └──────┬──────┘             │
      │         │                    │         │                    │
      │         │                    │         │ Score: 0.85        │
      │         │                    │         │                    │
      │         │                    │  Store in NEAR registry      │
      │         │                    │         │                    │
      │         │                    │         ├──────────────────►│
      │         │                    │         │                   │
      │         │                    │         │   Pattern shows up │
      │         │                    │         │   in discovery     │
      │         │                    │         │         │          │
      │         │                    │         │         ▼          │
      │         │                    │         │  ┌─────────────┐   │
      │         │                    │         │  │ User opts   │   │
      │         │                    │         │  │ to try it   │   │
      │         │                    │         │  └──────┬──────┘   │
      │         │                    │         │         │          │
      │  Creator earns tokens        │         │         │          │
      │◄─────────────────────────────┤◄────────┴─────────┘          │
      │                              │                              │
```

### 3. Context-Aware Synthesis

The AI generates appropriate UI for the moment:

```rust
/// Generate UI based on current context
pub async fn synthesize_ui(
    context: &UserContext,
    personal_model: &PersonalModel,
    collective_patterns: &[UiPattern],
) -> SynthesizedLayout {
    // 1. Analyze current context
    let analysis = analyze_context(context);
    
    // 2. Find matching patterns
    let candidates = collective_patterns
        .iter()
        .filter(|p| p.trigger.matches(&analysis))
        .collect::<Vec<_>>();
    
    // 3. Score patterns against personal preferences
    let scored: Vec<(f32, &UiPattern)> = candidates
        .iter()
        .map(|p| (personal_model.score_pattern(p), *p))
        .collect();
    
    // 4. If good match exists, use it
    if let Some((score, pattern)) = scored.iter().max_by(|a, b| a.0.partial_cmp(&b.0).unwrap()) {
        if *score > 0.7 {
            return pattern.layout.instantiate(context);
        }
    }
    
    // 5. Otherwise, ask AI to generate new layout
    let prompt = format!(r#"
        Generate a UI layout for the following context:
        
        Task: {}
        Domain: {}
        Time: {}
        Recent activity: {:?}
        User expertise: {}
        
        Available surface types:
        - conversation: Primary chat interface
        - code_editor: Code editing with syntax highlighting
        - terminal: Command line
        - web_view: Embedded browser
        - file_browser: File navigation
        - preview: Document/image preview
        
        Return a JSON layout specification.
    "#,
        analysis.inferred_task,
        analysis.domain,
        analysis.time_of_day,
        analysis.recent_files,
        personal_model.expertise_level,
    );
    
    let response = ai_router.generate(&prompt).await?;
    let layout: LayoutSpec = serde_json::from_str(&response)?;
    
    layout.instantiate(context)
}
```

---

## Layout System

### Layout Specification

```rust
/// A complete layout specification
#[derive(Serialize, Deserialize, Clone)]
pub struct LayoutSpec {
    /// Layout type
    pub layout_type: LayoutType,
    
    /// Surfaces in this layout
    pub surfaces: Vec<SurfaceSpec>,
    
    /// Constraints and relationships
    pub constraints: Vec<LayoutConstraint>,
    
    /// Animations for transitions
    pub transitions: TransitionSpec,
}

#[derive(Serialize, Deserialize, Clone)]
pub enum LayoutType {
    /// Single surface fills screen
    Single,
    
    /// Two surfaces side by side
    Split { ratio: f32, direction: Direction },
    
    /// Multiple columns
    Columns { ratios: Vec<f32> },
    
    /// Multiple rows
    Rows { ratios: Vec<f32> },
    
    /// Free-form with absolute positioning
    Canvas,
    
    /// Tabbed interface
    Tabs,
    
    /// Stacked with one visible
    Stack,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct SurfaceSpec {
    /// Surface identifier
    pub id: String,
    
    /// Type of surface
    pub surface_type: SurfaceType,
    
    /// Position in layout (depends on layout type)
    pub position: Position,
    
    /// Initial size (can be flexible)
    pub size: Size,
    
    /// Content source
    pub content: ContentSpec,
    
    /// Can user resize?
    pub resizable: bool,
    
    /// Can user move?
    pub movable: bool,
    
    /// Priority for limited space
    pub priority: u8,
}

#[derive(Serialize, Deserialize, Clone)]
pub enum SurfaceType {
    Conversation,
    CodeEditor { language: Option<String> },
    Terminal,
    WebView { url: Option<String> },
    FileBrowser { path: Option<String> },
    Preview { file: Option<String> },
    Custom { renderer: String },
}
```

### Example Layouts

**Morning Coding Layout:**
```json
{
    "layout_type": { "Columns": { "ratios": [0.25, 0.5, 0.25] } },
    "surfaces": [
        {
            "id": "conversation",
            "surface_type": "Conversation",
            "position": { "column": 0 },
            "priority": 2
        },
        {
            "id": "editor",
            "surface_type": { "CodeEditor": { "language": "rust" } },
            "position": { "column": 1 },
            "priority": 1
        },
        {
            "id": "terminal",
            "surface_type": "Terminal",
            "position": { "column": 2 },
            "priority": 3
        }
    ]
}
```

**Writing Focus Layout:**
```json
{
    "layout_type": "Single",
    "surfaces": [
        {
            "id": "document",
            "surface_type": { "WebView": { "url": null } },
            "content": { "type": "markdown_editor" },
            "priority": 1
        }
    ]
}
```

**Research Layout:**
```json
{
    "layout_type": { "Split": { "ratio": 0.6, "direction": "horizontal" } },
    "surfaces": [
        {
            "id": "browser",
            "surface_type": { "WebView": {} },
            "position": { "side": "left" }
        },
        {
            "id": "notes",
            "surface_type": "Conversation",
            "position": { "side": "right" }
        }
    ]
}
```

---

## Transition System

Smooth transitions between layouts:

```rust
pub struct TransitionSpec {
    /// Duration in milliseconds
    pub duration_ms: u32,
    
    /// Easing function
    pub easing: EasingFunction,
    
    /// Per-surface transition rules
    pub surface_transitions: HashMap<String, SurfaceTransition>,
}

#[derive(Clone)]
pub enum SurfaceTransition {
    /// Slide from direction
    Slide { from: Direction },
    
    /// Fade in/out
    Fade,
    
    /// Scale from center
    Scale { from: f32 },
    
    /// Morph from another surface
    Morph { from_surface: String },
    
    /// No transition
    Instant,
}

impl LayoutManager {
    /// Transition from current layout to new layout
    pub async fn transition_to(&mut self, new_layout: LayoutSpec) {
        let old_layout = self.current_layout.clone();
        
        // Determine which surfaces are:
        // - Staying (animate resize/move)
        // - Leaving (animate out)
        // - Entering (animate in)
        
        let staying: Vec<_> = old_layout.surfaces.iter()
            .filter(|s| new_layout.surfaces.iter().any(|n| n.id == s.id))
            .collect();
        
        let leaving: Vec<_> = old_layout.surfaces.iter()
            .filter(|s| !new_layout.surfaces.iter().any(|n| n.id == s.id))
            .collect();
        
        let entering: Vec<_> = new_layout.surfaces.iter()
            .filter(|s| !old_layout.surfaces.iter().any(|o| o.id == s.id))
            .collect();
        
        // Start transitions
        let transition = new_layout.transitions.clone();
        let duration = transition.duration_ms;
        
        // Animate staying surfaces
        for surface in staying {
            let new_spec = new_layout.surfaces.iter()
                .find(|s| s.id == surface.id)
                .unwrap();
            self.animate_surface_change(&surface.id, new_spec, duration);
        }
        
        // Animate leaving surfaces
        for surface in leaving {
            let trans = transition.surface_transitions
                .get(&surface.id)
                .cloned()
                .unwrap_or(SurfaceTransition::Fade);
            self.animate_surface_out(&surface.id, trans, duration);
        }
        
        // Animate entering surfaces
        for surface in entering {
            let trans = transition.surface_transitions
                .get(&surface.id)
                .cloned()
                .unwrap_or(SurfaceTransition::Fade);
            self.animate_surface_in(surface, trans, duration);
        }
        
        // Wait for transitions to complete
        tokio::time::sleep(Duration::from_millis(duration as u64)).await;
        
        // Update current layout
        self.current_layout = new_layout;
    }
}
```

---

## Telemetry and Privacy

All UI learning respects privacy:

```rust
/// Privacy-respecting interaction record
#[derive(Serialize, Deserialize)]
pub struct InteractionRecord {
    /// Hashed session ID (not traceable to user)
    pub session_hash: String,
    
    /// Timestamp (rounded to nearest hour)
    pub timestamp_hour: DateTime<Utc>,
    
    /// Action type (never content)
    pub action_type: ActionType,
    
    /// Surface type involved
    pub surface_type: SurfaceType,
    
    /// Layout at time of action (structure only)
    pub layout_type: LayoutType,
    
    /// Time since last action (bucketed)
    pub time_since_last_bucket: TimeBucket,
    
    /// Success signal (task completed?)
    pub success_signal: Option<bool>,
}

/// What we DON'T collect:
/// - Text content
/// - File names or paths
/// - URLs visited
/// - Personal identifiers
/// - Exact timestamps
/// - Raw mouse/keyboard data

impl InteractionLogger {
    pub fn log(&mut self, raw_event: &RawEvent) {
        // Sanitize before recording
        let record = InteractionRecord {
            session_hash: hash(&self.session_id),
            timestamp_hour: raw_event.timestamp.round_to_hour(),
            action_type: categorize_action(&raw_event.action),
            surface_type: raw_event.surface.surface_type.clone(),
            layout_type: self.current_layout.layout_type.clone(),
            time_since_last_bucket: bucket_duration(raw_event.timestamp - self.last_action),
            success_signal: None, // Set later when task completes
        };
        
        self.records.push(record);
    }
}
```

---

## Evolution Timeline Example

### Week 1: New User

```
┌────────────────────────────────────────┐
│          Conversation (Full)           │
│                                        │
│  "Hello! I'm Clay. What would you     │
│   like to work on today?"              │
│                                        │
│  Suggestions based on time of day:     │
│  [📝 Write] [💻 Code] [📊 Analyze]     │
│                                        │
│  > [Your message here...]              │
└────────────────────────────────────────┘
```

### Week 2: Learns User Codes in Morning

```
┌──────────────────┬─────────────────────┐
│  Conversation    │  Last Project       │
│                  │  ~/code/mycel-os     │
│  Good morning!   │                     │
│                  │  Recent files:      │
│  Continue where  │  - main.rs          │
│  you left off?   │  - Cargo.toml       │
│                  │  - README.md        │
│  > [input]       │                     │
└──────────────────┴─────────────────────┘
```

### Week 4: Adopts Collective Pattern

```
┌────────────┬───────────────────┬───────────────┐
│ Convo      │    Code Editor    │   Terminal    │
│            │                   │               │
│ [history]  │  fn main() {      │  $ cargo run  │
│            │      println!...  │  > Running... │
│            │  }                │               │
│            │                   │  $ git status │
│            │                   │               │
│ > [input]  │ [ai suggestions]  │  > [output]   │
└────────────┴───────────────────┴───────────────┘

[Network badge: "Layout adopted from 847 developers"]
```

### Week 8: Personalized Hybrid

```
┌─────────────────────────────────────────────────┐
│  ┌─────────┐                                    │
│  │Quick    │   Main Workspace                   │
│  │Actions  │   ┌─────────────────────────────┐ │
│  │         │   │                             │ │
│  │[⚡ Run] │   │    [Adaptive content]       │ │
│  │[📝 Doc] │   │                             │ │
│  │[🔍 Find]│   │    Based on current task    │ │
│  │[💾 Save]│   │                             │ │
│  │         │   └─────────────────────────────┘ │
│  │[Custom] │   ┌─────────────────────────────┐ │
│  │[Custom] │   │  AI Assistant (collapsed)   │ │
│  └─────────┘   └─────────────────────────────┘ │
└─────────────────────────────────────────────────┘

[Personal optimizations applied]
[Collective pattern: sidebar_quick_actions v2.3]
```

---

## Implementation Roadmap

### Phase 3.1: Static Compositor (Weeks 21-24)
- Basic Wayland compositor
- Fixed layouts (single, split)
- Surface rendering

### Phase 3.2: Surface Types (Weeks 25-28)
- Conversation surface
- Code editor integration
- Terminal embedding
- WebKit web views

### Phase 3.3: Dynamic Layouts (Weeks 29-32)
- Layout specification parser
- Runtime layout changes
- Basic transitions

### Phase 4.1: Personal Learning (Weeks 33-36)
- Interaction logging (privacy-safe)
- Pattern extraction
- Local preference model

### Phase 4.2: Collective Patterns (Weeks 37-40)
- UI pattern format
- NEAR registry integration
- Bittensor evaluation

### Phase 4.3: AI Synthesis (Weeks 41-44)
- Context analysis
- Layout generation
- Smooth adaptation

---

## Technical Challenges

### Challenge 1: Performance
**Problem:** Real-time layout changes must not cause stuttering.
**Solution:** 
- Pre-compute layouts in background
- Use GPU for transitions
- Cache frequently used layouts

### Challenge 2: Learnability
**Problem:** System changes can confuse users.
**Solution:**
- Gradual changes with preview
- "Why did this change?" explanations
- Easy revert to previous layouts

### Challenge 3: Privacy
**Problem:** Learning requires data that could be sensitive.
**Solution:**
- Local-first learning
- Differential privacy for shared patterns
- Transparent data policies

### Challenge 4: Cold Start
**Problem:** New users have no data to personalize.
**Solution:**
- Onboarding questions
- Collective defaults by demographic
- Rapid learning from first few sessions

---

*The interface is not a product. It's a garden that grows with you.*
