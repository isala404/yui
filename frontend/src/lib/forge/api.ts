const API = import.meta.env.VITE_API_URL || "http://localhost:8080";

async function rpc<T>(
  fn_name: string,
  args: Record<string, unknown> = {},
): Promise<T> {
  const res = await fetch(`${API}/_api/rpc/${fn_name}`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ args }),
  });
  const data = await res.json();
  if (!data.success) throw new Error(data.error?.message || "RPC failed");
  return data.data;
}

export const listEvents = (args: { trace_id?: string; limit?: number } = {}) =>
  rpc<EventRow[]>("list_events", args);

export const listJobs = (args: { status?: string; limit?: number } = {}) =>
  rpc<Job[]>("list_jobs", args);

export const listOutbox = (
  args: { pending_only?: boolean; limit?: number } = {},
) => rpc<Outbox[]>("list_outbox", args);

export const listCrons = (args: { limit?: number } = {}) =>
  rpc<Cron[]>("list_crons", args);

export const listMessages = (args: { chat_id?: string; limit?: number } = {}) =>
  rpc<Message[]>("list_messages", args);

export const getTrace = (args: { trace_id: string }) =>
  rpc<TraceView>("get_trace", args);

export const cancelJob = (args: { job_id: string; reason?: string }) =>
  rpc<{ cancelled: boolean }>("cancel_job", args);

export const toggleCron = (args: { cron_id: string; enabled: boolean }) =>
  rpc<{ updated: boolean }>("toggle_cron", args);

export const getHealth = () => rpc<Health>("get_health", {});

export interface Health {
  pending_jobs: number;
  running_jobs: number;
  paused_jobs: number;
  pending_outbox: number;
  dead_letter_outbox: number;
  stuck_jobs: number;
}

export interface EventRow {
  id: string;
  trace_id: string | null;
  source: string;
  action: string;
  payload: Record<string, unknown>;
  created_at: string;
}

export interface Job {
  id: string;
  kind: string;
  chat_id: string;
  status: string;
  prompt: string | null;
  enriched_prompt: string | null;
  source_ids: string[];
  resume_input: string | null;
  output: string | null;
  error: string | null;
  cancel_reason: string | null;
  started_at: string | null;
  finished_at: string | null;
  trace_id: string | null;
  created_at: string;
  updated_at: string;
}

export interface Outbox {
  id: string;
  chat_id: string;
  content: string | null;
  attachments: unknown;
  reply_to: string | null;
  processed_at: string | null;
  attempt_count: number;
  last_error: string | null;
  trace_id: string | null;
  created_at: string;
  updated_at: string;
}

export interface Cron {
  id: string;
  name: string;
  schedule: string;
  timezone: string;
  chat_id: string;
  prompt: string;
  enabled: boolean;
  last_run_at: string | null;
  next_run_at: string | null;
  created_at: string;
  updated_at: string;
}

export interface Message {
  id: string;
  platform_id: string | null;
  platform_chat_id: string;
  platform_sender_id: string | null;
  direction: string;
  content: string | null;
  attachments: unknown;
  content_version: number;
  audit_processed_version: number;
  routed_at: string | null;
  audit_processed_at: string | null;
  is_deleted: boolean;
  trace_id: string | null;
  created_at: string;
  updated_at: string;
}

export interface TraceView {
  events: EventRow[];
  jobs: Job[];
  messages: Message[];
}
