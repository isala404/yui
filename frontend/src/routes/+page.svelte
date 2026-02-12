<script lang="ts">
  import { onMount } from 'svelte';
  import {
    listJobs, listMessages, listOutbox, listCrons, listEvents, getTrace,
    cancelJob, toggleCron, getHealth,
    type Job, type Message, type Outbox, type Cron, type EventRow, type TraceView, type Health,
  } from '$lib/forge/api';

  let tab = $state<'jobs' | 'messages' | 'outbox' | 'crons' | 'events' | 'trace'>('jobs');
  let jobs = $state<Job[]>([]);
  let messages = $state<Message[]>([]);
  let outbox = $state<Outbox[]>([]);
  let crons = $state<Cron[]>([]);
  let events = $state<EventRow[]>([]);
  let trace = $state<TraceView | null>(null);
  let health = $state<Health | null>(null);
  let traceId = $state('');
  let jobStatusFilter = $state('');
  let loading = $state(false);
  let error = $state('');
  const JOB_STATUSES = ['draft','pending','running','paused','done','failed','cancelled'] as const;
  const ACTIVE_STATUSES = ['draft','pending','running','paused'];
  let pollTimer: ReturnType<typeof setInterval>;

  function toErrorMessage(e: unknown) {
    return e instanceof Error ? e.message : String(e);
  }

  async function refresh() {
    try {
      error = '';
      getHealth().then(h => health = h).catch(() => {});
      if (tab === 'jobs') jobs = await listJobs(jobStatusFilter ? { status: jobStatusFilter } : {});
      else if (tab === 'messages') messages = await listMessages({});
      else if (tab === 'outbox') outbox = await listOutbox({});
      else if (tab === 'crons') crons = await listCrons({});
      else if (tab === 'events') events = await listEvents({ limit: 100 });
    } catch (e: unknown) {
      error = toErrorMessage(e);
    }
  }

  async function loadTrace() {
    if (!traceId.trim()) return;
    loading = true;
    try {
      trace = await getTrace({ trace_id: traceId.trim() });
      error = '';
    } catch (e: unknown) {
      error = toErrorMessage(e);
      trace = null;
    } finally {
      loading = false;
    }
  }

  async function handleCancelJob(id: string) {
    await cancelJob({ job_id: id });
    await refresh();
  }

  async function handleToggleCron(id: string, enabled: boolean) {
    await toggleCron({ cron_id: id, enabled });
    await refresh();
  }

  function switchTab(t: typeof tab) {
    tab = t;
    trace = null;
    refresh();
  }

  function fmt(ts: string | null) {
    if (!ts) return '\u2014';
    return new Date(ts).toLocaleString();
  }

  function short(id: string | null) {
    if (!id) return '\u2014';
    return id.slice(0, 8);
  }

  onMount(() => {
    refresh();
    pollTimer = setInterval(refresh, 3000);
    return () => clearInterval(pollTimer);
  });
</script>

<div class="shell">
  <header>
    <h1>yui</h1>
    <nav>
      {#each ['jobs', 'messages', 'outbox', 'crons', 'events', 'trace'] as t (t)}
        <button class:active={tab === t} onclick={() => switchTab(t as typeof tab)}>{t}</button>
      {/each}
    </nav>
  </header>

  {#if health}
    <div class="health">
      <span class="stat" class:warn={health.running_jobs > 0}>
        <b>{health.running_jobs}</b> running
      </span>
      <span class="stat" class:warn={health.pending_jobs > 0}>
        <b>{health.pending_jobs}</b> pending
      </span>
      <span class="stat" class:warn={health.paused_jobs > 0}>
        <b>{health.paused_jobs}</b> paused
      </span>
      <span class="stat" class:warn={health.pending_outbox > 0}>
        <b>{health.pending_outbox}</b> outbox
      </span>
      <span class="stat" class:alert={health.stuck_jobs > 0}>
        <b>{health.stuck_jobs}</b> stuck
      </span>
      <span class="stat" class:alert={health.dead_letter_outbox > 0}>
        <b>{health.dead_letter_outbox}</b> dead
      </span>
    </div>
  {/if}

  {#if error}
    <div class="error">{error}</div>
  {/if}

  <main>
    {#if tab === 'jobs'}
      <div class="toolbar">
        <select bind:value={jobStatusFilter} onchange={refresh}>
          <option value="">all statuses</option>
          {#each JOB_STATUSES as s (s)}
            <option value={s}>{s}</option>
          {/each}
        </select>
      </div>
      <table>
        <thead><tr>
          <th>id</th><th>kind</th><th>status</th><th>chat</th><th>prompt</th><th>created</th><th></th>
        </tr></thead>
        <tbody>
          {#each jobs as j (j.id)}
            <tr>
              <td class="mono">{short(j.id)}</td>
              <td>{j.kind}</td>
              <td><span class="badge {j.status}">{j.status}</span></td>
              <td class="mono">{short(j.chat_id)}</td>
              <td class="truncate">{j.prompt ?? '\u2014'}</td>
              <td>{fmt(j.created_at)}</td>
              <td>
                {#if ACTIVE_STATUSES.includes(j.status)}
                  <button class="sm danger" onclick={() => handleCancelJob(j.id)}>cancel</button>
                {/if}
                {#if j.trace_id}
                  <button class="sm" onclick={() => { traceId = j.trace_id!; tab = 'trace'; loadTrace(); }}>trace</button>
                {/if}
              </td>
            </tr>
          {/each}
          {#if jobs.length === 0}
            <tr><td colspan="7" class="empty">no jobs</td></tr>
          {/if}
        </tbody>
      </table>

    {:else if tab === 'messages'}
      <table>
        <thead><tr>
          <th>id</th><th>dir</th><th>chat</th><th>sender</th><th>content</th><th>routed</th><th>created</th>
        </tr></thead>
        <tbody>
          {#each messages as m (m.id)}
            <tr class:deleted={m.is_deleted}>
              <td class="mono">{short(m.id)}</td>
              <td><span class="badge {m.direction}">{m.direction}</span></td>
              <td class="mono">{short(m.platform_chat_id)}</td>
              <td class="mono">{short(m.platform_sender_id)}</td>
              <td class="truncate">{m.content ?? '\u2014'}</td>
              <td>{m.routed_at ? 'yes' : 'no'}</td>
              <td>{fmt(m.created_at)}</td>
            </tr>
          {/each}
          {#if messages.length === 0}
            <tr><td colspan="7" class="empty">no messages</td></tr>
          {/if}
        </tbody>
      </table>

    {:else if tab === 'outbox'}
      <table>
        <thead><tr>
          <th>id</th><th>chat</th><th>content</th><th>attempts</th><th>error</th><th>sent</th><th>created</th>
        </tr></thead>
        <tbody>
          {#each outbox as o (o.id)}
            <tr>
              <td class="mono">{short(o.id)}</td>
              <td class="mono">{short(o.chat_id)}</td>
              <td class="truncate">{o.content ?? '\u2014'}</td>
              <td>{o.attempt_count}</td>
              <td class="truncate err">{o.last_error ?? ''}</td>
              <td>{o.processed_at ? 'yes' : 'pending'}</td>
              <td>{fmt(o.created_at)}</td>
            </tr>
          {/each}
          {#if outbox.length === 0}
            <tr><td colspan="7" class="empty">no outbox items</td></tr>
          {/if}
        </tbody>
      </table>

    {:else if tab === 'crons'}
      <table>
        <thead><tr>
          <th>name</th><th>schedule</th><th>tz</th><th>chat</th><th>enabled</th><th>last run</th><th>next run</th><th></th>
        </tr></thead>
        <tbody>
          {#each crons as c (c.id)}
            <tr>
              <td>{c.name}</td>
              <td class="mono">{c.schedule}</td>
              <td>{c.timezone}</td>
              <td class="mono">{short(c.chat_id)}</td>
              <td>{c.enabled ? 'on' : 'off'}</td>
              <td>{fmt(c.last_run_at)}</td>
              <td>{fmt(c.next_run_at)}</td>
              <td>
                <button class="sm" onclick={() => handleToggleCron(c.id, !c.enabled)}>
                  {c.enabled ? 'disable' : 'enable'}
                </button>
              </td>
            </tr>
          {/each}
          {#if crons.length === 0}
            <tr><td colspan="8" class="empty">no crons</td></tr>
          {/if}
        </tbody>
      </table>

    {:else if tab === 'events'}
      <table>
        <thead><tr>
          <th>id</th><th>source</th><th>action</th><th>payload</th><th>trace</th><th>created</th>
        </tr></thead>
        <tbody>
          {#each events as e (e.id)}
            <tr>
              <td class="mono">{short(e.id)}</td>
              <td>{e.source}</td>
              <td>{e.action}</td>
              <td class="truncate mono">{JSON.stringify(e.payload)}</td>
              <td>
                {#if e.trace_id}
                  <button class="sm" onclick={() => { traceId = e.trace_id!; tab = 'trace'; loadTrace(); }}>
                    {short(e.trace_id)}
                  </button>
                {:else}
                  &mdash;
                {/if}
              </td>
              <td>{fmt(e.created_at)}</td>
            </tr>
          {/each}
          {#if events.length === 0}
            <tr><td colspan="6" class="empty">no events</td></tr>
          {/if}
        </tbody>
      </table>

    {:else if tab === 'trace'}
      <div class="toolbar">
        <input type="text" placeholder="trace id" bind:value={traceId} />
        <button onclick={loadTrace} disabled={loading}>lookup</button>
      </div>
      {#if trace}
        <h3>Events ({trace.events.length})</h3>
        <table>
          <thead><tr><th>source</th><th>action</th><th>payload</th><th>time</th></tr></thead>
          <tbody>
            {#each trace.events as e (e.id)}
              <tr>
                <td>{e.source}</td><td>{e.action}</td>
                <td class="truncate mono">{JSON.stringify(e.payload)}</td><td>{fmt(e.created_at)}</td>
              </tr>
            {/each}
          </tbody>
        </table>
        <h3>Jobs ({trace.jobs.length})</h3>
        <table>
          <thead><tr><th>id</th><th>status</th><th>prompt</th><th>output</th></tr></thead>
          <tbody>
            {#each trace.jobs as j (j.id)}
              <tr>
                <td class="mono">{short(j.id)}</td>
                <td><span class="badge {j.status}">{j.status}</span></td>
                <td class="truncate">{j.prompt ?? '\u2014'}</td>
                <td class="truncate">{j.output ?? '\u2014'}</td>
              </tr>
            {/each}
          </tbody>
        </table>
        <h3>Messages ({trace.messages.length})</h3>
        <table>
          <thead><tr><th>id</th><th>dir</th><th>content</th><th>time</th></tr></thead>
          <tbody>
            {#each trace.messages as m (m.id)}
              <tr>
                <td class="mono">{short(m.id)}</td>
                <td><span class="badge {m.direction}">{m.direction}</span></td>
                <td class="truncate">{m.content ?? '\u2014'}</td>
                <td>{fmt(m.created_at)}</td>
              </tr>
            {/each}
          </tbody>
        </table>
      {:else if !loading}
        <p class="empty">enter a trace id above</p>
      {/if}
    {/if}
  </main>
</div>

<style>
  :global(body) { margin: 0; background: #0a0a0a; color: #e0e0e0; }

  .shell {
    max-width: 80rem;
    margin: 0 auto;
    padding: 1rem 1.5rem;
    font-family: system-ui, -apple-system, sans-serif;
    font-size: 0.875rem;
  }

  header {
    display: flex;
    align-items: center;
    gap: 2rem;
    margin-bottom: 1rem;
    border-bottom: 1px solid #222;
    padding-bottom: 0.75rem;
  }

  h1 { margin: 0; font-size: 1.25rem; font-weight: 600; color: #fff; }
  h3 { margin: 1.5rem 0 0.5rem; font-size: 0.9rem; color: #999; }

  nav { display: flex; gap: 0.25rem; }
  nav button {
    background: none;
    border: 1px solid transparent;
    color: #888;
    padding: 0.35rem 0.75rem;
    border-radius: 4px;
    cursor: pointer;
    font-size: 0.8rem;
    text-transform: uppercase;
    letter-spacing: 0.05em;
  }
  nav button:hover { color: #ccc; }
  nav button.active { color: #fff; border-color: #444; background: #1a1a1a; }

  .toolbar {
    display: flex;
    gap: 0.5rem;
    margin-bottom: 0.75rem;
  }

  select, input[type="text"] {
    background: #111;
    border: 1px solid #333;
    color: #e0e0e0;
    padding: 0.35rem 0.5rem;
    border-radius: 4px;
    font-size: 0.8rem;
  }
  input[type="text"] { width: 24rem; font-family: monospace; }

  table {
    width: 100%;
    border-collapse: collapse;
    font-size: 0.8rem;
  }
  th {
    text-align: left;
    padding: 0.4rem 0.5rem;
    color: #666;
    font-weight: 500;
    text-transform: uppercase;
    font-size: 0.7rem;
    letter-spacing: 0.05em;
    border-bottom: 1px solid #222;
  }
  td {
    padding: 0.4rem 0.5rem;
    border-bottom: 1px solid #151515;
    vertical-align: top;
  }
  tr:hover td { background: #111; }
  tr.deleted td { opacity: 0.4; text-decoration: line-through; }

  .mono { font-family: monospace; font-size: 0.75rem; color: #888; }
  .truncate { max-width: 20rem; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .err { color: #f55; }
  .empty { text-align: center; color: #444; padding: 2rem 0; }
  .error { background: #2a1010; border: 1px solid #552020; color: #f88; padding: 0.5rem 0.75rem; border-radius: 4px; margin-bottom: 0.75rem; }

  .badge {
    display: inline-block;
    padding: 0.15rem 0.4rem;
    border-radius: 3px;
    font-size: 0.7rem;
    font-weight: 500;
    text-transform: uppercase;
  }
  .badge.draft { background: #1a1a2e; color: #88f; }
  .badge.pending { background: #1a1a2e; color: #88f; }
  .badge.running { background: #0a2a1a; color: #4f4; }
  .badge.paused { background: #2a2a0a; color: #ff8; }
  .badge.done { background: #0a2a0a; color: #4c4; }
  .badge.failed { background: #2a0a0a; color: #f44; }
  .badge.cancelled { background: #1a1a1a; color: #888; }
  .badge.in { background: #0a1a2a; color: #4af; }
  .badge.out { background: #1a0a2a; color: #a4f; }

  button {
    background: #1a1a1a;
    border: 1px solid #333;
    color: #ccc;
    padding: 0.35rem 0.75rem;
    border-radius: 4px;
    cursor: pointer;
    font-size: 0.8rem;
  }
  button:hover { background: #222; }
  button:disabled { opacity: 0.4; cursor: default; }
  button.sm { padding: 0.2rem 0.5rem; font-size: 0.7rem; }
  button.danger { border-color: #533; color: #f88; }
  button.danger:hover { background: #2a1010; }

  .health {
    display: flex;
    gap: 1rem;
    margin-bottom: 0.75rem;
    padding: 0.5rem 0.75rem;
    background: #111;
    border: 1px solid #222;
    border-radius: 4px;
    font-size: 0.75rem;
    color: #666;
  }
  .health .stat b { color: #888; margin-right: 0.2rem; }
  .health .stat.warn b { color: #ff8; }
  .health .stat.alert b { color: #f44; }
</style>
