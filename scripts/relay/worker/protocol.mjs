// Node / Worker の取得リレーが共有する JSON・ヘッダ・本文の境界規則。
export const MAX_BODY_BYTES = 16 * 1024 * 1024;
export const MAX_REQUEST_BYTES = 64 * 1024;
export const MAX_HEAD_BYTES = 64 * 1024;
export const MAX_METADATA_BYTES = 6 * MAX_HEAD_BYTES + 64 * 1024;

const utf8 = new TextDecoder("utf-8", { fatal: true });
const utf8Encoder = new TextEncoder();

// Rust 側の wire cap と同じ予算: body の base64 文字だけを除き、
// JSON のキー・引用符・escape・空の body フィールドはすべて数える。
export const assertResponseMetadata = (metadata) => {
  if (!Number.isInteger(metadata.status) || metadata.status < 200 || metadata.status > 599) {
    throw new Error("upstream did not return a final HTTP response");
  }
  const envelope = JSON.stringify({ ...metadata, body: "" });
  if (envelope.length > MAX_METADATA_BYTES
    || utf8Encoder.encode(envelope).byteLength > MAX_METADATA_BYTES) {
    throw new Error(`response metadata too large: limit ${MAX_METADATA_BYTES}`);
  }
};

export const parseJsonObject = (bytes) => {
  const value = JSON.parse(utf8.decode(bytes));
  if (value === null || typeof value !== "object" || Array.isArray(value)) {
    throw new Error("expected JSON object");
  }
  return value;
};

export const decodeHeaderJson = (raw) => {
  if (raw.length > 4 * Math.ceil(MAX_REQUEST_BYTES / 3)) {
    throw new Error("request too large");
  }
  if (!/^[A-Za-z0-9+/]*={0,2}$/.test(raw)) {
    throw new Error("headers must be base64 JSON");
  }
  const binary = atob(raw);
  if (btoa(binary).replace(/=+$/, "") !== raw.replace(/=+$/, "")) {
    throw new Error("headers must be base64 JSON");
  }
  if (binary.length > MAX_REQUEST_BYTES) throw new Error("request too large");
  const bytes = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i += 1) bytes[i] = binary.charCodeAt(i);
  return parseJsonObject(bytes);
};

// 同名ヘッダの配列を、受けた順の [name, value] に開く。
export const headerPairs = (raw) => {
  const pairs = [];
  for (const [name, value] of Object.entries(raw || {})) {
    if (typeof value === "string") {
      pairs.push([name, value]);
    } else if (Array.isArray(value)) {
      for (const item of value) {
        if (typeof item === "string") pairs.push([name, item]);
      }
    }
  }
  return pairs;
};

export const readBoundedBytes = async (
  stream,
  { limit, contentLength = null, verifyLength = true, label = "body", cancelOnLimit = true },
) => {
  const declared = contentLength !== null && /^\d+$/.test(String(contentLength))
    ? Number(contentLength)
    : null;
  const reader = stream?.getReader();
  try {
    const tooLarge = async () => {
      const error = new Error(`${label} too large: limit ${limit}`);
      if (reader && cancelOnLimit) await reader.cancel().catch(() => {});
      throw error;
    };
    if (declared !== null && declared > limit) await tooLarge();

    const chunks = [];
    let length = 0;
    if (reader) {
      for (;;) {
        const { done, value } = await reader.read();
        if (done) break;
        length += value.byteLength;
        if (length > limit) await tooLarge();
        chunks.push(value);
      }
    }
    if (verifyLength && declared !== null && length !== declared) {
      throw new Error(`${label} truncated: expected ${declared} bytes, received ${length}`);
    }
    const bytes = new Uint8Array(length);
    let offset = 0;
    for (const chunk of chunks) {
      bytes.set(chunk, offset);
      offset += chunk.byteLength;
    }
    return bytes;
  } finally {
    reader?.releaseLock();
  }
};

export const readResponseBytes = (response) => {
  const encoding = response.headers.get("content-encoding");
  return readBoundedBytes(response.body, {
    limit: MAX_BODY_BYTES,
    contentLength: response.headers.get("content-length"),
    // fetch が展開した圧縮本文のサイズは wire の Content-Length と一致しない。
    verifyLength: (!encoding || encoding.toLowerCase() === "identity")
      && response.status !== 204 && response.status !== 304,
    label: "response",
  });
};
