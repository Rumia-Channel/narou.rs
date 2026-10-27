// WebSocket が使えない環境 (前段 proxy が upgrade を渡さない等) の補完。
//
// 履歴 API を差分取得してコンソールを追い続ける。native は `id` (通しの
// 番号) を返すのでカーソル以降の行だけを追加し、返さない環境 (Worker) は
// 全量を置き換える。
//
// カーソルはストリームごとに持つ。共有すると、片方の取得が失敗した
// ときに、もう片方で未取得の行を飛ばしてしまう。
//
// 取得は 1 つずつ行い、前の取得が終わる前に次の取得を重ねない。終了
// (stop) 後に届いた結果は描画しない。接続が切り替わるときに、古い取得の
// 結果がコンソールへ混ざるのを防ぐため。

export const CONSOLE_HISTORY_POLL_MS = 3000;

export function createConsoleHistoryPoller(options) {
  const fetchJson = options.fetchJson;
  const getTargets = options.getTargets;
  const notify = options.notify || function () {};
  const intervalMs = options.intervalMs || CONSOLE_HISTORY_POLL_MS;

  let timer = null;
  let inFlight = false;
  let generation = 0;
  const cursors = new Map();
  let notified = false;

  async function poll() {
    // 前の取得が終わるまで重ねない (同じ差分を二重に描かない)。
    if (inFlight) return;
    inFlight = true;
    const current = generation;
    try {
      const targets = getTargets() || [];
      if (targets.length === 0) return;
      for (const target of targets) {
        const cursor = cursors.get(target.stream);
        const query = cursor === undefined ? '' : `&after=${cursor}`;
        let data = null;
        try {
          data = await fetchJson(
            `/api/history?format=json&stream=${encodeURIComponent(target.stream)}${query}`,
          );
        } catch {
          data = null;
        }
        // 停止 (stop) 後に届いた結果は使わない。接続が切り替わった直後に
        // 古い結果がコンソールへ混ざると二重表示になる。
        if (current !== generation) return;
        if (!data) continue;
        if (typeof data.id === 'number') {
          // 取得できたストリームのカーソルだけを進める。
          cursors.set(target.stream, data.id);
          if (cursor === undefined) {
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
    } finally {
      inFlight = false;
    }
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
    // 進行中の取得を無効化する。
    generation += 1;
    if (timer) {
      clearInterval(timer);
      timer = null;
    }
    cursors.clear();
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
