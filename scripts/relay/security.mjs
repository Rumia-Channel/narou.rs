import { lookup as dnsLookup } from "node:dns/promises";
import { isIP } from "node:net";
import { domainToASCII } from "node:url";

export class RelayTargetError extends Error {
  constructor(status, message) {
    super(message);
    this.status = status;
  }
}

// URL と許可リストに同じ正規化を使う。ポート・ワイルドカードはホスト名ではない。
export const normalizeHost = (value) => {
  if (typeof value !== "string" || !value || /[\s/@?#\\*]/u.test(value)) {
    throw new RelayTargetError(400, "invalid host");
  }
  let host = value.replace(/\.$/, "");
  if (host.startsWith("[") && host.endsWith("]")) host = host.slice(1, -1);
  if (isIP(host) === 6) return new URL(`http://[${host}]/`).hostname.slice(1, -1);
  host = domainToASCII(host).toLowerCase().replace(/\.$/, "");
  if (!host || host.length > 253 || !host.split(".").every((label) =>
    label.length <= 63 && /^[a-z0-9](?:[a-z0-9-]*[a-z0-9])?$/.test(label))) {
    throw new RelayTargetError(400, "invalid host");
  }
  // WHATWG URL は省略形・整数・16進表記の IPv4 も正規化する。
  try { return new URL(`http://${host}/`).hostname; } catch { throw new RelayTargetError(400, "invalid host"); }
};

const publicV4 = (address) => {
  const [a, b, c] = address.split(".").map(Number);
  return !(a === 0 || a === 10 || a === 127 || a >= 224
    || (a === 100 && b >= 64 && b <= 127)
    || (a === 169 && b === 254)
    || (a === 172 && b >= 16 && b <= 31)
    || (a === 192 && b === 168)
    || (a === 192 && b === 0 && (c === 0 || c === 2))
    || (a === 192 && b === 88 && c === 99)
    || (a === 198 && (b === 18 || b === 19))
    || (a === 198 && b === 51 && c === 100)
    || (a === 203 && b === 0 && c === 113));
};

export const isPublicAddress = (address) => {
  const family = isIP(address);
  if (family === 4) return publicV4(address);
  if (family !== 6) return false;
  const canonical = new URL(`http://[${address}]/`).hostname.slice(1, -1);
  const [left, right] = canonical.split("::");
  const first = left ? left.split(":") : [];
  const last = right ? right.split(":") : [];
  const groups = right === undefined ? first : [...first, ...Array(8 - first.length - last.length).fill("0"), ...last];
  const parts = groups.map((part) => Number.parseInt(part, 16));
  // 公開 unicast 以外と、文書用・廃止済み・IPv4 埋込みの移行用アドレスは拒否する。
  // mapped/NAT64/compatible は 2000::/3 外。Teredo/ORCHID 等は 2001::/23 内。
  return parts[0] >= 0x2000 && parts[0] <= 0x3fff
    && !(parts[0] === 0x2001 && (parts[1] < 0x0200 || parts[1] === 0x0db8))
    && parts[0] !== 0x2002 && parts[0] !== 0x3ffe
    && !(parts[0] === 0x3fff && parts[1] < 0x1000)
    && !(parts[5] === 0x5efe && (parts[4] === 0 || parts[4] === 0x0200));
};

export const createTargetResolver = (allowedHosts, { lookup = dnsLookup } = {}) => {
  const allowed = new Set(String(allowedHosts || "").split(",").map((host) => host.trim())
    .filter(Boolean).map(normalizeHost));
  return async (rawUrl, { signal } = {}) => {
    const cancelled = () => {
      if (signal?.aborted) throw new RelayTargetError(502, "request cancelled");
    };
    cancelled();
    let url;
    try {
      if (typeof rawUrl !== "string" || /[\u0000-\u001f\u007f]/u.test(rawUrl)) throw new Error();
      url = new URL(rawUrl);
    } catch {
      throw new RelayTargetError(400, "invalid target URL");
    }
    if (!['http:', 'https:'].includes(url.protocol) || url.username || url.password || !url.hostname) {
      throw new RelayTargetError(400, "target must be HTTP(S) without user information");
    }
    const host = normalizeHost(url.hostname);
    if (!allowed.has(host)) throw new RelayTargetError(400, "target host is not allowed");
    const literal = isIP(host);
    url.hostname = literal === 6 ? `[${host}]` : host;
    let records;
    if (literal) {
      records = [{ address: host, family: literal }];
    } else {
      try {
        records = await lookup(host, { all: true, verbatim: true });
      } catch {
        throw new RelayTargetError(502, "target DNS lookup failed");
      }
    }
    cancelled();
    if (!Array.isArray(records) || records.length === 0) {
      throw new RelayTargetError(502, "target DNS returned no addresses");
    }
    if (records.some((record) => !record || isIP(record.address) !== record.family || !isPublicAddress(record.address))) {
      throw new RelayTargetError(400, "target address is not public");
    }
    // すべてを検査してから 1 個だけ選び、呼び出し側が接続先を固定する。
    const selected = records.find((record) => record.family === 4) || records[0];
    return { url, address: selected.address, family: selected.family };
  };
};
