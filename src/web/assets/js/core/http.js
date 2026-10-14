export async function fetchJson(url, options = {}) {
  const response = await request(url, options);
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
    const error = new Error(envelope.message || `${response.status} ${response.statusText}`);
    error.status = response.status;
    error.data = envelope.data;
    error.code = envelope.code;
    throw error;
  }
  return response;
}

export function assertApiSuccess(result, fallbackMessage = "操作に失敗しました") {
  if (result && typeof result === "object" && result.success === false) {
    const error = new Error(result.error?.message || result.message
      || (typeof result.error === "string" ? result.error : "") || fallbackMessage);
    error.data = result;
    error.code = result.error?.code || result.code || "";
    throw error;
  }
  return result;
}

/**
 * エラー本文から機械可読コードと表示用メッセージを取り出す。
 * Worker は `{error: {code, message}}`、native は平文を返す。
 */
function parseErrorEnvelope(text) {
  if (!text) return { code: "", message: "", data: null };
  try {
    const parsed = JSON.parse(text);
    const error = parsed && parsed.error;
    const code = error && typeof error.code === "string" ? error.code : "";
    const nested = error && typeof error.message === "string" ? error.message : "";
    const top = parsed && typeof parsed.message === "string" ? parsed.message : "";
    const plainError = typeof error === "string" ? error : "";
    return { code, message: nested || top || plainError || text, data: parsed };
  } catch (_) {
    // JSON でない応答 (native の平文エラーなど) はそのまま見せる。
    return { code: "", message: text, data: text };
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
