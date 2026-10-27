// WebSocket の接続が確立しない環境でもコンソールを追えるようにする。
//
// 接続が失敗 (constructor が例外 / onclose) する obvious な場合は
// Simon 側でも拾えるが、handshake が保留になる proxy のように
// onopen も onclose も来ない場合はfeltNT 何も起きない。そこで接続に
// 時間制限を設け、一定時間確立できなければ履歴取得へ切り替える。

export const WS_CONNECT_TIMEOUT_MS = 5000;

export function createResilientWebSocket(options) {
  const {
    url,
    createSocket,
    setTimeoutImpl = setTimeout,
    clearTimeoutImpl = clearTimeout,
    connectTimeoutMs = WS_CONNECT_TIMEOUT_MS,
    onOpen = () => {},
    onMessage = () => {},
    onClose = () => {},
    onFallbackStart = () => {},
    onFallbackStop = () => {},
  } = options;

  let socket = null;
  let timer = null;
  let fallbackActive = false;

  function startFallback() {
    if (fallbackActive) return;
    fallbackActive = true;
    onFallbackStart();
  }

  function stopFallback() {
    // 履歴取得の停止は接続ごとに持たない状態なので、fallbackActive が
    // false でも通知する。接続が切れて作り直された接続が開いたときも、
    // 前の接続が始めた履歴取得を止められるようにするため。
    fallbackActive = false;
    onFallbackStop();
  }

  function clearTimer() {
    if (timer === null) return;
    clearTimeoutImpl(timer);
    timer = null;
  }

  function connect() {
    clearTimer();
    try {
      socket = createSocket(url);
    } catch {
      startFallback();
      return;
    }

    // 接続が確立しないまま保留になると onopen も onclose も来ないため、
    // 一定時間 连接できていない扱いにし、履歴取得へ切り替える。
    timer = setTimeoutImpl(() => {
      timer = null;
      startFallback();
    }, connectTimeoutMs);

    socket.onopen = () => {
      clearTimer();
      stopFallback();
      onOpen();
    };
    socket.onmessage = (event) => onMessage(event);
    socket.onerror = () => {
      try {
        socket.close();
      } catch { /* already closed */ }
    };
    socket.onclose = (event) => {
      clearTimer();
      startFallback();
      onClose(event);
    };
  }

  return {
    connect,
    close() {
      clearTimer();
      if (socket) {
        try {
          socket.close();
        } catch { /* already closed */ }
      }
    },
    isFallbackActive() {
      return fallbackActive;
    },
  };
}
