// WebSocket が使えない環境 (前段 proxy が upgrade を渡さない等) の補完。
//
// 履歴 API を差分取得してコンソールを追い続ける。native は `id` (通しの
// 番号) を返すのでカーソル以降の行だけを追加し、返さない環境 (Worker) は
// 全量を置き換える。

export const CONSOLE_HISTORY_POLL_MS = 3000;

export function createConsoleHistoryPoller(options) {
  const fetchJson = options.fetchJson;
  const getTargets = options.getTargets;
  const notify = options.notify || function () {};
  const intervalMs = options.intervalMs || CONSOLE_HISTORY_POLL_MS;

  let timer = null;
  let cursor = null;
  let notified = false;

  async function poll() {
    const targets = getTargets() || [];
    if (targets.length === 0) return;
    let nextCursor = null;
    for (const target of targets) {
      const query = cursor === null ? '' : `&after=${cursor}`;
      let data = null;
      try {
        data = await fetchJson(
          `/api/history?format=json&stream=${encodeURIComponent(target.stream)}${query}`,
        );
      } catch {
        data = null;
      }
      if (!data) continue;
      if (typeof data.id === 'number') {
        if (nextCursor === null) nextCursor = data.id;
        if (cursor === null) {
          // 最初は全量。(次の取得からは差分だけ追記する)
          target.replace(data.history || '');
        } else if (data.history) {
          target.append(data.history || '');
        }
      } else {
        // 差分取得に対応しない環境 (Worker) は全量を置き換える。
        target.replace(data.history || '');
      }
    }
    if (nextCursor !== null) cursor = nextCursor;
  }

  function start() {
    if (!notified) {
      notified = true;
      notify();
    }
    if (timer) return;
    timer = setInterval(poll, intervalMs);
    void poll();
  }

  function stop() {
    if (!timer) return;
    clearInterval(timer);
    timer = null;
    cursor = null;
    notified = false;
  }

  return {
    start,
    stop,
    poll,
    isRunning() {
      return timer !== null;
    },
  };
}
