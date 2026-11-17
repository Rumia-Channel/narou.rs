/**
 * クライアント主体転送の ESM Dedicated Worker。
 *
 * このファイルは単体で動作する必要がある。実行時に読み込む外部コードは
 * artifact manifest (/assets/transfer/manifest.json) が指す wasm-bindgen glue
 * URL だけで、その glue が init 用 WASM URL を受け取る。サーバー製 ZIP との
 * 互換経路は一切持たない。
 *
 * ネットワーク・WASM・再開状態はすべてこの Worker 内で扱い、UI スレッドには
 * 進捗・確認要求・保存先への bounded chunk だけを postMessage する。
 * バイトは transferable ArrayBuffer で受け渡しし、File は structured clone で
 * 受け取る。ファイル/ZIP 全体を一度に ArrayBuffer 化しない。
 *
 * Wire/WASM contract: docs/client-transfer-design.md (/api/transfer/v2)。
 *
 * Node のテストから純粋関数を import できるよう、Worker 側の起動処理は
 * DedicatedWorkerGlobalScope 内だけで実行する。
 */

export const PROTOCOL_VERSION = 2;
export const ABI_VERSION = 2;

/** 設計 §5 と capabilities.limits のキーに対応する固定値。 */
export const LIMITS = Object.freeze({
  /** フレーム込みのバイナリ HTTP 本文上限 (binary_body_bytes)。 */
  binaryBodyBytes: 8 * 1024 * 1024,
  /** 1 file record payload 上限 (chunk_bytes = binary - 64 KiB)。 */
  chunkBytes: 8 * 1024 * 1024 - 64 * 1024,
  /** codec/JS-WASM 境界の最大 I/O (codec_io_bytes)。 */
  codecIoBytes: 64 * 1024,
  /** 1 リクエストの最大レコード数 (batch_records)。 */
  maxRecordsPerBatch: 100,
  /** レコードヘッダ JSON の最大バイト数 (record_header_bytes)。 */
  recordHeaderBytes: 512,
  /** 制御 JSON 本文の上限 (control_body_bytes)。 */
  controlJsonBytes: 256 * 1024,
  /** エラー応答を読み取る上限。 */
  errorBodyBytes: 16 * 1024,
  /** wire/export manifest の直列化上限 (manifest_bytes)。 */
  manifestBytes: 64 * 1024 * 1024,
  /** WASM が保持するメタデータ予算 (manifest_metadata_bytes)。 */
  metadataBytes: 24 * 1024 * 1024,
  /** catalog/files/plan ページの項目数上限 (page_items)。 */
  pageItems: 100,
});

/** manifest chunk が使う予約 file ID (u32::MAX)。 */
export const MANIFEST_FILE_ID = 0xFFFFFFFF;

const API_PREFIX = '/api/transfer/v2';
const SECRET_HEADER = 'x-narou-transfer-secret';
const MAX_CONTROL_ATTEMPTS = 6;
const SESSION_DB = 'narou-transfer';
const SESSION_STORE = 'sessions';
const SPOOL_DIR = 'narou-transfer';

/** FileRole serde names → 表示名。 */
const ROLE_LABELS = Object.freeze({
  body: '本文',
  toc: '目次',
  metadata: '作品情報',
  settings: '変換設定',
  illustration: '挿絵',
  illustration_index: '挿絵索引',
  raw: '取得時の raw データ',
  history: '旧本文・差分履歴',
  generated: '再生成可能な出力',
  unknown: '分類できないファイル',
  internal: '内部データ',
});

const ROLE_ORDER = ['body', 'toc', 'metadata', 'settings', 'illustration',
  'illustration_index', 'raw', 'history', 'generated', 'unknown', 'internal'];

export function roleLabel(role) {
  return ROLE_LABELS[role] || `その他 (${String(role)})`;
}

// ---------------------------------------------------------------------------
// 基本ユーティリティ
// ---------------------------------------------------------------------------

export class TransferError extends Error {
  constructor(code, message, options = {}) {
    super(message);
    this.name = 'TransferError';
    this.code = code;
    this.httpStatus = options.httpStatus ?? 0;
    this.retryable = options.retryable === true;
    this.html = options.html === true;
    this.revision = options.revision ?? '';
    this.retryAfterMs = options.retryAfterMs ?? 0;
    this.wasm = options.wasm === true;
  }
}

/** WASM の reject は "prefix: detail" の機械安定メッセージを持つ。 */
export function classifyWasmError(error) {
  const message = String(error?.message || error);
  const prefix = message.includes(':') ? message.slice(0, message.indexOf(':')) : '';
  const known = ['invalid_archive', 'hash_mismatch', 'budget', 'truncated',
    'unsupported', 'invalid_selection', 'closed_handle', 'cancelled'];
  const code = known.includes(prefix) ? `wasm_${prefix}` : 'wasm_error';
  return new TransferError(code, message, { wasm: true });
}

/** 10 進文字列への変換。byte/offset は JSON number を信用しない。 */
export function decimal(value) {
  if (typeof value === 'bigint') return value.toString(10);
  if (typeof value === 'number' && Number.isSafeInteger(value) && value >= 0) return BigInt(value).toString(10);
  throw new TransferError('protocol_error', `10 進整数に変換できない値: ${String(value)}`);
}

export function parseDecimal(value, fallback = 0n) {
  if (typeof value === 'bigint') return value >= 0n ? value : fallback;
  if (typeof value === 'number') {
    return Number.isSafeInteger(value) && value >= 0 ? BigInt(value) : fallback;
  }
  if (typeof value === 'string' && /^\d+$/.test(value)) return BigInt(value);
  return fallback;
}

export function bytesText(value) {
  const bytes = typeof value === 'bigint' ? value : parseDecimal(value, 0n);
  const mib = 1024n * 1024n;
  if (bytes >= mib) return `${(Number(bytes) / Number(mib)).toFixed(1)} MiB`;
  if (bytes >= 1024n) return `${(Number(bytes) / 1024).toFixed(1)} KiB`;
  return `${bytes} B`;
}

export function base64UrlEncode(bytes) {
  const view = bytes instanceof Uint8Array ? bytes : new Uint8Array(bytes);
  let text = '';
  for (let i = 0; i < view.length; i += 1) text += String.fromCharCode(view[i]);
  return btoa(text).replace(/[+/=]/g, (c) => (c === '+' ? '-' : c === '/' ? '_' : ''));
}

export function randomSecretBytes() {
  const secret = new Uint8Array(32);
  crypto.getRandomValues(secret);
  return secret;
}

export async function sha256Hex(data) {
  const bytes = data instanceof Uint8Array ? data : new Uint8Array(data);
  const digest = await crypto.subtle.digest('SHA-256', bytes);
  return Array.from(new Uint8Array(digest), (b) => b.toString(16).padStart(2, '0')).join('');
}

// ---------------------------------------------------------------------------
// バイナリフレーム: [u32be header_length][UTF-8 JSON header][payload]
// ---------------------------------------------------------------------------

const textEncoder = new TextEncoder();
const textDecoder = new TextDecoder('utf-8', { fatal: true });

/**
 * 1 レコードを直列化する。header.file_id / chunk_index / payload_size(10進) /
 * final を必須とし、upload 側は sha256 も必須。
 */
function checkedHeader(header, payloadSize, limits) {
  if (!Number.isInteger(header.file_id) || header.file_id < 0 || header.file_id > 0xFFFFFFFF
    || !Number.isInteger(header.chunk_index) || header.chunk_index < 0 || header.chunk_index > 0xFFFFFFFF
    || typeof header.final !== 'boolean' || payloadSize > limits.chunkBytes
    || (header.sha256 !== undefined && !/^[a-f0-9]{64}$/.test(header.sha256))) {
    throw new TransferError('invalid_frame', 'レコードの識別子・サイズ・SHA が不正です。');
  }
  return { file_id: header.file_id, chunk_index: header.chunk_index,
    payload_size: String(payloadSize), final: header.final,
    ...(header.sha256 === undefined ? {} : { sha256: header.sha256 }) };
}

export function encodeRecord(header, payload, limits = LIMITS) {
  const body = payload instanceof Uint8Array ? payload : new Uint8Array(payload);
  const normalized = checkedHeader(header, body.byteLength, limits);
  const headerBytes = textEncoder.encode(JSON.stringify(normalized));
  if (headerBytes.byteLength > limits.recordHeaderBytes) {
    throw new TransferError('record_header_too_large', `レコードヘッダが ${limits.recordHeaderBytes} バイトを超えました。`);
  }
  const frame = new Uint8Array(4 + headerBytes.byteLength + body.byteLength);
  new DataView(frame.buffer).setUint32(0, headerBytes.byteLength, false);
  frame.set(headerBytes, 4);
  frame.set(body, 4 + headerBytes.byteLength);
  return frame;
}

/** 連続フレームを厳密にデコードする。途中切れは成功 EOF にしない。 */
export function decodeFrames(bytes, limits = LIMITS) {
  const view = bytes instanceof Uint8Array ? bytes : new Uint8Array(bytes);
  if (view.byteLength > limits.binaryBodyBytes) {
    throw new TransferError('response_too_large', 'バッチが交渉済み上限を超えています。');
  }
  const records = [];
  let offset = 0;
  while (offset < view.byteLength) {
    if (records.length >= limits.maxRecordsPerBatch) {
      throw new TransferError('invalid_frame', 'バッチのレコード数が上限を超えています。');
    }
    if (view.byteLength - offset < 4) {
      throw new TransferError('truncated_frame', 'フレーム長フィールドが途中で切れています。');
    }
    const headerLength = new DataView(view.buffer, view.byteOffset + offset, 4).getUint32(0, false);
    if (headerLength <= 0 || headerLength > limits.recordHeaderBytes) {
      throw new TransferError('invalid_frame', `不正なレコードヘッダ長 ${headerLength} です。`);
    }
    const headerEnd = offset + 4 + headerLength;
    if (headerEnd > view.byteLength) {
      throw new TransferError('truncated_frame', 'レコードヘッダが応答の途中で切れています。');
    }
    let header;
    try {
      header = JSON.parse(textDecoder.decode(view.subarray(offset + 4, headerEnd)));
    } catch {
      throw new TransferError('invalid_frame', 'レコードヘッダが JSON として読めません。');
    }
    if (!header || typeof header.payload_size !== 'string' || !/^(0|[1-9][0-9]*)$/.test(header.payload_size)) {
      throw new TransferError('invalid_frame', 'レコードの payload_size が10進文字列ではありません。');
    }
    const payloadSize = BigInt(header.payload_size);
    if (payloadSize > BigInt(limits.chunkBytes)) {
      throw new TransferError('invalid_frame', 'レコードの payload_size が上限を超えています。');
    }
    checkedHeader(header, Number(payloadSize), limits);
    const payloadEnd = headerEnd + Number(payloadSize);
    if (payloadEnd > view.byteLength) {
      throw new TransferError('truncated_frame', 'レコード本文が応答の途中で切れています。');
    }
    records.push({ header, payload: view.subarray(headerEnd, payloadEnd) });
    offset = payloadEnd;
  }
  return records;
}

/**
 * 送信バッチを組み立てる。file record の payload は chunk_bytes
 * (8 MiB - 64 KiB) まで、1 バッチは 100 レコード・フレーム込み 8 MiB 以内。
 * 送信側の payload は同じ file 内で offset 昇順・連続でなければならない。
 */
export class BatchBuilder {
  constructor(limits = LIMITS) {
    this.limits = limits;
    this.buffer = null;
    this.size = 0;
    this.count = 0;
  }

  fits(frameBytes) {
    return this.count < this.limits.maxRecordsPerBatch
      && this.size + frameBytes <= this.limits.binaryBodyBytes;
  }

  add(frame) {
    if (!this.fits(frame.byteLength)) return false;
    this.buffer ??= new Uint8Array(this.limits.binaryBodyBytes);
    this.buffer.set(frame, this.size);
    this.size += frame.byteLength;
    this.count += 1;
    return true;
  }

  addRecord(header, payload) {
    const bytes = textEncoder.encode(JSON.stringify(checkedHeader(header, payload.byteLength, this.limits)));
    if (bytes.byteLength > this.limits.recordHeaderBytes) {
      throw new TransferError('invalid_frame', 'レコードヘッダが上限を超えました。');
    }
    const length = 4 + bytes.byteLength + payload.byteLength;
    if (!this.fits(length)) return false;
    this.buffer ??= new Uint8Array(this.limits.binaryBodyBytes);
    new DataView(this.buffer.buffer).setUint32(this.size, bytes.byteLength, false);
    this.buffer.set(bytes, this.size + 4);
    this.buffer.set(payload, this.size + 4 + bytes.byteLength);
    this.size += length;
    this.count += 1;
    return true;
  }

  build() {
    return this.count ? this.buffer.subarray(0, this.size) : null;
  }
}

/** 確定 offset (10進/BigInt) から resume する chunk_index を求める。 */
export function resumeChunkIndex(confirmedBytes, chunkBytes = LIMITS.chunkBytes) {
  const offset = parseDecimal(confirmedBytes, 0n);
  const unit = BigInt(chunkBytes);
  if (offset % unit !== 0n) {
    throw new TransferError('unexpected_offset', `確定 offset ${offset} が chunk 境界に一致しません。`);
  }
  return Number(offset / unit);
}

// ---------------------------------------------------------------------------
// エラー分類
// ---------------------------------------------------------------------------

function contentTypeIsHtml(headers) {
  const type = headers?.get?.('content-type') || '';
  return type.includes('text/html');
}

/**
 * HTTP 応答を TransferError に分類する。JSON エラー封筒
 * {error:{code,message,retryable},session_id,revision} を優先し、HTML や
 * 非 JSON は前段 (リバースプロキシ/Access) 由来として区別する。
 */
export function classifyHttpError(status, bodyText, headers) {
  const retryAfterHeader = headers?.get?.('retry-after');
  const retryValue = retryAfterHeader && /^[0-9]+$/.test(retryAfterHeader)
    ? Number(retryAfterHeader) * 1000 : Math.max(0, Date.parse(retryAfterHeader || '') - Date.now());
  const retryAfterMs = Number.isFinite(retryValue) && retryValue <= 2_147_483_647 ? retryValue : 0;
  let parsed = null;
  if (bodyText && !contentTypeIsHtml(headers)) {
    try { parsed = JSON.parse(bodyText); } catch { /* 非 JSON 応答 */ }
  }
  const envelope = parsed && typeof parsed === 'object' ? parsed.error : null;
  const code = envelope && typeof envelope.code === 'string' ? envelope.code : '';
  const message = (envelope && typeof envelope.message === 'string' && envelope.message)
    || (parsed && typeof parsed.message === 'string' && parsed.message)
    || '';
  const revision = parsed && parsed.revision !== undefined ? String(parsed.revision) : '';
  const retryable = envelope?.retryable === true;
  const html = contentTypeIsHtml(headers);

  if (status === 401 || code === 'authentication_required') {
    return new TransferError(code || 'authentication_required',
      message || '認証が切れました。再ログインが必要です。', { httpStatus: status, revision });
  }
  if (status === 413) {
    if (code === 'transfer_chunk_too_large') {
      return new TransferError('transfer_chunk_too_large',
        `交渉済み上限を超える転送を送信しました (実装不整合)。${message}`, { httpStatus: status, revision });
    }
    const requestId = String(headers?.get?.('x-request-id') || headers?.get?.('cf-ray') || '').slice(0, 128);
    return new TransferError('front_proxy_limit',
      `転送経路の前段がリクエストを拒否しました (HTTP 413)。原因のサーバーは断定できません。${message ? ` ${message}` : ''}${requestId ? ` 要求 ID: ${requestId}` : ''}`,
      { httpStatus: status, html });
  }
  if (status === 429 || status === 503) {
    return new TransferError(code || (status === 429 ? 'rate_limited' : 'service_unavailable'),
      message || `サーバーが処理を受け付けられません (HTTP ${status})。`,
      { httpStatus: status, retryable: true, retryAfterMs: retryAfterMs || 2000, revision });
  }
  if (status === 410) {
    return new TransferError(code || 'session_expired',
      message || '転送セッションの期限が切れました。', { httpStatus: status, revision });
  }
  if (status === 428 || code === 'precondition_required' || status === 412) {
    return new TransferError(code || 'precondition_failed',
      message || 'セッション状態が更新されています。', { httpStatus: status, retryable, revision });
  }
  if (status === 409) {
    return new TransferError(code || 'conflict', message || '転送が競合しました (HTTP 409)。',
      { httpStatus: status, retryable, revision });
  }
  if (status === 423 || code === 'novel_busy' || code === 'novel_recovering') {
    return new TransferError(code || 'resource_locked', message || '対象の作品が他の処理で使用中です。',
      { httpStatus: status, retryable: true, retryAfterMs: retryAfterMs || 3000, revision });
  }
  if (code) {
    return new TransferError(code, message || `転送に失敗しました (HTTP ${status})。`,
      { httpStatus: status, retryable, revision, html });
  }
  return new TransferError(`http_${status}`,
    html
      ? `転送 API ではない応答 (HTML, HTTP ${status}) を受け取りました。認証または前段の制限を確認してください。`
      : `転送に失敗しました (HTTP ${status})。`,
    { httpStatus: status, html });
}

export function isRetryable(error) {
  if (error instanceof TransferError) return error.retryable === true;
  return error?.name === 'TypeError' || error?.name === 'TimeoutError';
}

export function retryDelayMs(attempt, error) {
  const hinted = error instanceof TransferError ? error.retryAfterMs : 0;
  const backoff = Math.min(500 * 2 ** attempt, 30_000);
  return Math.max(hinted, Math.min(backoff, 30_000));
}

// ---------------------------------------------------------------------------
// wire manifest spool (OPFS 優先、無ければメモリ)
// ---------------------------------------------------------------------------

/**
 * wire manifest / export manifest の spool (上限 64 MiB)。OPFS の
 * sync access handle / writable が使える環境ではディスクへ、それ以外では
 * bounded なメモリ列へ書く。巨大 Blob 化はしない。
 */
export class ByteSpool {
  constructor() {
    this.fileHandle = null;
    this.directory = null;
    this.access = null;
    this.writable = null;
    this.parts = [];
    this.size = 0;
    this.finalized = false;
  }

  static async open(name) {
    const spool = new ByteSpool();
    const storage = globalThis.navigator?.storage;
    if (!storage?.getDirectory) return spool;
    try {
      spool.directory = await (await storage.getDirectory()).getDirectoryHandle(SPOOL_DIR, { create: true });
      spool.name = name;
      spool.fileHandle = await spool.directory.getFileHandle(name, { create: true });
      if (typeof spool.fileHandle.createSyncAccessHandle === 'function') {
        spool.access = await spool.fileHandle.createSyncAccessHandle();
        spool.access.truncate(0);
      } else if (typeof spool.fileHandle.createWritable === 'function') {
        spool.writable = await spool.fileHandle.createWritable();
      } else {
        await spool.close({ remove: true });
      }
    } catch (error) {
      await spool.close({ remove: true });
      if (error?.name !== 'NotSupportedError' && error?.name !== 'SecurityError') throw error;
    }
    return spool;
  }

  async append(chunk) {
    const bytes = chunk instanceof Uint8Array ? chunk : new Uint8Array(chunk);
    if (this.finalized || this.size + bytes.byteLength > LIMITS.manifestBytes) {
      throw new TransferError('manifest_too_large', 'manifest spool の上限または書込み状態が不正です。');
    }
    if (this.access) {
      let written = 0;
      while (written < bytes.byteLength) {
        const count = this.access.write(bytes.subarray(written), { at: this.size + written });
        if (!count) throw new TransferError('storage_write', 'manifest spool を書き込めません。');
        written += count;
      }
    } else if (this.writable) {
      await this.writable.write(bytes);
    } else {
      this.parts.push(bytes.slice());
    }
    this.size += bytes.byteLength;
  }

  async finalize() {
    if (this.finalized) return;
    this.access?.flush();
    if (this.writable) { await this.writable.close(); this.writable = null; }
    this.finalized = true;
  }

  async read(offset, length) {
    if (!Number.isSafeInteger(offset) || !Number.isSafeInteger(length) || offset < 0 || length < 0
      || offset + length > this.size || !this.finalized) {
      throw new TransferError('spool_range', 'manifest spool の読出し範囲または状態が不正です。');
    }
    if (this.access) {
      const out = new Uint8Array(length);
      let read = 0;
      while (read < length) {
        const count = this.access.read(out.subarray(read), { at: offset + read });
        if (!count) throw new TransferError('truncated_frame', 'manifest spool が途中で切れています。');
        read += count;
      }
      return out;
    }
    if (this.fileHandle) {
      const file = await this.fileHandle.getFile();
      const out = new Uint8Array(await file.slice(offset, offset + length).arrayBuffer());
      if (out.byteLength !== length) throw new TransferError('truncated_frame', 'manifest spool が途中で切れています。');
      return out;
    }
    const out = new Uint8Array(length);
    let written = 0, cursor = 0;
    for (const part of this.parts) {
      const end = cursor + part.byteLength;
      if (end > offset && cursor < offset + length) {
        const start = Math.max(offset - cursor, 0);
        const take = Math.min(part.byteLength - start, length - written);
        out.set(part.subarray(start, start + take), written);
        written += take;
      }
      cursor = end;
      if (written === length) break;
    }
    if (written !== length) throw new TransferError('truncated_frame', 'manifest spool が途中で切れています。');
    return out;
  }

  async close({ remove = false } = {}) {
    try { this.access?.close(); } catch { /* already closed */ }
    try { await this.writable?.abort(); } catch { /* already closed */ }
    this.access = null;
    this.writable = null;
    this.parts = [];
    if (remove && this.directory && this.name) {
      try { await this.directory.removeEntry(this.name); } catch { /* stale spool is private, never library data */ }
    }
    this.fileHandle = null;
  }
}

// ---------------------------------------------------------------------------
// セッション永続化 (IndexedDB / メモリ fallback)
// ---------------------------------------------------------------------------

function openSessionDb() {
  return new Promise((resolve) => {
    if (typeof indexedDB === 'undefined') { resolve(null); return; }
    const request = indexedDB.open(SESSION_DB, 1);
    request.onupgradeneeded = () => {
      request.result.createObjectStore(SESSION_STORE, { keyPath: 'requestId' });
    };
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => resolve(null);
  });
}

export class SessionStore {
  constructor(db) {
    this.db = db;
    this.memory = new Map();
  }

  get persistent() { return this.db !== null; }

  async put(record) {
    if (!this.db) { this.memory.set(record.requestId, record); return; }
    await new Promise((resolve, reject) => {
      const tx = this.db.transaction(SESSION_STORE, 'readwrite');
      tx.objectStore(SESSION_STORE).put(record);
      tx.oncomplete = resolve;
      tx.onerror = () => reject(tx.error);
    });
  }

  async get(requestId) {
    if (!this.db) return this.memory.get(requestId) ?? null;
    return new Promise((resolve, reject) => {
      const tx = this.db.transaction(SESSION_STORE, 'readonly');
      const request = tx.objectStore(SESSION_STORE).get(requestId);
      request.onsuccess = () => resolve(request.result ?? null);
      request.onerror = () => reject(request.error);
    });
  }

  async remove(requestId) {
    if (!this.db) { this.memory.delete(requestId); return; }
    await new Promise((resolve, reject) => {
      const tx = this.db.transaction(SESSION_STORE, 'readwrite');
      tx.objectStore(SESSION_STORE).delete(requestId);
      tx.oncomplete = resolve;
      tx.onerror = () => reject(tx.error);
    });
  }

  async all() {
    if (!this.db) return [...this.memory.values()];
    return new Promise((resolve, reject) => {
      const tx = this.db.transaction(SESSION_STORE, 'readonly');
      const request = tx.objectStore(SESSION_STORE).getAll();
      request.onsuccess = () => resolve(request.result ?? []);
      request.onerror = () => reject(request.error);
    });
  }

  static async open() {
    return new SessionStore(await openSessionDb());
  }
}

// ---------------------------------------------------------------------------
// selection digest (exports/{s}/select)
// ---------------------------------------------------------------------------

/**
 * catalog revision、profile、追加 role、全 file identity を共有 service と同じ順序で束ねる。
 */
function compareUtf8Text(left, right) {
  let a = 0, b = 0;
  while (a < left.length && b < right.length) {
    const x = left.codePointAt(a), y = right.codePointAt(b);
    if (x !== y) return x - y;
    a += x > 0xFFFF ? 2 : 1; b += y > 0xFFFF ? 2 : 1;
  }
  return (a < left.length ? 1 : 0) - (b < right.length ? 1 : 0);
}

export async function selectionDigest(profile, extraRoles, catalogRevision, items) {
  const roles = [...new Set(extraRoles)].sort();
  const rows = items.map((item) => `${item.novel_ordinal}\0${item.logical_path}\0${item.size}\0${item.role}`).sort(compareUtf8Text);
  const canonical = `v2\0${catalogRevision}\0${profile}\0${roles.join(',')}\0`
    + rows.map((row) => `${row}\x01`).join('');
  return sha256Hex(textEncoder.encode(canonical));
}

// ---------------------------------------------------------------------------
// Worker 本処理 (DedicatedWorkerGlobalScope 内でのみ起動)
// ---------------------------------------------------------------------------

const IS_WORKER = typeof WorkerGlobalScope !== 'undefined'
  && typeof self !== 'undefined'
  && self instanceof WorkerGlobalScope;

if (IS_WORKER) {
  boot().catch((error) => {
    try { self.postMessage({ type: 'fatal', error: serializeError(error) }); }
    catch { /* postMessage 自体が失敗した環境では諦める */ }
  });
}

function serializeError(error) {
  const err = error instanceof TransferError ? error
    : new TransferError(error?.name === 'QuotaExceededError' ? 'storage_quota' : 'transfer_error', String(error?.message || error));
  return {
    name: err?.name || 'Error',
    code: err instanceof TransferError ? err.code : '',
    message: String(err?.message || err),
    retryable: err?.retryable === true,
    httpStatus: err?.httpStatus ?? 0,
  };
}

async function boot() {
  const ctx = {
    wasm: null,
    codec: null,
    manifest: null,
    capabilities: null,
    limits: { ...LIMITS },
    secretHeader: '',
    requestId: '',
    sessionId: '',
    direction: '',
    operationId: '',
    revision: '',
    etag: '',
    abort: false,
    abortListeners: new Set(),
    store: await SessionStore.open(),
    pendingUi: new Map(),
    uiSequence: 0,
  };
  self.onmessage = (event) => { void handleMessage(ctx, event.data); };
  self.postMessage({ type: 'booted' });
}

async function handleMessage(ctx, data) {
  if (!data || typeof data !== 'object') return;
  try {
    switch (data.type) {
      case 'init':
        await initWorker(ctx, data.manifest, data.capabilities);
        post({ type: 'ready', codec: ctx.codec });
        break;
      case 'command':
        if (data.name === 'abort') {
          ctx.abort = true;
          ctx.abortAction = data.payload?.action === 'cancel' ? 'cancel' : 'pause';
          ctx.controller?.abort();
          for (const pending of ctx.pendingUi.values()) {
            pending.reject(new TransferError('aborted', '転送が中止されました。'));
          }
          ctx.pendingUi.clear();
          for (const listener of ctx.abortListeners) { try { listener(); } catch { /* 継続 */ } }
          break;
        }
        await runCommand(ctx, data);
        break;
      case 'response':
        resolvePending(ctx, data);
        break;
      default:
        break;
    }
  } catch (error) {
    const err = error instanceof TransferError ? error
    : new TransferError(error?.name === 'QuotaExceededError' ? 'storage_quota' : 'transfer_error', String(error?.message || error));
    if (data.type === 'command') {
      post({ type: 'result', name: data.name, ok: false, error: serializeError(err) });
    } else {
      post({ type: 'fatal', error: serializeError(err) });
    }
  }
}

function post(message, transfer = []) {
  self.postMessage(message, transfer);
}

/** UI スレッドへの要求。応答を待つ Promise を返す。 */
function requestUi(ctx, name, payload, transfer = []) {
  throwIfAborted(ctx);
  const id = ++ctx.uiSequence;
  return new Promise((resolve, reject) => {
    ctx.pendingUi.set(id, { resolve, reject });
    post({ type: 'request', id, name, payload }, transfer);
  });
}

function resolvePending(ctx, data) {
  const pending = ctx.pendingUi.get(data.id);
  if (!pending) return;
  ctx.pendingUi.delete(data.id);
  if (data.ok) pending.resolve(data.value);
  else pending.reject(new TransferError(data.error?.code || 'ui_rejected', data.error?.message || 'UI が要求を拒否しました。'));
}

function emit(ctx, name, payload) {
  post({ type: 'event', name, payload });
}

function phase(ctx, name, label, extra = {}) {
  emit(ctx, 'phase', { phase: name, label, ...extra });
}

function progress(ctx, update) {
  emit(ctx, 'progress', update);
}

function throwIfAborted(ctx) {
  if (ctx.abort) throw new TransferError('aborted', '転送が中止されました。');
}

async function runCommand(ctx, command) {
  if (ctx.busy) {
    post({ type: 'result', name: command.name, ok: false,
      error: serializeError(new TransferError('busy', 'このタブでは既に転送しています。')) });
    return;
  }
  ctx.busy = true;
  Object.assign(ctx, { abort: false, abortAction: 'pause', requestId: '', sessionId: '',
    secretHeader: '', direction: '', operationId: '', revision: '', status: null,
    pendingOperation: null, createRequest: null, fingerprint: '', fileName: '', ids: [],
    policy: null, policyVersion: undefined, selection: null, manifestNextChunk: 0, receipts: [], uploadStats: null,
    outputBytes: '0', outputClosed: false, receivedBytes: 0n, expectedFileHashes: null });
  ctx.controller = new AbortController();
  const name = command.name;
  try {
    let value = null;
    if (name === 'import' || name === 'resume-import') value = await runImport(ctx, command.payload ?? {});
    else if (name === 'export' || name === 'resume-export') value = await runExport(ctx, command.payload ?? {});
    else if (name === 'pending') value = await pendingSessions(ctx);
    else if (name === 'discard') await discardSession(ctx, command.payload || {});
    else throw new TransferError('unknown_command', `不明なコマンド ${String(name)}`);
    post({ type: 'result', name, ok: true, value });
  } catch (error) {
    // A cancelled fetch may already have committed. Recover that receipt before
    // pausing/cancelling, retaining the identity if the network is unavailable.
    if ((ctx.sessionId || ctx.createRequest) && ['import', 'resume-import', 'export', 'resume-export'].includes(name)) {
      ctx.abort = false;
      ctx.controller = new AbortController();
      try {
        await recoverCreation(ctx);
        await recoverPendingOperation(ctx);
        const status = (await apiRequest(ctx, sessionPath(ctx))).body;
        adoptStatus(ctx, status);
        if (!isTerminal(status.state)) {
          if (ctx.abortAction === 'cancel') {
            await sessionOperation(ctx, sessionPath(ctx), {}, { method: 'DELETE' });
            await advanceUntil(ctx, (state) => state === 'cancelled', 'cancel');
            await ctx.store.remove(ctx.requestId);
          } else {
            if (status.state !== 'paused') await sessionOperation(ctx, `${sessionPath(ctx)}/pause`, {});
            await advanceUntil(ctx, (state, next) => (state === 'paused' && !['advance', 'finalize_file'].includes(next)) || isTerminal(state), 'pause');
          }
        }
        if (ctx.status?.state === 'completed' || ctx.status?.state === 'completed_with_errors') {
          const summary = summarizePlan(await collectPages(ctx, `${sessionPath(ctx)}/plan`));
          await ctx.store.remove(ctx.requestId);
          post({ type: 'result', name, ok: true, value: { state: ctx.status.state, counts: summary.counts, errors: summary.errors } });
          return;
        }
        if (ctx.status?.state === 'released') {
          await ctx.store.remove(ctx.requestId);
          post({ type: 'result', name, ok: true, value: { state: 'released', bytes: ctx.outputBytes, alreadyCompleted: !ctx.outputClosed } });
          return;
        }
        if (ctx.status?.state === 'cancelled') await ctx.store.remove(ctx.requestId);
      } catch (controlError) {
        emit(ctx, 'phase', { phase: 'pause', label: '停止結果を確認できません。再開情報は保持しています。',
          error: serializeError(controlError) });
      }
    }
    post({ type: 'result', name, ok: false, error: { ...serializeError(error), stopState: ctx.status?.state || '' } });
  } finally {
    ctx.busy = false;
  }
}

// ---------------------------------------------------------------------------
// 初期化: artifact manifest → glue import → WASM init → codec 照合
// ---------------------------------------------------------------------------

const REQUIRED_WASM_API = ['codec_info', 'open_archive', 'validate_archive', 'catalog_page',
  'write_transfer_manifest', 'stream_entry', 'close_archive', 'build_zip'];

async function initWorker(ctx, manifest, capabilities) {
  if (!manifest || typeof manifest !== 'object') {
    throw new TransferError('artifact_missing', '転送 artifact manifest が取得できません。');
  }
  if (manifest.protocol_version !== PROTOCOL_VERSION || manifest.abi_version !== ABI_VERSION) {
    throw new TransferError('protocol_mismatch',
      `転送 artifact の protocol ${String(manifest.protocol_version)}/abi ${String(manifest.abi_version)} は`
      + ` ${PROTOCOL_VERSION}/${ABI_VERSION} と一致しません。再読み込みしてください。`);
  }
  if (!manifest.glue_url || !manifest.wasm_url || !manifest.build_id) {
    throw new TransferError('artifact_missing', '転送 WASM/glue の URL または build_id が artifact manifest にありません。');
  }
  for (const name of ['glue_url', 'wasm_url', 'worker_url']) {
    const url = new URL(manifest[name], self.location.href);
    if (url.origin !== self.location.origin || !['https:', 'http:'].includes(url.protocol)) {
      throw new TransferError('artifact_invalid', '転送 artifact は同一 origin の URL である必要があります。');
    }
  }
  if (capabilities?.protocol_version !== PROTOCOL_VERSION
    || String(capabilities?.build?.abi_version) !== String(ABI_VERSION)) {
    throw new TransferError('abi_mismatch', 'サーバーの転送 protocol/ABI が一致しません。');
  }
  ctx.manifest = manifest;
  ctx.capabilities = capabilities ?? null;
  applyServerLimits(ctx, capabilities);

  let glue;
  try {
    glue = await import(manifest.glue_url);
  } catch (error) {
    throw new TransferError('wasm_glue_missing', `転送 WASM glue を読み込めません: ${error?.message || error}`);
  }
  for (const name of REQUIRED_WASM_API) {
    if (typeof glue[name] !== 'function') {
      throw new TransferError('abi_mismatch', `転送 WASM に ${name} がありません。配布物を更新してください。`);
    }
  }
  try {
    await glue.default(manifest.wasm_url);
  } catch (error) {
    throw new TransferError('wasm_init_failed', `転送 WASM を初期化できません: ${error?.message || error}`);
  }
  let codec;
  try {
    codec = JSON.parse(glue.codec_info());
  } catch (error) {
    throw new TransferError('abi_mismatch', `codec_info を読めません: ${error?.message || error}`);
  }
  // サーバーの主張ではなく、コンパイル済み codec_info と artifact manifest を照合する。
  if (codec.protocol_version !== PROTOCOL_VERSION || codec.abi_version !== ABI_VERSION) {
    throw new TransferError('abi_mismatch',
      `転送 WASM の protocol ${String(codec.protocol_version)}/abi ${String(codec.abi_version)} は`
      + ` ${PROTOCOL_VERSION}/${ABI_VERSION} と一致しません。`);
  }
  if (codec.build_id !== manifest.build_id) {
    throw new TransferError('abi_mismatch',
      '転送 WASM build が artifact manifest と一致しません。ページを再読み込みしてください。');
  }
  const serverBuild = ctx.capabilities?.build?.wasm_build_id ?? '';
  if (!serverBuild) {
    throw new TransferError('wasm_unsupported',
      'サーバーが転送 WASM の配布をサポートしていません (capabilities build.wasm_build_id がありません)。');
  }
  if (serverBuild !== codec.build_id) {
    throw new TransferError('abi_mismatch',
      'サーバーが期待する転送 WASM build と一致しません。ページを再読み込みしてください。');
  }
  ctx.wasm = glue;
  ctx.codec = codec;
}

function applyServerLimits(ctx, capabilities) {
  const limits = capabilities?.limits;
  if (!limits || typeof limits !== 'object') throw new TransferError('protocol_error', 'capabilities に limits がありません。');
  const fields = { binary_body_bytes: 'binaryBodyBytes', chunk_bytes: 'chunkBytes', codec_io_bytes: 'codecIoBytes',
    batch_records: 'maxRecordsPerBatch', record_header_bytes: 'recordHeaderBytes', control_body_bytes: 'controlJsonBytes',
    manifest_bytes: 'manifestBytes', manifest_metadata_bytes: 'metadataBytes', page_items: 'pageItems' };
  const negotiated = { ...LIMITS };
  for (const [wire, key] of Object.entries(fields)) {
    const value = parseDecimal(limits[wire], -1n);
    if (value <= 0n || value > BigInt(LIMITS[key])) throw new TransferError('protocol_error', `不正な転送上限 ${wire} です。`);
    negotiated[key] = Number(value);
  }
  if (negotiated.chunkBytes > negotiated.binaryBodyBytes - 64 * 1024
    || negotiated.chunkBytes % negotiated.codecIoBytes !== 0) {
    throw new TransferError('protocol_error', 'chunk と framing/codec の転送上限が一致しません。');
  }
  ctx.limits = negotiated;
}

// ---------------------------------------------------------------------------
// HTTP 層
// ---------------------------------------------------------------------------

async function readErrorBody(response) {
  const reader = response.body?.getReader?.();
  if (!reader) return '';
  const bytes = new Uint8Array(LIMITS.errorBodyBytes);
  let used = 0;
  try {
    while (used < bytes.byteLength) {
      const { value, done } = await reader.read();
      if (done) break;
      const take = Math.min(value.byteLength, bytes.byteLength - used);
      bytes.set(value.subarray(0, take), used);
      used += take;
    }
    return new TextDecoder().decode(bytes.subarray(0, used));
  } catch { return ''; }
  finally {
    try { await reader.cancel(); } catch { /* closed */ }
    reader.releaseLock();
  }
}

/**
 * 転送 API への 1 要求。session secret は create だけでなく同じ identity の
 * 全 session 要求へ付ける。redirect は成功として扱わない。
 */
async function apiRequest(ctx, path, options = {}) {
  const { method = 'GET', json, binary, headers = {}, raw = false } = options;
  const init = {
    method, redirect: 'manual', cache: 'no-store', credentials: 'same-origin',
    headers: { ...headers }, signal: ctx.controller?.signal,
  };
  if (ctx.secretHeader) init.headers[SECRET_HEADER] = ctx.secretHeader;
  if (json !== undefined) {
    const body = JSON.stringify(json);
    if (textEncoder.encode(body).byteLength > ctx.limits.controlJsonBytes) {
      throw new TransferError('control_json_too_large', '制御 JSON が交渉上限を超えました。');
    }
    init.headers['content-type'] = 'application/json';
    init.body = body;
  } else if (binary !== undefined) {
    if (binary.byteLength > ctx.limits.binaryBodyBytes) {
      throw new TransferError('transfer_chunk_too_large', '送信本文が交渉済み上限を超えました。');
    }
    init.headers['content-type'] = 'application/octet-stream';
    init.headers['x-transfer-body-bytes'] = String(binary.byteLength);
    init.body = binary;
  }
  let response;
  try { response = await fetch(`${API_PREFIX}${path}`, init); }
  catch (error) {
    if (ctx.abort || error?.name === 'AbortError') {
      throw new TransferError('aborted', '転送が中止されました。');
    }
    throw new TransferError('network', `通信に失敗しました: ${error?.message || error}`, { retryable: true });
  }
  if (response.type === 'opaqueredirect' || (response.status >= 300 && response.status < 400)) {
    throw new TransferError('authentication_required', '転送 API が認証ページへリダイレクトしました。');
  }
  if (!response.ok) {
    throw classifyHttpError(response.status, await readErrorBody(response), response.headers);
  }
  if (raw) {
    if (!(response.headers.get('content-type') || '').toLowerCase().startsWith('application/octet-stream')) {
      try { await response.body?.cancel(); } catch { /* closed */ }
      throw new TransferError('authentication_required', 'バイナリ転送の代わりに認証ページなどが返されました。');
    }
    const bytes = await readBoundedBody(response, ctx.limits.binaryBodyBytes);
    return { status: response.status, headers: response.headers, bytes };
  }
  if (!(response.headers.get('content-type') || '').includes('json')) {
    throw new TransferError('authentication_required', '認証ページまたは転送 API 以外の応答を受け取りました。');
  }
  const bytes = await readBoundedBody(response, ctx.limits.controlJsonBytes);
  let body = null;
  try { body = bytes.byteLength ? JSON.parse(textDecoder.decode(bytes)) : null; }
  catch { throw new TransferError('invalid_response', '転送 API の応答が JSON ではありません。'); }
  return { status: response.status, headers: response.headers, body };
}

/** 宣言に頼らず実 byte を数え、上限超過時に直ちに cancel する。 */
export async function readBoundedBody(response, limit) {
  if (!response.body) throw new TransferError('truncated_frame', '応答本文がありません。');
  const declared = response.headers.get('content-length');
  if (declared && parseDecimal(declared, BigInt(limit + 1)) > BigInt(limit)) {
    await response.body.cancel();
    throw new TransferError('response_too_large', '応答が交渉済み上限を超えています。');
  }
  const reader = response.body.getReader();
  const buffer = new Uint8Array(limit);
  let used = 0;
  try {
    for (;;) {
      const { value, done } = await reader.read();
      if (done) break;
      if (used + value.byteLength > limit) {
        throw new TransferError('response_too_large', '実応答が交渉済み上限を超えました。');
      }
      buffer.set(value, used);
      used += value.byteLength;
    }
    if (declared && parseDecimal(declared, -1n) !== BigInt(used)) {
      throw new TransferError('truncated_frame', '応答長が Content-Length と一致しません。');
    }
    return buffer.subarray(0, used);
  } finally {
    try { await reader.cancel(); } catch { /* closed */ }
    reader.releaseLock();
  }
}
async function withRetry(ctx, fn, { operation = 'operation', attempts = MAX_CONTROL_ATTEMPTS } = {}) {
  for (let attempt = 0; ; attempt += 1) {
    throwIfAborted(ctx);
    try {
      return await fn();
    } catch (error) {
      if (error instanceof TransferError && (error.code === 'authentication_required' || error.httpStatus === 401)) {
        const retry = await requestUi(ctx, 'auth', {
          message: '認証が切れました。別のタブでログインし直してから「再試行」を押してください。転送は中断せず待機しています。',
        });
        if (!retry) throw error;
        continue;
      }
      if (!isRetryable(error) || attempt + 1 >= attempts) throw error;
      const delay = retryDelayMs(attempt, error);
      progress(ctx, { phase: 'retry', label: `${operation} を再試行します (${Math.ceil(delay / 1000)} 秒後)`, attempt: attempt + 1 });
      await sleep(delay, ctx);
    }
  }
}

function sleep(ms, ctx) {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => { cleanup(); resolve(); }, ms);
    const onAbort = () => { cleanup(); reject(new TransferError('aborted', '転送が中止されました。')); };
    const cleanup = () => { clearTimeout(timer); ctx.abortListeners.delete(onAbort); };
    ctx.abortListeners.add(onAbort);
  });
}

export function adoptStatus(ctx, body) {
  if (!body || body.protocol_version !== PROTOCOL_VERSION || typeof body.revision !== 'string'
    || !/^[0-9]+$/.test(body.revision) || !body.session_id) {
    throw new TransferError('protocol_error', 'セッション状態の版・revision・ID が不正です。');
  }
  if (ctx.sessionId && ctx.sessionId !== body.session_id) {
    throw new TransferError('protocol_error', '別のセッションの状態が返されました。');
  }
  if (BigInt(body.revision) < parseDecimal(ctx.revision, -1n)) {
    if (!ctx.status) throw new TransferError('stale_status', '保存された確定 revision より古い状態が返されました。');
    return ctx.status;
  }
  if (parseDecimal(body.chunk_bytes, -1n) !== BigInt(ctx.limits.chunkBytes)) {
    throw new TransferError('protocol_error', 'session の chunk_bytes が交渉済み値と一致しません。');
  }
  ctx.revision = body.revision;
  ctx.sessionId = body.session_id;
  ctx.operationId = body.operation_id || '';
  ctx.status = body;
  ctx.manifestNextChunk = body.manifest_next_chunk_index ?? ctx.manifestNextChunk ?? 0;
  return body;
}

function isTerminal(state) {
  return ['completed', 'completed_with_errors', 'released', 'cancelled', 'expired', 'failed'].includes(state);
}

function sessionPath(ctx) {
  return `/${ctx.direction}/${encodeURIComponent(ctx.sessionId)}`;
}

function ifMatchHeaders(ctx) {
  return ctx.revision ? { 'if-match': `"${ctx.revision}"` } : {};
}

async function recoverPendingOperation(ctx) {
  const pending = ctx.pendingOperation;
  if (!pending) return ctx.status;
  // Only a definitive 412/428 revision-gate rejection proves this send left no
  // receipt: adopt the authoritative status, then replay the identical
  // operation identity/body under the fresh revision. A Busy (423) or any
  // other ambiguous retryable failure does NOT prove an earlier attempt never
  // ran — the session lock may be held by that same in-flight operation, and a
  // committed receipt fingerprint binds the revision it was sent with — so the
  // identical receipt is replayed unchanged: a committed operation dedups
  // before the revision gate, while a never-applied one meets a definitive
  // mismatch that reconciles (repeatedly, inside the unchanged send budget).
  const reconcile = async () => {
    adoptStatus(ctx, (await apiRequest(ctx, sessionPath(ctx))).body);
    pending.revision = ctx.revision;
    await saveSessionRecord(ctx);
  };
  for (let attempt = 0; ; attempt += 1) {
    throwIfAborted(ctx);
    try {
      const path = pending.method === 'DELETE'
        ? `${pending.path}?operation_id=${encodeURIComponent(pending.operationId)}` : pending.path;
      const response = await apiRequest(ctx, path, {
        method: pending.method,
        json: pending.method === 'DELETE' ? undefined : { ...pending.body, operation_id: pending.operationId },
        headers: pending.intrinsic ? {} : { 'if-match': `"${pending.revision}"` },
      });
      const status = adoptStatus(ctx, response.body);
      ctx.pendingOperation = null;
      await saveSessionRecord(ctx);
      return status;
    } catch (error) {
      if (error instanceof TransferError && (error.code === 'authentication_required' || error.httpStatus === 401)) {
        const retry = await requestUi(ctx, 'auth', {
          message: '認証が切れました。別のタブでログインし直してから「再試行」を押してください。転送は中断せず待機しています。',
        });
        if (!retry) throw error;
        continue;
      }
      const mismatch = error.httpStatus === 412 || error.httpStatus === 428 || error.code === 'revision_mismatch';
      const retryable = isRetryable(error);
      if ((!mismatch && !retryable) || attempt + 1 >= MAX_CONTROL_ATTEMPTS) {
        // A definitive non-conflict rejection voids the persisted receipt. A
        // revision conflict or an exhausted retryable failure keeps it so a
        // later recovery can reconcile and replay the same identity.
        if (!mismatch && !retryable && error instanceof TransferError && error.httpStatus >= 400 && error.httpStatus < 500
          && error.httpStatus !== 401 && error.httpStatus !== 429) {
          ctx.pendingOperation = null;
          await saveSessionRecord(ctx);
        }
        throw error;
      }
      if (mismatch) {
        await reconcile();
      } else {
        const delay = retryDelayMs(attempt, error);
        progress(ctx, { phase: 'retry', label: `${pending.path} を再試行します (${Math.ceil(delay / 1000)} 秒後)`, attempt: attempt + 1 });
        await sleep(delay, ctx);
      }
    }
  }
}

/** Save the operation body AND original revision before making any request. */
async function sessionOperation(ctx, path, body = {}, { method = 'POST' } = {}) {
  if (ctx.pendingOperation) await recoverPendingOperation(ctx);
  if (!ctx.revision) throw new TransferError('protocol_error', '操作に必要な revision がありません。');
  ctx.pendingOperation = { path, body, method, revision: ctx.revision, operationId: crypto.randomUUID() };
  await saveSessionRecord(ctx);
  return recoverPendingOperation(ctx);
}

async function postAdvance(ctx) {
  if (ctx.pendingOperation) return recoverPendingOperation(ctx);
  if (!ctx.operationId) throw new TransferError('protocol_error', 'advance の operation_id がありません。');
  ctx.pendingOperation = { path: `${sessionPath(ctx)}/advance`, method: 'POST', body: {},
    intrinsic: true, operationId: ctx.operationId };
  await saveSessionRecord(ctx);
  return recoverPendingOperation(ctx);
}

async function advanceUntil(ctx, wanted, label) {
  for (;;) {
    throwIfAborted(ctx);
    const status = adoptStatus(ctx, (await withRetry(ctx, () => apiRequest(ctx, sessionPath(ctx)),
      { operation: '状態確認' })).body);
    const { state, next_action: next } = status;
    progress(ctx, { phase: label, state, nextAction: next, confirmed: status.confirmed_bytes,
      published: status.published_novels, error: status.error ? { message: String(status.error) } : null });
    if (wanted(state, next, status)) return status;
    if (isTerminal(state)) {
      throw new TransferError(`session_${state}`, `転送状態: ${state}。${status.error || ''}`, { revision: ctx.revision });
    }
    if (next === 'advance' || next === 'finalize_file') { await postAdvance(ctx); continue; }
    if (state === 'paused') throw new TransferError('session_paused', '転送は一時停止しています。');
    if (state === 'cancelling') { await sleep(500, ctx); continue; }
    throw new TransferError('protocol_error', `転送状態 ${state} の次の操作 ${next} を実行できません。`);
  }
}

async function resumeIfPaused(ctx) {
  if (ctx.pendingOperation) await recoverPendingOperation(ctx);
  let status = adoptStatus(ctx, (await withRetry(ctx, () => apiRequest(ctx, sessionPath(ctx)),
    { operation: '状態確認' })).body);
  if (status.state === 'cancelling') {
    return advanceUntil(ctx, (state) => state === 'cancelled', 'cancel');
  }
  if (status.state === 'paused' && ['advance', 'finalize_file'].includes(status.next_action)) {
    status = await advanceUntil(ctx, (state, next) => state === 'paused' && !['advance', 'finalize_file'].includes(next), 'pause');
  }
  if (status.state === 'paused') status = await sessionOperation(ctx, `${sessionPath(ctx)}/resume`);
  return status;
}

// ---------------------------------------------------------------------------
// 再開: IndexedDB の identity + secret + fingerprint
// ---------------------------------------------------------------------------

function restoreSessionRecord(ctx, record) {
  if (!record?.requestId || !record.secretHeader || !['imports', 'exports'].includes(record.direction)) {
    throw new TransferError('resume_missing', '再開用の identity がありません。明示的に新規開始してください。');
  }
  Object.assign(ctx, { requestId: record.requestId, sessionId: record.sessionId || '',
    direction: record.direction, secretHeader: record.secretHeader, fingerprint: record.fingerprint || '',
    fileName: record.fileName || '', ids: record.ids || [], policy: record.policy, selection: record.selection,
    conflictPolicy: record.conflictPolicy || 'skip', policyVersion: record.policyVersion ?? record.selection?.policy_version, createRequest: record.createRequest,
    pendingOperation: record.pendingOperation, manifestNextChunk: record.manifestNextChunk || 0,
    outputBytes: record.outputBytes || '0', outputClosed: record.outputClosed === true,
    revision: record.revision || '', status: null, operationId: '', receipts: record.receipts || [] });
}

async function recoverCreation(ctx) {
  if (ctx.sessionId) return;
  if (!ctx.createRequest) throw new TransferError('resume_missing', '保存された作成要求がありません。');
  const response = await withRetry(ctx, () => apiRequest(ctx, `/${ctx.direction}`,
    { method: 'POST', json: ctx.createRequest }), { operation: 'セッション作成の結果確認' });
  adoptStatus(ctx, response.body);
  await saveSessionRecord(ctx);
}

async function pendingSessions(ctx) {
  const sessions = [], records = await ctx.store.all();
  for (const record of records) {
    if (!record.direction || !record.secretHeader || (!record.sessionId && !record.createRequest)) continue;
    restoreSessionRecord(ctx, record);
    const item = { requestId: record.requestId, sessionId: record.sessionId || '', direction: record.direction, fileName: record.fileName || '',
      ids: record.ids || [], updatedAt: record.updatedAt || 0, resumeMode: 'continue' };
    if (!record.sessionId) {
      sessions.push({ ...item, state: 'creation_unconfirmed' });
      continue;
    }
    try {
      const status = adoptStatus(ctx, (await apiRequest(ctx, sessionPath(ctx))).body);
      if (['cancelled', 'expired', 'released'].includes(status.state)) {
        await ctx.store.remove(record.requestId);
        continue;
      }
      sessions.push({ ...item, state: status.state, resumeMode: status.resume_mode });
    } catch (error) {
      sessions.push({ ...item, state: 'unknown', error: error.message });
    }
  }
  const localIds = new Set(sessions.map((session) => session.sessionId).filter(Boolean));
  const remoteSessions = await managedSessions(ctx);
  for (const record of records) {
    if (record.managed && !remoteSessions.some((remote) => remote.session_id === record.sessionId && !isTerminal(remote.state))) {
      await ctx.store.remove(record.requestId);
    }
  }
  for (const remote of remoteSessions) {
    if (localIds.has(remote.session_id) || isTerminal(remote.state)) continue;
    sessions.push({ requestId: `admin:${remote.session_id}`, sessionId: remote.session_id, managed: true,
      revision: remote.revision, direction: remote.direction === 'import' ? 'imports' : 'exports',
      state: remote.state, resumeMode: 'none', fileName: '別のブラウザ・失われた再開情報の転送', updatedAt: 0 });
  }
  return sessions;
}

async function managedSessions(ctx) {
  const secret = ctx.secretHeader;
  ctx.secretHeader = '';
  try { return (await collectPages(ctx, '/sessions')).flatMap((page) => page.items); }
  finally { ctx.secretHeader = secret; }
}

async function discardManagedSession(ctx, payload) {
  if (typeof payload.sessionId !== 'string' || !payload.sessionId || !/^[0-9]+$/.test(payload.revision || '')) {
    throw new TransferError('protocol_error', '管理者取消に必要な session identity/revision がありません。');
  }
  const key = `admin:${payload.sessionId}`;
  let record = await ctx.store.get(key);
  if (!record?.managed) {
    record = { requestId: key, managed: true, sessionId: payload.sessionId, direction: payload.direction,
      revision: payload.revision, operationId: crypto.randomUUID(), updatedAt: Date.now() };
    await ctx.store.put(record);
  }
  ctx.secretHeader = '';
  try {
    for (let attempt = 0; ; attempt += 1) {
      try {
        await withRetry(ctx, () => apiRequest(ctx, `/sessions/${encodeURIComponent(record.sessionId)}?operation_id=${encodeURIComponent(record.operationId)}`,
          { method: 'DELETE', headers: { 'if-match': `"${record.revision}"` } }), { operation: '管理者による転送取消' });
        break;
      } catch (error) {
        if (error.httpStatus === 404 || error.httpStatus === 410) { await ctx.store.remove(key); return; }
        if (error.httpStatus !== 412 || attempt > 0) throw error;
        const latest = (await managedSessions(ctx)).find((session) => session.session_id === record.sessionId);
        if (!latest || isTerminal(latest.state)) { await ctx.store.remove(key); return; }
        record.revision = latest.revision; await ctx.store.put(record);
      }
    }
    // The authenticated listing advances cancelling sessions by one bounded unit.
    for (;;) {
      throwIfAborted(ctx);
      const latest = (await managedSessions(ctx)).find((session) => session.session_id === record.sessionId);
      if (!latest || isTerminal(latest.state)) { await ctx.store.remove(key); return; }
      if (latest.state !== 'cancelling') throw new TransferError('cancel_unconfirmed', '取消結果が未確定です。同じ操作 identity を保持しています。');
      progress(ctx, { phase: 'cancel', label: '未公開分の回復・回収を待っています…' });
      await sleep(500, ctx);
    }
  } catch (error) { throw error; }
}

async function discardSession(ctx, payload) {
  if (payload.managed) return discardManagedSession(ctx, payload);
  const requestId = payload.requestId;
  const record = requestId ? await ctx.store.get(requestId) : null;
  if (!record) return;
  restoreSessionRecord(ctx, record);
  try {
    await recoverCreation(ctx);
    if (ctx.pendingOperation) await recoverPendingOperation(ctx);
    const status = adoptStatus(ctx, (await withRetry(ctx, () => apiRequest(ctx, sessionPath(ctx)),
      { operation: '状態確認' })).body);
    if (!isTerminal(status.state)) {
      await sessionOperation(ctx, sessionPath(ctx), {}, { method: 'DELETE' });
      await advanceUntil(ctx, (state) => state === 'cancelled' || isTerminal(state), 'cancel');
    }
  } catch (error) {
    if (error.httpStatus !== 404 && error.httpStatus !== 410) throw error;
  }
  await ctx.store.remove(requestId);
}

async function saveSessionRecord(ctx, extra = {}) {
  if (!ctx.requestId) return;
  await ctx.store.put({
    requestId: ctx.requestId, sessionId: ctx.sessionId, direction: ctx.direction,
    secretHeader: ctx.secretHeader, fingerprint: ctx.fingerprint || '', fileName: ctx.fileName || '',
    ids: ctx.ids || [], policy: ctx.policy, selection: ctx.selection, conflictPolicy: ctx.conflictPolicy || 'skip',
    policyVersion: ctx.policyVersion ?? ctx.selection?.policy_version ?? ctx.capabilities.storage.policy_version,
    manifestNextChunk: ctx.manifestNextChunk || 0, createRequest: ctx.createRequest,
    pendingOperation: ctx.pendingOperation, revision: ctx.revision || '',
    receipts: ctx.receipts || [], outputBytes: ctx.outputBytes || '0', outputClosed: ctx.outputClosed === true, updatedAt: Date.now(), ...extra,
  });
}

async function freshCreationTicket(ctx) {
  const capabilities = (await withRetry(ctx, () => apiRequest(ctx, '/capabilities'),
    { operation: '転送環境の再確認' })).body;
  if (capabilities?.protocol_version !== PROTOCOL_VERSION
    || capabilities?.build?.wasm_build_id !== ctx.codec.build_id
    || String(capabilities?.build?.abi_version) !== String(ABI_VERSION)) {
    throw new TransferError('abi_mismatch', 'サーバーが更新されました。ページを再読み込みしてください。');
  }
  if (capabilities.storage?.policy_version !== ctx.capabilities.storage?.policy_version) {
    throw new TransferError('policy_changed', '保存先の file policy が変わりました。内容を選び直してください。');
  }
  if (!capabilities.creation_ticket) throw new TransferError('protocol_error', 'creation_ticket がありません。');
  ctx.capabilities = capabilities;
  return capabilities.creation_ticket;
}

// ---------------------------------------------------------------------------
// catalog 分類 → role 集計 → file_id 投影
// ---------------------------------------------------------------------------

/**
 * catalog_page の全項目を収集する。file 項目は entry ordinal・role・
 * runtime/complete ごとの send 区分を持つ。entry ordinal と wire file_id は
 * 別物: file_id は選択後に manifest 順の連番として決まる。
 */
async function collectCatalog(ctx, handle) {
  const summaries = { runtime: emptySummary(), complete: emptySummary() };
  let cursor = null, entries = 0;
  const seen = new Set();
  for (;;) {
    throwIfAborted(ctx);
    const json = await ctx.wasm.catalog_page(handle, cursor);
    const page = JSON.parse(json);
    if (!Array.isArray(page.items) || page.items.length > ctx.limits.pageItems
      || textEncoder.encode(json).byteLength > ctx.limits.controlJsonBytes || (entries += page.items.length) > 60000) {
      throw new TransferError('metadata_budget', 'catalog のページ数・サイズが上限を超えました。');
    }
    for (const profile of ['runtime', 'complete']) mergeSummary(summaries[profile], summarizeCatalog(page.items, profile));
    if (page.next_cursor == null) return { runtime: finishSummary(summaries.runtime), complete: finishSummary(summaries.complete) };
    if (!page.items.length || seen.has(page.next_cursor)) throw new TransferError('protocol_error', 'catalog cursor が進みません。');
    seen.add(page.next_cursor); cursor = String(page.next_cursor);
    progress(ctx, { phase: 'catalog', entries });
  }
}

function emptySummary() { return { roles: new Map(), totalFiles: 0, totalBytes: 0n }; }
function mergeSummary(into, summary) {
  into.totalFiles += summary.totalFiles; into.totalBytes += BigInt(summary.totalBytes);
  for (const bucket of summary.roles) {
    const old = into.roles.get(bucket.role);
    if (!old) { into.roles.set(bucket.role, { ...bucket, reasons: [...bucket.reasons] }); continue; }
    for (const key of ['requiredFiles', 'optionalFiles', 'optionalDefault', 'excludedFiles']) old[key] += bucket[key];
    for (const key of ['requiredBytes', 'optionalBytes', 'excludedBytes']) old[key] = (BigInt(old[key]) + BigInt(bucket[key])).toString();
    old.reasons = [...new Set([...old.reasons, ...bucket.reasons])];
  }
}
function finishSummary(summary) { return { roles: [...summary.roles.values()], totalFiles: summary.totalFiles, totalBytes: summary.totalBytes.toString() }; }

const SEND_REQUIRED = 'required';
const SEND_OPTIONAL = 'optional';
const SEND_EXCLUDED = 'excluded';

/** file item の profile に対する send 区分を返す。 */
function sendClass(item, profile) {
  const policy = profile === 'complete' ? item?.complete : item?.runtime;
  const send = policy?.send;
  if (send === SEND_REQUIRED || send === SEND_OPTIONAL || send === SEND_EXCLUDED) return send;
  return SEND_REQUIRED; // 分類不能なものは落とさない。
}

/** role ごとの件数/byte/選択可否を集計する。 */
export function summarizeCatalog(items, profile) {
  const roles = new Map();
  let totalFiles = 0, totalBytes = 0n;
  for (const item of items) {
    if (item?.kind !== 'file') continue;
    const role = String(item.role ?? 'unknown');
    const size = parseDecimal(item.size, 0n);
    const cls = sendClass(item, profile);
    const policy = profile === 'complete' ? item.complete : item.runtime;
    const bucket = roles.get(role) ?? {
      role, label: roleLabel(role),
      requiredFiles: 0, requiredBytes: 0n,
      optionalFiles: 0, optionalBytes: 0n, optionalDefault: 0,
      excludedFiles: 0, excludedBytes: 0n,
      reasons: [],
    };
    const reason = policy?.reason ? String(policy.reason) : '';
    if (cls === SEND_REQUIRED) { bucket.requiredFiles += 1; bucket.requiredBytes += size; }
    else if (cls === SEND_OPTIONAL) {
      bucket.optionalFiles += 1; bucket.optionalBytes += size;
      if (policy?.default === true) bucket.optionalDefault += 1;
      if (reason && !bucket.reasons.includes(reason)) bucket.reasons.push(reason);
    } else {
      bucket.excludedFiles += 1; bucket.excludedBytes += size;
      if (reason && !bucket.reasons.includes(reason)) bucket.reasons.push(reason);
    }
    roles.set(role, bucket);
    totalFiles += 1;
    totalBytes += size;
  }
  const ordered = [...roles.values()].sort((a, b) => {
    const ia = ROLE_ORDER.indexOf(a.role), ib = ROLE_ORDER.indexOf(b.role);
    return (ia === -1 ? ROLE_ORDER.length : ia) - (ib === -1 ? ROLE_ORDER.length : ib);
  });
  const text = (v) => v.toString(10);
  return {
    roles: ordered.map((bucket) => ({
      role: bucket.role, label: bucket.label,
      requiredFiles: bucket.requiredFiles, requiredBytes: text(bucket.requiredBytes),
      optionalFiles: bucket.optionalFiles, optionalBytes: text(bucket.optionalBytes),
      optionalDefault: bucket.optionalDefault,
      excludedFiles: bucket.excludedFiles, excludedBytes: text(bucket.excludedBytes),
      reasons: bucket.reasons,
    })),
    totalFiles,
    totalBytes: text(totalBytes),
  };
}

// ---------------------------------------------------------------------------
// 取り込み
// ---------------------------------------------------------------------------

async function runImport(ctx, payload) {
  const file = payload.file;
  if (!(file instanceof Blob) || !Number.isSafeInteger(file.size)) {
    throw new TransferError('invalid_input', '安全に範囲読出しできる ZIP ファイルを選択してください。');
  }
  const resume = payload.requestId ? await ctx.store.get(payload.requestId) : null;
  if (payload.requestId && !resume) throw new TransferError('resume_missing', '再開用の記録がありません。');
  ctx.direction = 'imports';
  ctx.fileName = file.name || 'archive.zip';
  phase(ctx, 'validate', 'ZIP 全体の CRC・サイズ・SHA を検証しています…');
  const readRange = async (offset, length) => {
    throwIfAborted(ctx);
    if (!Number.isSafeInteger(offset) || !Number.isSafeInteger(length) || offset < 0 || length < 0
      || length > ctx.limits.codecIoBytes || offset + length > file.size) {
      throw new TransferError('protocol_error', 'codec が不正な ZIP 読出し範囲を要求しました。');
    }
    const bytes = new Uint8Array(await file.slice(offset, offset + length).arrayBuffer());
    if (bytes.byteLength !== length) throw new TransferError('truncated_frame', 'ZIP を要求長だけ読み出せません。');
    return bytes;
  };
  const limits = JSON.stringify({ max_entries: 50000, max_novels: 10000,
    max_archive_manifest_bytes: ctx.limits.manifestBytes, metadata_budget: ctx.limits.metadataBytes });
  const handle = await ctx.wasm.open_archive(readRange, String(file.size), limits);
  let spool;
  try {
    let lastReport = 0;
    const validated = JSON.parse(await ctx.wasm.validate_archive(handle, (completed, total) => {
      throwIfAborted(ctx);
      if (Date.now() - lastReport < 120 && completed !== total) return;
      lastReport = Date.now();
      progress(ctx, { phase: 'validate', processed: decimal(completed), total: decimal(total) });
    }));
    throwIfAborted(ctx);
    const fingerprint = validated.fingerprint;
    if (!/^[a-f0-9]{64}$/.test(fingerprint || '')) throw new TransferError('protocol_error', 'source fingerprint が不正です。');
    if (resume && resume.fingerprint !== fingerprint) {
      throw new TransferError('source_mismatch', '選択した ZIP は中断した ZIP と内容が異なります。元の ZIP を選び直してください。新しい session は作成していません。');
    }
    ctx.fingerprint = fingerprint;
    const storage = ctx.capabilities.storage;
    const forbidden = new Set(storage.unpersistable_roles || []);
    let selection;
    if (resume) {
      restoreSessionRecord(ctx, resume);
      await recoverCreation(ctx);
      if (ctx.pendingOperation) await recoverPendingOperation(ctx);
      const retained = adoptStatus(ctx, (await apiRequest(ctx, sessionPath(ctx))).body);
      if (retained.state === 'completed' || retained.state === 'completed_with_errors') {
        const summary = summarizePlan(await collectPages(ctx, `${sessionPath(ctx)}/plan`));
        await ctx.store.remove(ctx.requestId);
        return { state: retained.state, counts: summary.counts, errors: summary.errors };
      }
      if (!resume.selection || resume.selection.policy_version !== storage.policy_version
        || resume.selection.allow_raw !== !forbidden.has('raw')) {
        throw new TransferError('policy_changed', '保存先 policy が変わったため、この選択のまま再開できません。');
      }
      selection = resume.selection;
    } else {
      phase(ctx, 'catalog', '送信前に内容を分類しています…');
      const byProfile = await collectCatalog(ctx, handle);
      const decision = await requestUi(ctx, 'import-select', {
        fileName: ctx.fileName, fileSize: decimal(file.size), novels: validated.novels, files: validated.files,
        uncompressed: validated.total_uncompressed,
        byProfile,
        policy: { version: storage.policy_version, forbiddenRoles: [...forbidden] },
        resumeSupported: ctx.store.persistent, availableBytes: storage.available_bytes,
      });
      if (!decision?.confirm) throw new TransferError('aborted', '取り込みを開始していません。');
      selection = canonicalSelection(decision, storage.policy_version, forbidden);
      const summary = byProfile[selection.profile];
      if (summary.roles.some((bucket) => bucket.requiredFiles && forbidden.has(bucket.role))) {
        throw new TransferError('required_role_unsupported', '必須の作品データを保存できないため、本文を送信せず停止しました。');
      }
      ctx.policy = { profile: selection.profile, extra_roles: selection.extra_roles };
      ctx.conflictPolicy = decision.conflictPolicy === 'overwrite' ? 'overwrite' : 'skip';
    }
    ctx.selection = selection;
    phase(ctx, 'manifest', '確認済みの選択だけで転送 manifest を作成しています…');
    spool = await ByteSpool.open(`import-${crypto.randomUUID()}`);
    const manifestInfo = JSON.parse(await ctx.wasm.write_transfer_manifest(handle, JSON.stringify(selection), async (chunk) => {
      throwIfAborted(ctx);
      const bytes = chunk instanceof Uint8Array ? chunk : new Uint8Array(chunk);
      if (bytes.byteLength > ctx.limits.codecIoBytes) throw new TransferError('protocol_error', 'manifest 出力が64 KiBを超えました。');
      await spool.append(bytes);
      progress(ctx, { phase: 'manifest', produced: String(spool.size) });
    }));
    await spool.finalize();
    const wireSize = parseDecimal(manifestInfo.wire_size, -1n);
    const wireSha = manifestInfo.wire_sha256;
    if (wireSize !== BigInt(spool.size) || wireSize > BigInt(ctx.limits.manifestBytes)
      || !/^[a-f0-9]{64}$/.test(wireSha || '') || manifestInfo.fingerprint !== fingerprint) {
      throw new TransferError('protocol_error', '投影 manifest のサイズ・SHA・fingerprint が不正です。');
    }
    if (resume) {
      if (resume.createRequest?.manifest_sha256 !== wireSha || resume.createRequest?.manifest_size !== wireSize.toString()) {
        throw new TransferError('projection_mismatch', '元の投影 manifest と一致しないため再開を停止しました。');
      }
      phase(ctx, 'resume', '確定 receipt とローカル prefix を検証して再開します…');
      await recoverCreation(ctx);
      await resumeIfPaused(ctx);
    } else {
      await createImportSession(ctx, selection, wireSize, wireSha);
    }
    const result = await driveImport(ctx, handle, spool, wireSize, wireSha);
    await ctx.store.remove(ctx.requestId);
    return result;
  } finally {
    await spool?.close({ remove: true });
    ctx.wasm.close_archive(handle);
  }
}

export function canonicalSelection(decision, policyVersion, forbidden = new Set()) {
  const profile = decision.profile === 'complete' ? 'complete' : 'runtime';
  const allowed = profile === 'runtime' ? ['raw', 'history'] : ['raw', 'history', 'generated'];
  const extraRoles = [...new Set(decision.roles || [])].filter((role) => allowed.includes(role) && !forbidden.has(role)).sort();
  return { profile, extra_roles: extraRoles, policy_version: policyVersion, allow_raw: !forbidden.has('raw') };
}

async function createImportSession(ctx, selection, wireSize, wireSha) {
  const ticket = await freshCreationTicket(ctx);
  ctx.requestId = crypto.randomUUID();
  ctx.secretHeader = base64UrlEncode(randomSecretBytes());
  ctx.createRequest = { request_id: ctx.requestId, creation_ticket: ticket, source_fingerprint: ctx.fingerprint,
    profile: selection.profile, extra_roles: selection.extra_roles, policy_version: selection.policy_version,
    conflict_policy: ctx.conflictPolicy, manifest_size: wireSize.toString(), manifest_sha256: wireSha };
  await saveSessionRecord(ctx);
  await recoverCreation(ctx);
}

/**
 * session の現在状態から残りの処理を進める。
 * receiving_manifest → (manifest chunks + seal) → planning → transferring →
 * ready → plan 確認 → publishing → completed。
 */
async function driveImport(ctx, handle, spool, wireSize, wireSha) {
  let stale = false;
  for (;;) {
    throwIfAborted(ctx);
    const status = adoptStatus(ctx, (await withRetry(ctx, () => apiRequest(ctx, sessionPath(ctx)),
      { operation: '状態確認' })).body);
    progress(ctx, { state: status.state, nextAction: status.next_action, confirmed: status.confirmed_bytes, published: status.published_novels });
    if (status.state === 'completed' || status.state === 'completed_with_errors') {
      const summary = summarizePlan(await collectPages(ctx, `${sessionPath(ctx)}/plan`));
      return { state: status.state, counts: summary.counts, errors: summary.errors, uploaded: ctx.uploadStats };
    }
    if (isTerminal(status.state)) throw new TransferError(`session_${status.state}`, String(status.error || status.state));
    if (status.state === 'receiving_manifest') {
      phase(ctx, 'manifest_upload', '転送 manifest の未確定部分を送信しています…');
      await uploadManifest(ctx, spool, wireSize, status);
      await sessionOperation(ctx, `${sessionPath(ctx)}/manifest/seal`, { size: wireSize.toString(), sha256: wireSha });
      await spool.close({ remove: true });
      continue;
    }
    if (status.next_action === 'advance' || status.next_action === 'finalize_file') {
      phase(ctx, status.state === 'publishing' ? 'publish' : 'planning',
        status.state === 'publishing' ? '作品ごとの公開を確定しています…' : 'サーバーが計画・検証を進めています…');
      await postAdvance(ctx);
      continue;
    }
    if (status.state === 'transferring') {
      const files = await collectPages(ctx, `${sessionPath(ctx)}/files`);
      ctx.uploadStats = await uploadFiles(ctx, handle, files);
      adoptStatus(ctx, (await apiRequest(ctx, sessionPath(ctx))).body);
      if (['advance', 'finalize_file'].includes(ctx.status.next_action)) continue;
      phase(ctx, 'ready', '全ファイルの検証済み receipt を確認しています…');
      await sessionOperation(ctx, `${sessionPath(ctx)}/ready`);
      continue;
    }
    if (status.state === 'ready') {
      const summary = summarizePlan(await collectPages(ctx, `${sessionPath(ctx)}/plan`));
      if (!summary.planRevision || summary.planRevision !== status.plan_revision) {
        throw new TransferError('plan_stale', '確認中に plan が変わりました。転送を再開して最新の計画を確認してください。');
      }
      const confirmed = await requestUi(ctx, 'plan-confirm', { ...summary, stale });
      if (!confirmed) {
        await sessionOperation(ctx, `${sessionPath(ctx)}/pause`);
        throw new TransferError('aborted', '公開を一時停止しました。検証済みのデータは保持しています。');
      }
      try {
        await sessionOperation(ctx, `${sessionPath(ctx)}/publish`, { plan_revision: summary.planRevision });
      } catch (error) {
        if (error.code !== 'plan_stale') throw error;
        stale = true;
      }
      continue;
    }
    if (status.state === 'paused') throw new TransferError('session_paused', '転送は一時停止しています。');
    throw new TransferError('protocol_error', `取り込み状態 ${status.state}/${status.next_action} を処理できません。`);
  }
}

/** Manifest path n carries exactly one framed record with intrinsic identity n. */
async function uploadManifest(ctx, spool, wireSize, status) {
  const total = Number(wireSize), unit = ctx.limits.chunkBytes;
  let index = status.manifest_next_chunk_index;
  const confirmed = parseDecimal(status.manifest_confirmed_bytes, -1n);
  if (!Number.isSafeInteger(index) || index < 0 || confirmed < 0n
    || confirmed !== BigInt(Math.min(index * unit, total))) {
    throw new TransferError('protocol_error', 'manifest の確定位置が不正です。');
  }
  for (let offset = Number(confirmed); offset < total; index += 1) {
    throwIfAborted(ctx);
    const length = Math.min(unit, total - offset);
    const payload = await spool.read(offset, length);
    const builder = new BatchBuilder(ctx.limits);
    if (!builder.addRecord({ file_id: MANIFEST_FILE_ID, chunk_index: index, final: offset + length === total,
      sha256: await sha256Hex(payload) }, payload)) throw new TransferError('protocol_error', 'manifest record が上限を超えました。');
    await sendBatch(ctx, `${sessionPath(ctx)}/manifest/chunks/${index}`, builder.build());
    offset += length;
    ctx.manifestNextChunk = index + 1;
    await saveSessionRecord(ctx);
    progress(ctx, { phase: 'manifest_upload', sent: String(offset), total: String(total),
      confirmed: ctx.status.confirmed_bytes, state: ctx.status.state });
  }
}

async function sendBatch(ctx, path, body) {
  const records = decodeFrames(body, ctx.limits);
  if (!records.length || records.some(({ header }) => !header.sha256)) {
    throw new TransferError('protocol_error', 'upload のレコード SHA がありません。');
  }
  const response = await withRetry(ctx, () => apiRequest(ctx, path, {
    method: 'PUT', binary: body, headers: { 'x-transfer-record-count': String(records.length) },
  }), { operation: 'バッチ送信' });
  await checkBatchResponse(ctx, response, records);
  return response;
}

async function checkBatchResponse(ctx, response, records) {
  const body = response.body;
  if (!Array.isArray(body?.receipts) || body.receipts.length > records.length
    || (!body.failure && body.receipts.length !== records.length)) {
    throw new TransferError('protocol_error', 'バッチ内の全レコードの receipt が返されませんでした。');
  }
  for (let index = 0; index < body.receipts.length; index += 1) {
    const receipt = body.receipts[index], expected = records[index].header;
    if (receipt.file_id !== expected.file_id || receipt.chunk_index !== expected.chunk_index
      || !['accepted', 'duplicate', 'sealed'].includes(receipt.outcome)
      || parseDecimal(receipt.confirmed_bytes, -1n) !== BigInt(expected.chunk_index) * BigInt(ctx.limits.chunkBytes) + BigInt(expected.payload_size)
      || !/^[a-f0-9]{64}$/.test(receipt.confirmed_prefix_sha256 || '')) {
      throw new TransferError('protocol_error', 'バッチの receipt が送信したレコードに対応しません。');
    }
    const finalSha = expected.file_id === MANIFEST_FILE_ID ? ctx.createRequest?.manifest_sha256
      : ctx.expectedFileHashes?.get(expected.file_id);
    if (expected.final && finalSha && receipt.confirmed_prefix_sha256 !== finalSha) {
      throw new TransferError('hash_mismatch', 'final receipt の whole-file SHA がローカルの検証済み値と一致しません。');
    }
  }
  adoptStatus(ctx, body.status || body);
  ctx.receipts = body.receipts;
  await saveSessionRecord(ctx);
  if (body.failure) {
    const failure = body.failure;
    throw new TransferError(failure.code || 'chunk_conflict',
      `バッチの一部が確定した後に失敗しました: ${failure.message || failure.code}。再開時に確定 receipt を照合します。`);
  }
}

async function uploadFiles(ctx, handle, filePages) {
  const byFileId = new Map();
  for (const page of filePages) {
    for (const file of page?.items ?? []) {
      if (!Number.isInteger(file?.file_id) || file.file_id < 0 || file.file_id !== file.manifest_file_index
        || byFileId.has(file.file_id)) throw new TransferError('protocol_error', 'file_id の対応または連番が不正です。');
      byFileId.set(file.file_id, file);
    }
  }
  ctx.expectedFileHashes = new Map([...byFileId].map(([id, file]) => [id, file.sha256]));
  const chunkBytes = ctx.limits.chunkBytes;
  let totalBytes = 0n, sentBytes = 0n, fileCount = 0;
  const targets = [];
  for (const [fileId, file] of [...byFileId.entries()].sort((a, b) => a[0] - b[0])) {
    const state = String(file.state ?? '');
    if (!file.upload_required || ['verified', 'reused', 'skipped'].includes(state)) continue;
    const size = parseDecimal(file.size, -1n);
    const confirmed = parseDecimal(file.confirmed_bytes, -1n);
    if (size < 0n || confirmed < 0n || confirmed > size) {
      throw new TransferError('protocol_error', 'file のサイズまたは確定 offset が不正です。');
    }
    totalBytes += size - confirmed;
    targets.push({ fileId, file, size, confirmed });
    fileCount += 1;
  }
  if (targets.length) {
    phase(ctx, 'upload', `${fileCount.toLocaleString()} ファイルを送信しています…`, { total: totalBytes.toString(10) });
  }

  let builder = new BatchBuilder(ctx.limits);
  const flush = async () => {
    const body = builder.build();
    if (!body) return;
    builder = new BatchBuilder(ctx.limits);
    await sendBatch(ctx, `${sessionPath(ctx)}/chunks`, body);
    progress(ctx, { phase: 'upload', sent: sentBytes.toString(), total: totalBytes.toString(),
      confirmed: ctx.status.confirmed_bytes, state: ctx.status.state });
  };

  for (const target of targets) {
    throwIfAborted(ctx);
    const { fileId, file, size, confirmed } = target;
    const remaining = size - confirmed;
    const prefixSha = confirmed > 0n ? String(file.confirmed_prefix_sha256 ?? '') : null;
    if (confirmed > 0n && !prefixSha) {
      throw new TransferError('protocol_error', `file_id ${fileId} の確定 prefix SHA がサーバーから返っていません。`);
    }
    const declaredNext = file.next_chunk_index !== undefined ? Number(file.next_chunk_index) : null;
    let chunkIndex = declaredNext ?? resumeChunkIndex(confirmed, chunkBytes);
    if (declaredNext !== null && declaredNext !== resumeChunkIndex(confirmed, chunkBytes)) {
      throw new TransferError('protocol_error',
        `file_id ${fileId} の next_chunk_index ${declaredNext} が confirmed_bytes と一致しません。`);
    }
    let produced = confirmed;

    const recordPayload = new Uint8Array(Math.min(chunkBytes, Number(remaining)));
    let recordLength = 0;
    let recordCap = Math.min(chunkBytes, Number(remaining));
    const emitRecord = async (final) => {
      const payload = recordPayload.subarray(0, recordLength);
      const header = {
        file_id: fileId, chunk_index: chunkIndex, final,
        sha256: await sha256Hex(payload),
      };
      if (!builder.addRecord(header, payload)) {
        await flush();
        if (!builder.addRecord(header, payload)) {
          throw new TransferError('transfer_chunk_too_large', 'ファイル record が batch に収まりません。');
        }
      }
      sentBytes += BigInt(recordLength);
      chunkIndex += 1;
      recordLength = 0;
      recordCap = Math.min(chunkBytes, Number(size - produced));
      progress(ctx, { phase: 'upload', produced: sentBytes.toString(), total: totalBytes.toString(), fileId });
    };

    const streamResult = JSON.parse(await ctx.wasm.stream_entry(
      handle,
      fileId,
      confirmed.toString(),
      prefixSha,
      async (chunk) => {
        throwIfAborted(ctx);
        const bytes = chunk instanceof Uint8Array ? chunk : new Uint8Array(chunk);
        if (bytes.byteLength > ctx.limits.codecIoBytes) {
          throw new TransferError('protocol_error', 'codec が 64 KiB を超える出力を返しました。');
        }
        let offset = 0;
        while (offset < bytes.byteLength) {
          const space = recordCap - recordLength;
          if (space <= 0) throw new TransferError('protocol_error', 'WASM が宣言サイズを超えて出力しました。');
          const take = Math.min(space, bytes.byteLength - offset);
          recordPayload.set(bytes.subarray(offset, offset + take), recordLength);
          recordLength += take;
          produced += BigInt(take);
          offset += take;
          // 最終 record は stream_entry の全 SHA 検証が終わってから seal する。
          if (recordLength === recordCap && produced < size) await emitRecord(false);
        }
      },
    ));
    const emitted = parseDecimal(streamResult.emitted, -1n);
    if (produced !== size || emitted !== remaining) {
      throw new TransferError('protocol_error', `file_id ${fileId} の展開サイズが manifest と一致しません。`);
    }
    if (!/^[a-f0-9]{64}$/.test(file.sha256 || '') || file.sha256 !== streamResult.sha256
      || parseDecimal(streamResult.file_size, -1n) !== size) {
      throw new TransferError('hash_mismatch', `file_id ${fileId} の SHA がサーバー申告と一致しません。`);
    }
    await emitRecord(true);
  }
  await flush();
  return { files: fileCount, bytes: sentBytes.toString(10) };
}

function summarizePlan(planPages) {
  const counts = { create: 0, overwrite: 0, skip: 0, reject: 0, failed: 0, other: 0 };
  const errors = [], samples = [];
  let planRevision = null, total = 0;
  for (const page of planPages) {
    if (planRevision !== null && planRevision !== page.plan_revision) {
      throw new TransferError('plan_stale', 'ページ取得中に plan revision が変わりました。再開して再確認してください。');
    }
    planRevision = page.plan_revision;
    for (const item of page.items) {
      total += 1;
      const receipt = item.receipt;
      const action = receipt
        ? ({ created: 'create', overwritten: 'overwrite', skipped: 'skip', failed: 'failed', rolled_back: 'failed' }[receipt.outcome] || 'other')
        : item.action;
      counts[action in counts ? action : 'other'] += 1;
      const title = String(item.title || item.toc_url || '');
      if (receipt?.error || action === 'reject') errors.push(`${title}: ${receipt?.error || item.reason || '拒否'}`);
      if (samples.length < 30) samples.push(`${action}: ${title}`);
    }
  }
  return { counts, errors, samples, total, planRevision };
}

async function forEachPage(ctx, path, visit) {
  const seen = new Set();
  let cursor = '', items = 0;
  for (;;) {
    throwIfAborted(ctx);
    const query = cursor ? `?cursor=${encodeURIComponent(cursor)}` : '';
    const page = (await withRetry(ctx, () => apiRequest(ctx, `${path}${query}`), { operation: 'ページ取得' })).body;
    if (!Array.isArray(page?.items) || page.items.length > ctx.limits.pageItems || (items += page.items.length) > 60000) {
      throw new TransferError('metadata_budget', '不正または上限を超えたページ応答です。');
    }
    await visit(page);
    if (page.next_cursor == null) return;
    if (!page.items.length || seen.has(page.next_cursor)) throw new TransferError('protocol_error', 'ページ cursor が進みません。');
    seen.add(page.next_cursor); cursor = String(page.next_cursor);
  }
}

async function collectPages(ctx, path) {
  const pages = [];
  let bytes = 0;
  await forEachPage(ctx, path, (page) => {
    if (path.endsWith('/files')) {
      page.items = page.items.map((file) => ({ file_id: file.file_id, manifest_file_index: file.manifest_file_index,
        size: file.size, sha256: file.sha256, state: file.state, upload_required: file.upload_required,
        confirmed_bytes: file.confirmed_bytes, next_chunk_index: file.next_chunk_index, confirmed_prefix_sha256: file.confirmed_prefix_sha256 }));
    }
    bytes += textEncoder.encode(JSON.stringify(page)).byteLength;
    if (bytes > ctx.limits.metadataBytes) throw new TransferError('metadata_budget', '転送制御のメタデータ予算を超えました。');
    pages.push(page);
  });
  return pages;
}

async function collectExportCatalog(ctx) {
  const summaries = { runtime: emptySummary(), complete: emptySummary() }, items = [];
  let bytes = 0;
  await forEachPage(ctx, `${sessionPath(ctx)}/catalog`, (page) => {
    const display = page.items.map((item) => ({ ...item, kind: 'file' }));
    for (const profile of ['runtime', 'complete']) mergeSummary(summaries[profile], summarizeCatalog(display, profile));
    for (const item of page.items) {
      const identity = { novel_ordinal: item.novel_ordinal, logical_path: item.logical_path, size: item.size, role: item.role };
      bytes += textEncoder.encode(JSON.stringify(identity)).byteLength;
      if (bytes > ctx.limits.metadataBytes) throw new TransferError('metadata_budget', 'catalog identity のメタデータ予算を超えました。');
      items.push(identity);
    }
  });
  return { items, byProfile: { runtime: finishSummary(summaries.runtime), complete: finishSummary(summaries.complete) } };
}

// ---------------------------------------------------------------------------
// 書き出し
// ---------------------------------------------------------------------------

async function runExport(ctx, payload) {
  const resume = payload.requestId ? await ctx.store.get(payload.requestId) : null;
  if (payload.requestId && !resume) throw new TransferError('resume_missing', '再開用の記録がありません。');
  if (resume) {
    restoreSessionRecord(ctx, resume);
    if (ctx.direction !== 'exports') throw new TransferError('resume_missing', '書き出し session ではありません。');
    await recoverCreation(ctx);
    if (ctx.pendingOperation) await recoverPendingOperation(ctx);
    const retained = adoptStatus(ctx, (await apiRequest(ctx, sessionPath(ctx))).body);
    if (retained.state !== 'released' && ctx.policyVersion !== ctx.capabilities.storage.policy_version) {
      throw new TransferError('policy_changed', '選択を確定した file policy と保存先が変わりました。この snapshot のまま再開できません。');
    }
    await resumeIfPaused(ctx);
  } else {
    const ids = [...new Set((payload.ids || []).map(Number))];
    if (!ids.length || ids.some((id) => !Number.isSafeInteger(id) || id < 0)) {
      throw new TransferError('invalid_input', '書き出す作品 ID が不正です。');
    }
    ctx.direction = 'exports'; ctx.ids = ids;
    await createExportSession(ctx, ids);
  }
  let status;
  for (;;) {
    throwIfAborted(ctx);
    status = adoptStatus(ctx, (await withRetry(ctx, () => apiRequest(ctx, sessionPath(ctx)),
      { operation: '状態確認' })).body);
    if (status.state === 'released') {
      await requestUi(ctx, 'sink-abort', {});
      await ctx.store.remove(ctx.requestId);
      return { state: 'released', alreadyCompleted: true, bytes: resume?.outputBytes || '0' };
    }
    if (isTerminal(status.state)) throw new TransferError(`session_${status.state}`, String(status.error || status.state));
    if (status.state === 'ready' || status.state === 'reading') break;
    if (status.state === 'selecting') {
      const catalogRevision = status.catalog_revision;
      if (typeof catalogRevision !== 'string' || !/^[0-9]+$/.test(catalogRevision)) {
        throw new TransferError('protocol_error', 'catalog revision がありません。');
      }
      const { items, byProfile } = await collectExportCatalog(ctx);
      const storage = ctx.capabilities.storage;
      const forbidden = new Set(storage.unpersistable_roles || []);
      const decision = await requestUi(ctx, 'export-select', { ids: ctx.ids, byProfile,
        totalFiles: byProfile.runtime.totalFiles, totalBytes: byProfile.runtime.totalBytes,
        policy: { version: storage.policy_version, forbiddenRoles: [...forbidden] },
        resumeSupported: ctx.store.persistent, availableBytes: storage.available_bytes });
      if (!decision?.confirm) {
        await sessionOperation(ctx, `${sessionPath(ctx)}/pause`);
        throw new TransferError('aborted', 'snapshot を作成せず一時停止しました。');
      }
      const selection = canonicalSelection(decision, storage.policy_version, forbidden);
      ctx.selection = selection;
      ctx.policy = { profile: selection.profile, extra_roles: selection.extra_roles };
      const digest = await selectionDigest(selection.profile, selection.extra_roles, catalogRevision, items);
      phase(ctx, 'select', '選択した file identity を確定しています…');
      await sessionOperation(ctx, `${sessionPath(ctx)}/select`, { profile: selection.profile,
        extra_roles: selection.extra_roles, catalog_revision: catalogRevision, selection_digest: digest });
      continue;
    }
    if (status.next_action === 'advance' || status.next_action === 'finalize_file') {
      phase(ctx, status.state === 'cataloguing' ? 'catalog' : 'prepare',
        status.state === 'cataloguing' ? '実バイトを読む前に catalog を作成しています…' : '選択したファイルだけの snapshot を準備しています…');
      progress(ctx, { state: status.state, nextAction: status.next_action, confirmed: status.confirmed_bytes, published: status.published_novels });
      await postAdvance(ctx); continue;
    }
    throw new TransferError('protocol_error', `書き出し状態 ${status.state}/${status.next_action} を処理できません。`);
  }

  phase(ctx, 'manifest', '固定した snapshot の manifest を受信しています…');
  const manifest = await fetchExportManifest(ctx);
  const reader = createExportReader(ctx, manifest.spool);
  let written = 0n;
  try {
    phase(ctx, 'download', '端末の WASM で ZIP を作成しています…');
    await ctx.wasm.build_zip(reader.readManifest, String(manifest.size), reader.readChunk, async (chunk) => {
      throwIfAborted(ctx);
      const source = chunk instanceof Uint8Array ? chunk : new Uint8Array(chunk);
      if (source.byteLength > ctx.limits.codecIoBytes) throw new TransferError('protocol_error', 'ZIP 出力が64 KiBを超えました。');
      const copy = source.slice(); // Never detach the WASM linear memory.
      const count = copy.byteLength;
      await requestUi(ctx, 'sink-write', { bytes: copy.buffer }, [copy.buffer]);
      written += BigInt(count);
      progress(ctx, { phase: 'download', written: written.toString(), confirmed: ctx.status.confirmed_bytes, state: ctx.status.state });
    });
    await reader.finish();
  } finally { await manifest.spool.close({ remove: true }); }
  await requestUi(ctx, 'sink-close', {});
  ctx.outputBytes = written.toString(); ctx.outputClosed = true;
  await saveSessionRecord(ctx);
  phase(ctx, 'complete', '保存先の close 成功をサーバーへ確定しています…');
  const completed = await sessionOperation(ctx, `${sessionPath(ctx)}/complete`, { archive_size: written.toString(), archive_sha256: null });
  if (completed.state !== 'released') throw new TransferError('protocol_error', '書き出し完了 receipt がありません。');
  await ctx.store.remove(ctx.requestId);
  return { state: completed.state, bytes: written.toString() };
}

async function createExportSession(ctx, ids) {
  const ticket = await freshCreationTicket(ctx);
  ctx.requestId = crypto.randomUUID();
  ctx.secretHeader = base64UrlEncode(randomSecretBytes());
  ctx.policy = { profile: 'runtime', extra_roles: [] };
  ctx.createRequest = { request_id: ctx.requestId, creation_ticket: ticket, ids, profile: 'runtime', extra_roles: [] };
  await saveSessionRecord(ctx);
  await recoverCreation(ctx);
}

function binaryByteCount(response) {
  const declared = response.headers.get('x-transfer-body-bytes');
  if (typeof declared !== 'string' || !/^[0-9]+$/.test(declared)
    || BigInt(declared) !== BigInt(response.bytes.byteLength)) {
    throw new TransferError('truncated_frame', 'バイナリ応答の実サイズが x-transfer-body-bytes と一致しません。');
  }
}

function binaryLast(response) {
  const last = response.headers.get('x-transfer-last');
  if (last !== 'true' && last !== 'false') throw new TransferError('protocol_error', 'バイナリ応答に終端の指定がありません。');
  return last === 'true';
}

async function verifyChunkDigest(response) {
  const etag = response.headers.get('etag') || '';
  const match = /^"([a-f0-9]{64})"$/.exec(etag);
  if (!match || await sha256Hex(response.bytes) !== match[1]) {
    throw new TransferError('hash_mismatch', '受信チャンクの実 SHA が ETag と一致しません。');
  }
}

async function fetchExportManifest(ctx) {
  const spool = await ByteSpool.open(`export-${crypto.randomUUID()}`);
  let total = null;
  try {
    for (let index = 0; ; index += 1) {
      throwIfAborted(ctx);
      const response = await withRetry(ctx, () => apiRequest(ctx, `${sessionPath(ctx)}/manifest/chunks/${index}`,
        { raw: true }), { operation: 'manifest 受信' });
      binaryByteCount(response);
      await verifyChunkDigest(response);
      const declared = parseDecimal(response.headers.get('x-transfer-manifest-bytes'), -1n);
      if (declared <= 0n || declared > BigInt(ctx.limits.manifestBytes) || (total !== null && total !== declared)) {
        throw new TransferError('manifest_too_large', 'manifest の全サイズが不正または途中で変化しました。');
      }
      total = declared;
      const remaining = total - BigInt(spool.size);
      const expected = remaining < BigInt(ctx.limits.chunkBytes) ? remaining : BigInt(ctx.limits.chunkBytes);
      if (expected <= 0n || BigInt(response.bytes.byteLength) !== expected) throw new TransferError('truncated_frame', 'manifest chunk が予定長に一致しません。');
      await spool.append(response.bytes);
      const last = binaryLast(response);
      if (last !== (BigInt(spool.size) === total)) throw new TransferError('truncated_frame', 'manifest の終端と全サイズが一致しません。');
      progress(ctx, { phase: 'manifest', received: String(spool.size), total: total.toString() });
      if (last) { await spool.finalize(); return { spool, size: spool.size }; }
    }
  } catch (error) { await spool.close({ remove: true }); throw error; }
}

/** One bounded batch input window. A record remains available for every 64 KiB codec read. */
export function createExportReader(ctx, manifestSpool) {
  const unit = ctx.limits.chunkBytes;
  let nextBatch = 0, lastBatch = false, queue = [], mode = 'batch', cached = null;
  let previousFile = -1, previousEnd = 0;
  let lastRecordFile = -1, lastRecordChunk = -1, lastRecordFinal = true;

  const fillQueue = async () => {
    if (queue.length || lastBatch) return;
    const response = await withRetry(ctx, () => apiRequest(ctx, `${sessionPath(ctx)}/batches/${nextBatch}`,
      { raw: true }), { operation: 'バッチ受信' });
    binaryByteCount(response);
    const countText = response.headers.get('x-transfer-record-count');
    if (!/^[0-9]+$/.test(countText || '')) throw new TransferError('invalid_frame', 'バッチのレコード数がありません。');
    const records = decodeFrames(response.bytes, ctx.limits);
    if (Number(countText) !== records.length) throw new TransferError('truncated_frame', 'バッチのレコード数が実際と一致しません。');
    lastBatch = binaryLast(response);
    if (!records.length && !lastBatch) throw new TransferError('truncated_frame', '終端でない空のバッチです。');
    for (const record of records) {
      const h = record.header;
      if ((h.file_id === lastRecordFile && (lastRecordFinal || h.chunk_index !== lastRecordChunk + 1))
        || (h.file_id !== lastRecordFile && (h.file_id !== lastRecordFile + 1 || !lastRecordFinal || h.chunk_index !== 0))
        || (!h.final && record.payload.byteLength !== unit)) {
        throw new TransferError('invalid_frame', 'snapshot の file/chunk 順またはレコード長が不正です。');
      }
      if (h.sha256 && await sha256Hex(record.payload) !== h.sha256) throw new TransferError('hash_mismatch', 'export record の SHA が一致しません。');
      lastRecordFile = h.file_id; lastRecordChunk = h.chunk_index; lastRecordFinal = h.final;
      record.used = 0;
    }
    queue = records; nextBatch += 1;
    ctx.receivedBytes = (ctx.receivedBytes || 0n) + BigInt(response.bytes.byteLength);
    progress(ctx, { phase: 'download', received: ctx.receivedBytes.toString() });
  };

  const individual = async (file, index) => {
    if (cached?.file !== file || cached?.index !== index) {
      cached = null; // release the old window before fetching the next one
      const response = await withRetry(ctx, () => apiRequest(ctx, `${sessionPath(ctx)}/files/${file}/chunks/${index}`,
        { raw: true }), { operation: 'チャンク再取得' });
      binaryByteCount(response);
      if (response.bytes.byteLength > unit) throw new TransferError('invalid_frame', '個別 chunk が上限を超えました。');
      await verifyChunkDigest(response);
      cached = { file, index, payload: response.bytes };
    }
    return cached.payload;
  };

  const readPart = async (file, start, want) => {
    const index = Math.floor(start / unit), within = start % unit;
    if (mode === 'individual') {
      const payload = await individual(file, index);
      if (within >= payload.byteLength) throw new TransferError('truncated_frame', '個別 chunk が短すぎます。');
      return payload.subarray(within, within + want);
    }
    for (;;) {
      await fillQueue();
      const head = queue[0];
      if (!head) throw new TransferError('truncated_frame', '必要なファイルより前に export batch が終了しました。');
      // Empty files have no read callback, but their final records still belong to the batch plan.
      if (head.header.file_id < file && head.header.final && !head.payload.byteLength && head.header.chunk_index === 0) { queue.shift(); continue; }
      if (head.header.file_id !== file || head.header.chunk_index !== index || head.used !== within) {
        throw new TransferError('invalid_frame', '要求した file/offset と snapshot batch が一致しません。');
      }
      const take = Math.min(want, head.payload.byteLength - within);
      if (take <= 0) throw new TransferError('truncated_frame', '空のレコードを本文として受け取りました。');
      const out = head.payload.subarray(within, within + take);
      head.used += take;
      if (head.used === head.payload.byteLength) queue.shift();
      return out;
    }
  };

  return {
    async readManifest(offset, length) {
      throwIfAborted(ctx);
      if (length > ctx.limits.codecIoBytes) throw new TransferError('protocol_error', 'manifest read が64 KiBを超えました。');
      return manifestSpool.read(offset, length);
    },
    async readChunk(_novel, _path, offset, length, fileId) {
      throwIfAborted(ctx);
      if (!Number.isInteger(fileId) || fileId < 0 || !Number.isSafeInteger(offset) || offset < 0
        || !Number.isSafeInteger(length) || length <= 0 || length > ctx.limits.codecIoBytes) {
        throw new TransferError('protocol_error', 'codec の file/offset/read size が不正です。');
      }
      if (fileId < previousFile || (fileId === previousFile && offset !== previousEnd)
        || (fileId > previousFile && offset !== 0)) {
        mode = 'individual'; queue = [];
      }
      let bytes = await readPart(fileId, offset, length);
      if (bytes.byteLength < length) {
        const out = new Uint8Array(length);
        out.set(bytes);
        let used = bytes.byteLength; bytes = null;
        while (used < length) {
          const part = await readPart(fileId, offset + used, length - used);
          out.set(part, used); used += part.byteLength;
        }
        bytes = out;
      }
      previousFile = fileId; previousEnd = offset + length;
      return bytes;
    },
    async finish() {
      if (mode === 'individual') { cached = null; return; }
      for (;;) {
        await fillQueue();
        if (queue.some((record) => record.payload.byteLength || !record.header.final)) {
          throw new TransferError('invalid_frame', 'manifest にない、または未読の export record が残っています。');
        }
        queue = [];
        if (lastBatch) {
          if (!lastRecordFinal) throw new TransferError('truncated_frame', '最後の file が final record なしで終了しました。');
          return;
        }
      }
    },
  };
}
