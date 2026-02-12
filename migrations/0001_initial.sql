-- @up

CREATE EXTENSION IF NOT EXISTS pgcrypto;
CREATE EXTENSION IF NOT EXISTS vector;

CREATE TABLE IF NOT EXISTS messages (
    id                       uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    platform_id              text UNIQUE,
    platform_chat_id         text NOT NULL,
    platform_sender_id       text,
    direction                text NOT NULL CHECK (direction IN ('in', 'out')),
    content                  text,
    attachments              jsonb DEFAULT '[]'::jsonb,
    embedding                vector(768),
    content_version          int NOT NULL DEFAULT 1,
    audit_processed_version  int NOT NULL DEFAULT 1,
    routed_at                timestamptz,
    audit_processed_at       timestamptz,
    is_deleted               bool NOT NULL DEFAULT false,
    trace_id                 uuid,
    reply_to_id              uuid,
    job_id                   uuid,
    rewritten_at             timestamptz,
    created_at               timestamptz NOT NULL DEFAULT now(),
    updated_at               timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS outbox (
    id                      uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    chat_id                 text NOT NULL,
    content                 text,
    attachments             jsonb DEFAULT '[]'::jsonb,
    reply_to                text,
    processed_at            timestamptz,
    attempt_count           int NOT NULL DEFAULT 0,
    last_error              text,
    trace_id                uuid,
    job_id                  uuid,
    reply_to_message_id     uuid,
    rewritten_at            timestamptz,
    created_at              timestamptz NOT NULL DEFAULT now(),
    updated_at              timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS jobs (
    id                  uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    kind                text NOT NULL DEFAULT 'action' CHECK (kind IN ('action', 'chat', 'schedule')),
    chat_id             text NOT NULL,
    status              text NOT NULL DEFAULT 'draft' CHECK (status IN ('draft', 'pending', 'running', 'paused', 'done', 'failed', 'cancelled')),
    prompt              text,
    enriched_prompt     text,
    source_ids          uuid[] DEFAULT '{}',
    resume_input        text,
    output              text,
    error               text,
    cancel_reason       text,
    started_at          timestamptz,
    finished_at         timestamptz,
    trace_id            uuid,
    forge_job_id        uuid,
    session_id          text,
    container_id        text,
    last_heartbeat_at   timestamptz,
    question_pending    text,
    created_at          timestamptz NOT NULL DEFAULT now(),
    updated_at          timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS crons (
    id              uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    name            text NOT NULL UNIQUE,
    schedule        text NOT NULL,
    timezone        text NOT NULL DEFAULT 'UTC',
    chat_id         text NOT NULL,
    prompt          text NOT NULL,
    enabled         bool NOT NULL DEFAULT true,
    last_run_at     timestamptz,
    next_run_at     timestamptz,
    last_job_id     uuid,
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS logs (
    id              uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    job_id          uuid NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
    stream          text NOT NULL DEFAULT 'stdout' CHECK (stream IN ('stdout', 'stderr')),
    line            text NOT NULL,
    created_at      timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS events (
    id              uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    trace_id        uuid,
    source          text NOT NULL,
    action          text NOT NULL,
    payload         jsonb DEFAULT '{}'::jsonb,
    created_at      timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS chat_subscriptions (
    chat_id      text PRIMARY KEY,
    enabled      bool NOT NULL DEFAULT true,
    created_at   timestamptz NOT NULL DEFAULT now(),
    updated_at   timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS agent_steps (
    id              uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    job_id          uuid NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
    step_number     int NOT NULL DEFAULT 0,
    tool_name       text,
    input_summary   text,
    output_summary  text,
    duration_ms     int,
    created_at      timestamptz NOT NULL DEFAULT now()
);

-- indexes

CREATE INDEX IF NOT EXISTS idx_messages_unrouted ON messages (created_at) WHERE direction = 'in' AND routed_at IS NULL;
CREATE INDEX IF NOT EXISTS idx_messages_audit ON messages (updated_at) WHERE audit_processed_version < content_version OR (is_deleted = true AND audit_processed_at IS NULL);
CREATE INDEX IF NOT EXISTS idx_messages_chat_created ON messages (platform_chat_id, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_outbox_unprocessed ON outbox (created_at) WHERE processed_at IS NULL;
CREATE INDEX IF NOT EXISTS idx_outbox_pending_rewrite ON outbox (created_at) WHERE rewritten_at IS NULL AND processed_at IS NULL;
CREATE INDEX IF NOT EXISTS idx_jobs_active ON jobs (status, updated_at) WHERE status IN ('draft', 'pending', 'running', 'paused');
CREATE INDEX IF NOT EXISTS idx_jobs_source_ids ON jobs USING gin (source_ids);
CREATE INDEX IF NOT EXISTS idx_jobs_forge_job ON jobs (forge_job_id) WHERE forge_job_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_jobs_actionable ON jobs (status, created_at) WHERE status IN ('pending', 'paused', 'running');
CREATE INDEX IF NOT EXISTS idx_events_trace ON events (trace_id, created_at);
CREATE INDEX IF NOT EXISTS idx_logs_job ON logs (job_id, created_at);
CREATE INDEX IF NOT EXISTS idx_messages_embedding ON messages USING ivfflat (embedding vector_cosine_ops) WITH (lists = 100);
CREATE INDEX IF NOT EXISTS idx_agent_steps_job ON agent_steps (job_id, step_number);

-- triggers

CREATE OR REPLACE FUNCTION touch_updated_at()
RETURNS TRIGGER AS $$
BEGIN
    NEW.updated_at = now();
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

DO $$ BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_trigger WHERE tgname = 'messages_updated_at') THEN
        CREATE TRIGGER messages_updated_at BEFORE UPDATE ON messages FOR EACH ROW EXECUTE FUNCTION touch_updated_at();
    END IF;
    IF NOT EXISTS (SELECT 1 FROM pg_trigger WHERE tgname = 'outbox_updated_at') THEN
        CREATE TRIGGER outbox_updated_at BEFORE UPDATE ON outbox FOR EACH ROW EXECUTE FUNCTION touch_updated_at();
    END IF;
    IF NOT EXISTS (SELECT 1 FROM pg_trigger WHERE tgname = 'jobs_updated_at') THEN
        CREATE TRIGGER jobs_updated_at BEFORE UPDATE ON jobs FOR EACH ROW EXECUTE FUNCTION touch_updated_at();
    END IF;
    IF NOT EXISTS (SELECT 1 FROM pg_trigger WHERE tgname = 'crons_updated_at') THEN
        CREATE TRIGGER crons_updated_at BEFORE UPDATE ON crons FOR EACH ROW EXECUTE FUNCTION touch_updated_at();
    END IF;
    IF NOT EXISTS (SELECT 1 FROM pg_trigger WHERE tgname = 'chat_subscriptions_updated_at') THEN
        CREATE TRIGGER chat_subscriptions_updated_at BEFORE UPDATE ON chat_subscriptions FOR EACH ROW EXECUTE FUNCTION touch_updated_at();
    END IF;
END $$;

-- reactivity

SELECT forge_enable_reactivity('messages');
SELECT forge_enable_reactivity('outbox');
SELECT forge_enable_reactivity('jobs');
SELECT forge_enable_reactivity('crons');
SELECT forge_enable_reactivity('logs');
SELECT forge_enable_reactivity('events');
SELECT forge_enable_reactivity('agent_steps');

-- @down

SELECT forge_disable_reactivity('agent_steps');
SELECT forge_disable_reactivity('events');
SELECT forge_disable_reactivity('logs');
SELECT forge_disable_reactivity('crons');
SELECT forge_disable_reactivity('jobs');
SELECT forge_disable_reactivity('outbox');
SELECT forge_disable_reactivity('messages');
DROP INDEX IF EXISTS idx_agent_steps_job;
DROP INDEX IF EXISTS idx_messages_embedding;
DROP INDEX IF EXISTS idx_logs_job;
DROP INDEX IF EXISTS idx_events_trace;
DROP INDEX IF EXISTS idx_jobs_actionable;
DROP INDEX IF EXISTS idx_jobs_forge_job;
DROP INDEX IF EXISTS idx_jobs_source_ids;
DROP INDEX IF EXISTS idx_jobs_active;
DROP INDEX IF EXISTS idx_outbox_pending_rewrite;
DROP INDEX IF EXISTS idx_outbox_unprocessed;
DROP INDEX IF EXISTS idx_messages_chat_created;
DROP INDEX IF EXISTS idx_messages_audit;
DROP INDEX IF EXISTS idx_messages_unrouted;
DROP TRIGGER IF EXISTS chat_subscriptions_updated_at ON chat_subscriptions;
DROP TRIGGER IF EXISTS crons_updated_at ON crons;
DROP TRIGGER IF EXISTS jobs_updated_at ON jobs;
DROP TRIGGER IF EXISTS outbox_updated_at ON outbox;
DROP TRIGGER IF EXISTS messages_updated_at ON messages;
DROP FUNCTION IF EXISTS touch_updated_at();
DROP TABLE IF EXISTS agent_steps;
DROP TABLE IF EXISTS chat_subscriptions;
DROP TABLE IF EXISTS events;
DROP TABLE IF EXISTS logs;
DROP TABLE IF EXISTS crons;
DROP TABLE IF EXISTS jobs;
DROP TABLE IF EXISTS outbox;
DROP TABLE IF EXISTS messages;
DROP EXTENSION IF EXISTS vector;
DROP EXTENSION IF EXISTS pgcrypto;
