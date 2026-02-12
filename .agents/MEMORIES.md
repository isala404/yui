Tooling
- Stack: Rust (edition 2024), Forge framework (forgex 0.2.0), SvelteKit 5, PostgreSQL 17 + pgvector
- Package manager: cargo (backend), bun (frontend)
- Dev: `forge dev --no-pg` with external DATABASE_URL
- Build: `cargo check --no-default-features` (no embedded frontend during dev)
- Frontend: `bun dev` on :5173, backend on :8080

Architecture
- 8 daemon loops: gateway, triage, context, clock, runtime, reply, delivery, audit
- 2 pluggable services: AiService, AgentRunnerService
- DB-first state machine: all state in PostgreSQL, polled by daemons
- Event sourcing: every state transition writes to `events` table

Patterns
- Forge daemons: `#[forge::daemon]` with `ctx.shutdown_signal()` select loop
- Forge queries: `#[forge::query(public)]` returns `Result<T>`
- Forge mutations: `#[forge::mutation(public, transactional)]` uses `ctx.db()` (returns DbConn, use `.execute()/.fetch_all()`)
- Schema models: `#[forge::model]` with `#[derive(sqlx::FromRow)]`
- Enums: `#[forge::forge_enum]` stores as lowercase text in DB
- sqlx macros need live DB schema at compile time
- Migrations: `-- @up` / `-- @down` format, auto-applied by forge on startup
- Use `IF NOT EXISTS` in migrations for idempotency (tables pre-exist for sqlx compile check)

Gotchas
- `sqlx::query_as!` with `uuid[]` columns needs `as "col_name!"` override (Vec<Uuid> vs Option<Vec<Uuid>>)
- Forge MutationContext.db() returns DbConn (not PgPool), use DbConn methods not .execute() directly on query
- QueryContext.db() returns &PgPool (can use directly with sqlx macros)
- DaemonContext.db() returns &PgPool
- Input structs for queries/mutations need both Serialize + Deserialize
- `replace_all` in Edit tool doesn't preserve trailing spaces. Watch for `EXISTSfoo` when doing mass replacements

DB
- pgvector/pgvector:pg17 image for docker compose
- Local dev: OrbStack postgres on :5432, database `yui`, user `postgres`, password `forge`
- Vector dimension: 768
- Extensions: pgcrypto, vector
