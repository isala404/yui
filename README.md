# Yui

An async-first AI assistant that works like texting a real person. Built on [Forge](https://github.com/isala404/forge).

## The Problem

Every AI assistant today forces a turn-based interaction model. You send a message, wait for a response, send another. That's nothing like working with a real assistant.

When you work with a person, you fire off thoughts as they come. You say "handle this" and move on. You change your mind mid-sentence. You send a photo, a voice note, a file. You trust they'll figure it out, juggle multiple tasks, and come back when things are done or when they need clarification.

The intelligence is there. LLMs are capable enough. The problem is orchestration. Current systems force synchronous, single-threaded interaction onto what should be an asynchronous, multi-threaded relationship.

## What It Feels Like

You text Yui on WhatsApp like you'd text a competent assistant.

- Send messages at your own pace. Yui watches for your typing indicator and waits until you've stopped for 5 seconds before processing. No race conditions between your thoughts and the system's eagerness to respond.
- Simple questions get instant answers. Complex requests spin up isolated agent containers that run in the background.
- Ask for multiple things at once, or across multiple messages. Each task progresses independently.
- Send text, images, files, voice. Get rich responses back.
- If the agent needs clarification, it asks. You reply naturally. Work resumes.
- Edit a message or change your mind. The system detects it, cancels affected work, restarts with your correction.
- A lightweight dashboard shows everything in flight so you always know what's happening.

## Architecture

Seven small, stateless loops polling a shared PostgreSQL database. Each loop is single-threaded, restartable, and independently deployable. Only the agent containers run in parallel.

```
WhatsApp ←→ Gateway ──→ messages table
                              │
                         Triage ──→ jobs / outbox / crons
                              │
                        Context ──→ enriched jobs (pending)
                              │
            Clock ──→ jobs    Runtime ──→ containers ──→ logs / outbox
                              │
                        Delivery ──→ WhatsApp
                              │
                          Audit ──→ cancellations on edit/delete
```

PostgreSQL is the single source of truth. If it's not in a row, it didn't happen. No loop triggers another. They discover work by polling, eliminating an entire class of failure modes around lost messages, ordering, and coupling. Kill any loop at any point, restart it, and it picks up where it left off.

### The Seven Loops

| Loop | Role | Reads | Writes |
|------|------|-------|--------|
| **Gateway** | WhatsApp I/O, typing-aware buffering | jobs, outbox | messages, events |
| **Triage** | Intent classification and routing | messages, jobs | jobs, outbox, crons, events |
| **Context** | RAG enrichment, promotes draft to pending | jobs, messages | jobs, outbox, events |
| **Clock** | Fires scheduled tasks | crons | jobs, crons, events |
| **Runtime** | Spawns/monitors agent containers | jobs | jobs, logs, outbox, events |
| **Delivery** | Sends outbox entries to WhatsApp | outbox | outbox, messages, events |
| **Audit** | Detects edits/deletes, cancels affected work | messages, jobs | messages, jobs, outbox, events |

### Principles

- **Database is truth.** All state lives in PostgreSQL. Every loop reads and writes the same tables.
- **Loops are stateless.** No in-memory state survives a restart. Everything reconstructable from the database.
- **No loop triggers another.** Polling only. This makes failure modes obvious and recovery trivial.
- **Single-tenant.** One user, one database. Simplicity over scalability.
- **Everything carries a `trace_id`.** One identifier threads through messages, jobs, outbox entries, and logs. Query by trace_id and you get the full story.

## Why Forge

Yui is built on [Forge](https://github.com/isala404/forge), a full-stack Rust framework where PostgreSQL is your only infrastructure.

Each of Yui's seven loops is a Forge [daemon](https://tryforge.dev/docs). The dashboard queries and mutations are Forge functions with automatic TypeScript codegen for the SvelteKit frontend. Migrations, background jobs, cron scheduling, real-time subscriptions, all handled by the framework. No Redis, no Kafka, no message queues.

Forge gives Yui:
- **Daemon primitives** with graceful shutdown, restart-on-panic, and leader election
- **Transactional mutations** with automatic rollback
- **Real-time subscriptions** powering the live dashboard
- **Built-in observability** for tracing requests across loops
- **Embedded PostgreSQL** for zero-config local development
- **End-to-end type safety** from Rust models to Svelte components

The entire backend is ~4,000 lines of Rust. Each loop fits in a single file.

## Scenarios

### Simple Chat

> "What's 2+2?"

Gateway buffers the message, waits for typing to stop, flushes to the database. Triage classifies it as chat, writes "4" directly to the outbox. Delivery sends it. No job created. Fast path.

### Background Task

> "Create a new repo called forge-v2"

Triage creates a draft job. Context enriches it with conversation history and promotes it to pending. Runtime spawns an agent container. Gateway sends typing indicators while the container runs. When it finishes, the result flows through the outbox to delivery.

### Message Buffering

> "Clone the repo forge-v2"
> *(typing...)*
> "And install the dependencies"

Gateway holds both messages while typing is active. After 5 seconds of idle, both flush together. Triage sees the full batch, detects the dependency, and creates one job instead of two.

### Parallel Tasks

> "Check weather in London. Also run the DB migration."

Triage splits this into two independent jobs. Both flow through context and runtime in parallel. Results arrive independently.

### Pause and Resume

> "Deploy to production"
> Agent asks: "Which environment: staging or prod?"
> "prod"

Runtime pauses the job and writes the question to outbox. User's reply gets routed by triage back to the paused job. Runtime resumes the container with the answer.

### Edit Cancellation

> "Delete all test files"
> *(edits to)* "Delete all test files EXCEPT fixtures"

Audit detects the edit, cancels the running job, kills the container, notifies the user, and creates a new draft job with the corrected message. The corrected request flows through the normal pipeline.

## Stack

- **Backend:** Rust 2024 edition + [Forge](https://github.com/isala404/forge)
- **Frontend:** SvelteKit 5 + TypeScript
- **Database:** PostgreSQL 17 + pgvector
- **Messaging:** WhatsApp via [whatsapp-rust](https://github.com/nicksenger/whatsapp-rust)

## Project Structure

```
src/
  main.rs                    # Registers 7 daemons + dashboard functions
  functions/
    gateway.rs               # WhatsApp I/O with typing-aware buffering
    triage.rs                # Intent classification and routing
    context.rs               # RAG enrichment
    clock.rs                 # Cron scheduling
    runtime.rs               # Agent container orchestration
    delivery.rs              # Outbox processing and WhatsApp delivery
    audit.rs                 # Edit/delete detection and cancellation
    dashboard.rs             # Dashboard queries and mutations
  services/
    ai.rs                    # AI service trait (mock in V1)
    agent_runner.rs          # Agent runner trait (mock in V1)
  schema/
    message.rs, job.rs, outbox.rs, cron.rs, event.rs, log_entry.rs
migrations/
  0001_initial.sql           # Full schema with pgvector, indexes, triggers
frontend/
  src/routes/+page.svelte    # Dashboard
forge.toml                   # Forge configuration
```

## Quick Start

```bash
# requires rust nightly (pinned in rust-toolchain.toml) and bun

# start with external postgres on DATABASE_URL
forge dev --no-pg

# or let forge manage postgres
forge dev
```

Backend runs on `http://localhost:8080`, frontend on `http://localhost:5173`.

On first run, Gateway prints a QR code in the terminal for WhatsApp pairing.

## Database

Six core tables, all carrying `trace_id` for end-to-end debugging:

- **messages** - conversation history with vector embeddings (768-dim) for RAG
- **outbox** - pending deliveries with retry tracking
- **jobs** - async work items with full lifecycle (draft, pending, running, paused, done, failed, cancelled)
- **crons** - scheduled tasks with timezone-aware scheduling
- **logs** - container stdout/stderr streams
- **events** - append-only audit log for every state change across every loop

## Dashboard

Reads directly from the database. Since the database is the single source of truth, the dashboard is just a window into system state.

- **Live Feed** - chronological stream of events across all loops
- **Jobs** - active jobs grouped by status, live log tailing for running jobs
- **Outbox** - pending and recent deliveries
- **Crons** - scheduled tasks with enable/disable toggle
- **Messages** - full conversation history with inline media
- **Trace Search** - enter a trace_id, see every database row touched by that request

## Current State

V1 is a working proof of concept with two mock services as swap points:

- `AiService` - handles triage decisions, prompt enrichment, and embeddings. Currently rule-based, drop in a real LLM provider by implementing the trait.
- `AgentRunnerService` - handles job execution. Currently in-process simulation, drop in Docker container management by implementing the trait.

Everything else is real. WhatsApp integration, typing-aware buffering, message routing, job lifecycle, cron scheduling, edit cancellation, delivery with retry, the full event audit trail.

## License

MIT
