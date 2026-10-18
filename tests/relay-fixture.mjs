// scripts/relay のビルドコンテキスト外。実 HTTP 契約試験だけが明示的に注入する。
// relay.invalid は通常 DNS では解決できず、検証済み IP の固定が外れると試験も失敗する。
export const resolveLocalTarget = async (raw) => {
  const url = new URL(raw);
  if (!['http:', 'https:'].includes(url.protocol) || url.username || url.password
    || !['127.0.0.1', 'relay.invalid'].includes(url.hostname)) throw new Error("invalid local fixture URL");
  return { url, address: "127.0.0.1", family: 4 };
};
