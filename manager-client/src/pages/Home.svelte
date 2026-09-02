<script lang="ts">
  import { onMount } from "svelte";
  import { McpHttpClient, extractToolPayload, type RepoSummary } from "../lib/mcp";

  export let onNavigate: (path: string) => void;

  const ENDPOINT_KEY = "weaver.endpoint";
  let endpoint = sessionStorage.getItem(ENDPOINT_KEY) ?? "http://127.0.0.1:8444/mcp";
  let status = "idle"; // idle | connecting | connected | error
  let errorMsg = "";
  let repos: RepoSummary[] = [];
  let client: McpHttpClient | null = null;

  // Index-new-repo state
  let newRepoPath = "";
  let indexState: "idle" | "running" | "done" | "error" | "cancelled" = "idle";
  let indexMsg = "";
  let indexJobId: string | null = null;
  let indexPollTimer: ReturnType<typeof setInterval> | null = null;

  // Per-repo re-index state keyed by repo id
  type ReindexState = {
    state: "idle" | "running" | "done" | "error";
    msg: string;
    timer: ReturnType<typeof setInterval> | null;
  };
  let reindexStates: Record<string, ReindexState> = {};

  // Which repo has delete confirmation shown
  let confirmDeleteId: string | null = null;
  let deletingId: string | null = null;
  let deleteError: string | null = null;

  // -------------------------------------------------------------------------

  onMount(() => {
    connect(true);

    const onVisible = () => {
      if (document.visibilityState === "visible" && status === "connected") {
        void refreshRepos();
      }
    };
    document.addEventListener("visibilitychange", onVisible);

    return () => {
      document.removeEventListener("visibilitychange", onVisible);
      stopIndexPoll();
    };
  });

  async function connect(silent = false) {
    status = "connecting";
    if (!silent) errorMsg = "";
    repos = [];
    client?.disconnect();

    try {
      sessionStorage.setItem(ENDPOINT_KEY, endpoint);
      const next = new McpHttpClient(endpoint);
      await next.initialize("weaver-manager");
      repos = await next.listRepos();
      client = next;
      status = "connected";
    } catch (err) {
      if (!silent) errorMsg = err instanceof Error ? err.message : String(err);
      status = silent ? "idle" : "error";
    }
  }

  // ---- index new repo -------------------------------------------------------

  async function indexRepo() {
    if (!client || !newRepoPath.trim()) return;
    stopIndexPoll();
    indexState = "running";
    indexMsg = "Starting…";
    indexJobId = null;

    try {
      const raw = await client.callTool("ingest_symbols", {
        repo_path: newRepoPath.trim(),
      });
      const text = payloadText(raw);

      const m = text.match(/job[_\s-]?id[:\s]+([a-f0-9-]{36})/i);
      if (m) {
        indexJobId = m[1]!;
        indexMsg = `job ${indexJobId.slice(0, 8)}… running`;
        await refreshRepos(); // repo record exists now; show it immediately
        startIndexPoll();
      } else {
        indexMsg = text.slice(0, 160);
        indexState = "done";
        await refreshRepos();
      }
    } catch (err) {
      indexMsg = err instanceof Error ? err.message : String(err);
      indexState = "error";
    }
  }

  async function cancelIndex() {
    if (!client || !indexJobId) return;
    try {
      await client.callTool("cancel_ingest", { job_id: indexJobId });
    } catch {
      // ignore — poll will pick up cancellation state
    }
  }

  function startIndexPoll() {
    indexPollTimer = setInterval(async () => {
      if (!client || !indexJobId) return;
      try {
        const text = payloadText(
          await client.callTool("ingest_symbols_status", { job_id: indexJobId }),
        );
        indexMsg = text.slice(0, 160);
        if (text.includes("status=done")) {
          indexState = "done";
          stopIndexPoll();
          await refreshRepos();
        } else if (text.includes("status=cancelled")) {
          indexState = "cancelled";
          stopIndexPoll();
          await refreshRepos();
        }
      } catch {
        // ignore transient poll errors
      }
    }, 2000);
  }

  function stopIndexPoll() {
    if (indexPollTimer !== null) {
      clearInterval(indexPollTimer);
      indexPollTimer = null;
    }
  }

  // ---- per-repo re-index (incremental, git-diff based) ----------------------

  async function reindexRepo(repo: RepoSummary) {
    if (!client) return;
    const prev = reindexStates[repo.id];
    if (prev?.timer) clearInterval(prev.timer);

    reindexStates = {
      ...reindexStates,
      [repo.id]: { state: "running", msg: "Syncing changed files…", timer: null },
    };

    try {
      // sync_incremental only re-indexes files changed since the last ingest —
      // much faster than a full walk and naturally incremental + interruptible
      // from a workflow perspective (just call it again to pick up more changes).
      const text = payloadText(
        await client.callTool("sync_incremental", { repo_path: repo.path }),
      );
      reindexStates = {
        ...reindexStates,
        [repo.id]: { state: "done", msg: text.slice(0, 160), timer: null },
      };
      await refreshRepos();
    } catch (err) {
      reindexStates = {
        ...reindexStates,
        [repo.id]: {
          state: "error",
          msg: err instanceof Error ? err.message : String(err),
          timer: null,
        },
      };
    }
  }

  function dismissReindex(repoId: string) {
    const s = reindexStates[repoId];
    if (s?.timer) clearInterval(s.timer);
    const next = { ...reindexStates };
    delete next[repoId];
    reindexStates = next;
  }

  // ---- delete ---------------------------------------------------------------

  async function deleteRepo(repo: RepoSummary) {
    if (!client) return;
    deletingId = repo.id;
    deleteError = null;

    try {
      const raw = await client.callTool("delete_repo", { repo_path: repo.path });
      const payload = extractToolPayload(raw);
      const deleted =
        typeof payload === "object" &&
        payload !== null &&
        (payload as Record<string, unknown>).deleted === true;

      if (deleted) {
        confirmDeleteId = null;
        await refreshRepos();
      } else {
        deleteError = "Server reported repo not found.";
      }
    } catch (err) {
      deleteError = err instanceof Error ? err.message : String(err);
    } finally {
      deletingId = null;
    }
  }

  // ---- shared ---------------------------------------------------------------

  async function refreshRepos() {
    if (!client) return;
    try {
      repos = await client.listRepos();
    } catch {
      // ignore
    }
  }

  function goToRepo(repo: RepoSummary) {
    sessionStorage.setItem("weaver.endpoint", endpoint);
    onNavigate(`/repo/${encodeURIComponent(repo.path)}`);
  }

  function payloadText(raw: unknown): string {
    const p = extractToolPayload(raw);
    return typeof p === "string" ? p : JSON.stringify(p);
  }

  function shortPath(path: string): string {
    const parts = path.replace(/\\/g, "/").split("/");
    return parts.slice(-2).join("/");
  }

  function formatDate(iso: string): string {
    try {
      return new Date(iso).toLocaleDateString(undefined, {
        year: "numeric",
        month: "short",
        day: "numeric",
      });
    } catch {
      return iso;
    }
  }
</script>

<div class="home">
  <header class="hero">
    <p class="eyebrow">Architecture Memory</p>
    <h1>Weaver</h1>
    <p class="tagline">
      Connect to a running daemon to browse repositories and inspect
      architectural decisions.
    </p>
  </header>

  <!-- Connection card -->
  <section class="connect-card card">
    <h2>Daemon endpoint</h2>
    <div class="connect-row">
      <input
        bind:value={endpoint}
        placeholder="http://127.0.0.1:8444/mcp"
        on:keydown={(e) => e.key === "Enter" && connect()}
      />
      <button
        class="btn-primary"
        on:click={() => connect()}
        disabled={status === "connecting"}
      >
        {status === "connecting" ? "Connecting…" : "Connect"}
      </button>
    </div>

    {#if status === "error"}
      <p class="pill pill-error">{errorMsg}</p>
    {:else if status === "connected"}
      <p class="pill pill-ok">
        Connected · {repos.length}
        {repos.length === 1 ? "repo" : "repos"} found
      </p>
    {/if}
  </section>

  {#if status === "connected"}
    <!-- Index new repo card -->
    <section class="index-card card">
      <h2>Index a repository</h2>
      <div class="index-row">
        <input
          bind:value={newRepoPath}
          placeholder="/absolute/path/to/repo"
          disabled={indexState === "running"}
          on:keydown={(e) =>
            e.key === "Enter" && indexState !== "running" && indexRepo()}
        />
        <button
          class="btn-primary"
          on:click={indexRepo}
          disabled={indexState === "running" || !newRepoPath.trim()}
        >
          {indexState === "running" ? "Indexing…" : "Index"}
        </button>
      </div>

      {#if indexState !== "idle"}
        <div
          class="job-status"
          class:done={indexState === "done"}
          class:cancelled={indexState === "cancelled"}
          class:error={indexState === "error"}
        >
          <div class="job-status-head">
            {#if indexState === "running"}
              <span class="spinner" aria-hidden="true"></span>
            {:else if indexState === "done"}
              <span class="job-icon ok">✓</span>
            {:else if indexState === "cancelled"}
              <span class="job-icon cancelled">⊘</span>
            {:else}
              <span class="job-icon err">✗</span>
            {/if}
            <span class="job-label">
              {indexState === "running"
                ? "Indexing"
                : indexState === "done"
                  ? "Done"
                  : indexState === "cancelled"
                    ? "Cancelled"
                    : "Error"}
            </span>
            {#if indexState === "running"}
              <button class="btn-cancel-job" on:click={cancelIndex}>Cancel</button>
            {:else}
              <button
                class="job-dismiss"
                on:click={() => { indexState = "idle"; indexMsg = ""; }}>×</button>
            {/if}
          </div>
          {#if indexMsg}<p class="job-msg">{indexMsg}</p>{/if}
        </div>
      {/if}
    </section>

    <!-- Repositories list -->
    <section class="repos-section">
      <div class="repos-header">
        <h2>
          Repositories
          {#if repos.length > 0}<span class="repo-count">{repos.length}</span>{/if}
        </h2>
        <button class="btn-refresh" on:click={refreshRepos} title="Refresh repo list"
          >↺ Refresh</button
        >
      </div>

      {#if repos.length > 0}
        <div class="repo-list">
          {#each repos as repo (repo.id)}
            <div
              class="repo-card card"
              class:confirming={confirmDeleteId === repo.id}
            >
              <!-- Main info row -->
              <div class="repo-main">
                <div class="repo-icon">⬡</div>
                <div class="repo-info">
                  <p class="repo-name">{repo.name ?? shortPath(repo.path)}</p>
                  <p class="repo-path" title={repo.path}>{repo.path}</p>
                  <p class="repo-date">Last indexed {formatDate(repo.ingested_at)}</p>
                </div>
                <div class="repo-actions">
                  <button
                    class="btn-open"
                    on:click={() => goToRepo(repo)}
                    title="Open workspace"
                  >Open →</button>
                  <button
                    class="btn-reindex"
                    disabled={reindexStates[repo.id]?.state === "running"}
                    on:click={() => reindexRepo(repo)}
                    title="Re-index symbols"
                  >
                    {reindexStates[repo.id]?.state === "running"
                      ? "…"
                      : "Re-index"}
                  </button>
                  <button
                    class="btn-delete"
                    disabled={deletingId === repo.id}
                    on:click={() => {
                      confirmDeleteId =
                        confirmDeleteId === repo.id ? null : repo.id;
                      deleteError = null;
                    }}
                    title="Delete repository"
                  >Delete</button>
                </div>
              </div>

              <!-- Per-repo re-index progress -->
              {#if reindexStates[repo.id] && reindexStates[repo.id]!.state !== "idle"}
                {@const rs = reindexStates[repo.id]!}
                <div
                  class="job-status"
                  class:done={rs.state === "done"}
                  class:error={rs.state === "error"}
                >
                  <div class="job-status-head">
                    {#if rs.state === "running"}
                      <span class="spinner" aria-hidden="true"></span>
                    {:else if rs.state === "done"}
                      <span class="job-icon ok">✓</span>
                    {:else}
                      <span class="job-icon err">✗</span>
                    {/if}
                    <span class="job-label">
                      {rs.state === "running"
                        ? "Re-indexing"
                        : rs.state === "done"
                          ? "Done"
                          : "Error"}
                    </span>
                    {#if rs.state !== "running"}
                      <button
                        class="job-dismiss"
                        on:click={() => dismissReindex(repo.id)}>×</button
                      >
                    {/if}
                  </div>
                  {#if rs.msg}<p class="job-msg">{rs.msg}</p>{/if}
                </div>
              {/if}

              <!-- Inline delete confirmation -->
              {#if confirmDeleteId === repo.id}
                <div class="delete-confirm">
                  <p class="delete-confirm-msg">
                    Permanently delete all indexed data for this repository?
                    <strong>This cannot be undone.</strong>
                  </p>
                  {#if deleteError}
                    <p class="delete-error">{deleteError}</p>
                  {/if}
                  <div class="delete-confirm-actions">
                    <button
                      class="btn-delete-confirm"
                      disabled={deletingId === repo.id}
                      on:click={() => deleteRepo(repo)}
                    >
                      {deletingId === repo.id ? "Deleting…" : "Yes, delete"}
                    </button>
                    <button
                      class="btn-cancel"
                      on:click={() => {
                        confirmDeleteId = null;
                        deleteError = null;
                      }}>Cancel</button
                    >
                  </div>
                </div>
              {/if}
            </div>
          {/each}
        </div>
      {:else}
        <div class="empty-state card">
          <p>No repositories indexed yet.</p>
          <p class="muted">Enter a path above to start indexing.</p>
        </div>
      {/if}
    </section>
  {/if}
</div>

<style>
  .home {
    max-width: 960px;
    margin: 0 auto;
    padding: 2rem 1.5rem 4rem;
  }

  .hero {
    text-align: center;
    margin-bottom: 2.5rem;
  }

  .hero h1 {
    margin: 0.2rem 0;
    font-size: clamp(3rem, 6vw, 5rem);
    letter-spacing: -0.05em;
    background: linear-gradient(135deg, var(--cyan), #b7fff5 60%, var(--amber));
    -webkit-background-clip: text;
    -webkit-text-fill-color: transparent;
    background-clip: text;
  }

  .tagline {
    color: var(--muted);
    max-width: 480px;
    margin: 0.6rem auto 0;
    line-height: 1.6;
  }

  /* ---- shared card shell ---- */
  .card {
    background: var(--panel);
    border: 1px solid var(--panel-border);
    border-radius: 24px;
    padding: 1.5rem;
    backdrop-filter: blur(20px);
    box-shadow: var(--shadow);
  }

  /* ---- connect card ---- */
  .connect-card h2 {
    margin: 0 0 1rem;
    font-size: 0.78rem;
    text-transform: uppercase;
    letter-spacing: 0.12em;
    color: var(--muted);
  }

  .connect-row {
    display: flex;
    gap: 0.75rem;
  }

  .connect-row input {
    flex: 1;
  }

  .btn-primary {
    white-space: nowrap;
  }

  .pill {
    margin-top: 0.75rem;
    display: inline-block;
    padding: 0.4rem 0.85rem;
    border-radius: 999px;
    font-size: 0.88rem;
  }

  .pill-ok {
    background: rgba(137, 240, 167, 0.14);
    color: var(--success);
  }

  .pill-error {
    background: rgba(244, 132, 95, 0.14);
    color: var(--coral);
  }

  /* ---- index card ---- */
  .index-card {
    margin-top: 1.5rem;
  }

  .index-card h2 {
    margin: 0 0 1rem;
    font-size: 0.78rem;
    text-transform: uppercase;
    letter-spacing: 0.12em;
    color: var(--muted);
  }

  .index-row {
    display: flex;
    gap: 0.75rem;
  }

  .index-row input {
    flex: 1;
    font-family: "Cascadia Code", "Fira Code", monospace;
    font-size: 0.88rem;
  }

  /* ---- repos section ---- */
  .repos-section {
    margin-top: 2rem;
  }

  .repos-header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    margin-bottom: 1rem;
  }

  .repos-header h2 {
    margin: 0;
    font-size: 0.78rem;
    text-transform: uppercase;
    letter-spacing: 0.12em;
    color: var(--muted);
    display: flex;
    align-items: center;
    gap: 0.5rem;
  }

  .repo-count {
    background: rgba(110, 231, 255, 0.12);
    color: var(--cyan);
    font-size: 0.72rem;
    padding: 0.1rem 0.45rem;
    border-radius: 999px;
    letter-spacing: 0;
    font-weight: 600;
  }

  .btn-refresh {
    background: transparent;
    border: 1px solid rgba(255, 255, 255, 0.08);
    border-radius: 8px;
    color: var(--muted);
    font-size: 0.78rem;
    padding: 0.3rem 0.7rem;
    cursor: pointer;
    box-shadow: none;
  }

  .btn-refresh:hover {
    border-color: rgba(110, 231, 255, 0.3);
    color: var(--cyan);
  }

  .repo-list {
    display: flex;
    flex-direction: column;
    gap: 0.75rem;
  }

  /* ---- repo card ---- */
  .repo-card {
    border-radius: 20px;
    padding: 1.1rem 1.2rem;
    transition: border-color 180ms ease;
  }

  .repo-card:hover {
    border-color: rgba(110, 231, 255, 0.35);
  }

  .repo-card.confirming {
    border-color: rgba(244, 132, 95, 0.45);
  }

  .repo-main {
    display: flex;
    align-items: center;
    gap: 1rem;
  }

  .repo-icon {
    font-size: 1.6rem;
    line-height: 1;
    color: var(--cyan);
    flex-shrink: 0;
  }

  .repo-info {
    flex: 1;
    overflow: hidden;
  }

  .repo-name {
    margin: 0 0 0.15rem;
    font-weight: 600;
    font-size: 0.97rem;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }

  .repo-path {
    margin: 0 0 0.25rem;
    font-size: 0.76rem;
    color: var(--muted);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
    font-family: "Cascadia Code", "Fira Code", monospace;
  }

  .repo-date {
    margin: 0;
    font-size: 0.76rem;
    color: var(--amber);
  }

  /* ---- per-card action buttons ---- */
  .repo-actions {
    display: flex;
    gap: 0.4rem;
    flex-shrink: 0;
    align-items: center;
  }

  .btn-open,
  .btn-reindex,
  .btn-delete {
    font-size: 0.78rem;
    border-radius: 8px;
    padding: 0.35rem 0.7rem;
    box-shadow: none;
    white-space: nowrap;
    cursor: pointer;
  }

  .btn-open {
    background: rgba(110, 231, 255, 0.12);
    border: 1px solid rgba(110, 231, 255, 0.3);
    color: var(--cyan);
    font-weight: 600;
  }

  .btn-open:hover {
    background: rgba(110, 231, 255, 0.2);
  }

  .btn-reindex {
    background: rgba(255, 255, 255, 0.04);
    border: 1px solid rgba(255, 255, 255, 0.08);
    color: var(--muted);
  }

  .btn-reindex:hover:not(:disabled) {
    border-color: rgba(137, 240, 167, 0.35);
    color: #89f0a7;
  }

  .btn-reindex:disabled {
    opacity: 0.5;
    cursor: default;
  }

  .btn-delete {
    background: rgba(255, 255, 255, 0.03);
    border: 1px solid rgba(255, 255, 255, 0.07);
    color: var(--muted);
  }

  .btn-delete:hover:not(:disabled) {
    border-color: rgba(244, 132, 95, 0.4);
    color: var(--coral);
  }

  .btn-delete:disabled {
    opacity: 0.5;
    cursor: default;
  }

  /* ---- inline delete confirmation ---- */
  .delete-confirm {
    margin-top: 0.9rem;
    padding: 0.85rem 1rem;
    border-radius: 14px;
    background: rgba(244, 132, 95, 0.07);
    border: 1px solid rgba(244, 132, 95, 0.3);
  }

  .delete-confirm-msg {
    margin: 0 0 0.65rem;
    font-size: 0.84rem;
    color: var(--text);
    line-height: 1.5;
  }

  .delete-confirm-msg strong {
    color: var(--coral);
  }

  .delete-error {
    margin: 0 0 0.5rem;
    font-size: 0.8rem;
    color: var(--coral);
  }

  .delete-confirm-actions {
    display: flex;
    gap: 0.5rem;
  }

  .btn-delete-confirm {
    background: rgba(244, 132, 95, 0.15);
    border: 1px solid rgba(244, 132, 95, 0.45);
    color: var(--coral);
    font-size: 0.82rem;
    font-weight: 600;
    padding: 0.4rem 0.85rem;
    border-radius: 8px;
    cursor: pointer;
    box-shadow: none;
  }

  .btn-delete-confirm:hover:not(:disabled) {
    background: rgba(244, 132, 95, 0.25);
  }

  .btn-delete-confirm:disabled {
    opacity: 0.6;
    cursor: default;
  }

  .btn-cancel {
    background: transparent;
    border: 1px solid rgba(255, 255, 255, 0.1);
    color: var(--muted);
    font-size: 0.82rem;
    padding: 0.4rem 0.85rem;
    border-radius: 8px;
    cursor: pointer;
    box-shadow: none;
  }

  .btn-cancel:hover {
    border-color: rgba(255, 255, 255, 0.2);
    color: var(--text);
  }

  /* ---- shared job status block ---- */
  .job-status {
    margin-top: 0.75rem;
    padding: 0.6rem 0.8rem;
    border-radius: 12px;
    background: rgba(2, 8, 14, 0.7);
    border: 1px solid rgba(137, 240, 167, 0.25);
  }

  .job-status.done {
    border-color: rgba(137, 240, 167, 0.45);
  }

  .job-status.cancelled {
    border-color: rgba(183, 163, 210, 0.4);
  }

  .job-status.error {
    border-color: rgba(244, 132, 95, 0.4);
  }

  .job-status-head {
    display: flex;
    align-items: center;
    gap: 0.4rem;
  }

  .job-label {
    font-size: 0.82rem;
    font-weight: 600;
    color: #89f0a7;
    flex: 1;
  }

  .job-status.error .job-label {
    color: var(--coral);
  }

  .job-status.cancelled .job-label {
    color: #b7a3d2;
  }

  .job-icon {
    font-size: 0.9rem;
  }

  .job-icon.ok {
    color: #89f0a7;
  }

  .job-icon.err {
    color: var(--coral);
  }

  .job-icon.cancelled {
    color: #b7a3d2;
  }

  .btn-cancel-job {
    background: transparent;
    border: 1px solid rgba(255, 255, 255, 0.12);
    border-radius: 6px;
    color: var(--muted);
    font-size: 0.72rem;
    padding: 0.15rem 0.5rem;
    cursor: pointer;
    box-shadow: none;
    line-height: 1.4;
  }

  .btn-cancel-job:hover {
    border-color: rgba(244, 132, 95, 0.4);
    color: var(--coral);
  }

  .job-dismiss {
    background: transparent;
    border: none;
    color: var(--muted);
    font-size: 1rem;
    padding: 0;
    cursor: pointer;
    line-height: 1;
    box-shadow: none;
  }

  .job-msg {
    margin: 0.3rem 0 0;
    font-size: 0.75rem;
    font-family: "Cascadia Code", "Fira Code", monospace;
    color: var(--muted);
    white-space: pre-wrap;
    word-break: break-all;
  }

  /* ---- empty state ---- */
  .empty-state {
    margin-top: 0;
    text-align: center;
    color: var(--muted);
  }

  .muted {
    color: var(--muted);
  }

  /* ---- spinner ---- */
  @keyframes spin {
    to {
      transform: rotate(360deg);
    }
  }

  .spinner {
    display: inline-block;
    width: 12px;
    height: 12px;
    border: 2px solid rgba(137, 240, 167, 0.3);
    border-top-color: #89f0a7;
    border-radius: 50%;
    animation: spin 0.8s linear infinite;
    flex-shrink: 0;
  }

  @media (max-width: 640px) {
    .repo-main {
      flex-wrap: wrap;
    }

    .repo-actions {
      width: 100%;
      justify-content: flex-end;
    }
  }
</style>
