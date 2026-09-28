export async function fetchJson(url) {
  const response = await request(url);
  return response.json();
}

export async function postJson(url, body) {
  const response = await request(url, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  return response.json();
}

export async function request(url, options = {}) {
  const response = await fetch(url, options);
  if (!response.ok) {
    const text = await response.text();
    const envelope = parseErrorEnvelope(text);
    if (isAuthenticationRequired(response, envelope)) {
      redirectToLogin();
    }
    throw new Error(envelope.message || `${response.status} ${response.statusText}`);
  }
  return response;
}

/**
 * エラー本文から機械可読コードと表示用メッセージを取り出す。
 * Worker は `{error: {code, message}}`、native は平文を返す。
 */
function parseErrorEnvelope(text) {
  if (!text) return { code: "", message: "" };
  try {
    const parsed = JSON.parse(text);
    const error = parsed && parsed.error;
    const code = error && typeof error.code === "string" ? error.code : "";
    const nested = error && typeof error.message === "string" ? error.message : "";
    const top = parsed && typeof parsed.message === "string" ? parsed.message : "";
    return { code, message: nested || top || text };
  } catch (_) {
    // JSON でない応答 (native の平文エラーなど) はそのまま見せる。
    return { code: "", message: text };
  }
}

function isAuthenticationRequired(response, envelope) {
  return response.status === 401 && envelope.code === "authentication_required";
}

/**
 * Worker の Cookie 認証が切れている / 未ログインのときはログインページへ送る。
 * native は Basic 認証をブラウザが処理するためこの分岐に入らない
 * (`authentication_required` は Worker だけが返すコード)。
 */
function redirectToLogin() {
  if (window.location.pathname === "/login") return;
  const target = window.location.pathname + window.location.search;
  window.location.assign("/login?return=" + encodeURIComponent(target));
}
