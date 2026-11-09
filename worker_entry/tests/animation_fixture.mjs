// Deterministic indexed PNG frames expand to a large RGBA APNG in the Worker.
// Test data is generated locally: no live site, cookies or copyrighted fixture.
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { deflateSync, inflateSync } from 'node:zlib';

const signature = Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]);
const crcTable = Uint32Array.from({ length: 256 }, (_, value) => {
  for (let bit = 0; bit < 8; bit++) value = (value >>> 1) ^ ((value & 1) ? 0xedb88320 : 0);
  return value >>> 0;
});
function crc32(bytes) {
  let value = 0xffffffff;
  for (const byte of bytes) value = (value >>> 8) ^ crcTable[(value ^ byte) & 255];
  return (value ^ 0xffffffff) >>> 0;
}
function chunk(kind, payload) {
  const result = Buffer.alloc(payload.length + 12);
  result.writeUInt32BE(payload.length, 0);
  result.write(kind, 4, 4, 'ascii');
  payload.copy(result, 8);
  result.writeUInt32BE(crc32(result.subarray(4, -4)), result.length - 4);
  return result;
}
export function storedZip(entries) {
  const body = [];
  const directory = [];
  let offset = 0;
  for (const [name, bytes] of entries) {
    const filename = Buffer.from(name);
    const checksum = crc32(bytes);
    const local = Buffer.alloc(30);
    local.writeUInt32LE(0x04034b50, 0);
    local.writeUInt16LE(20, 4);
    local.writeUInt16LE(0x0800, 6);
    local.writeUInt16LE(0x0021, 12);
    local.writeUInt32LE(checksum, 14);
    local.writeUInt32LE(bytes.length, 18);
    local.writeUInt32LE(bytes.length, 22);
    local.writeUInt16LE(filename.length, 26);
    const central = Buffer.alloc(46);
    central.writeUInt32LE(0x02014b50, 0);
    central.writeUInt16LE(20, 4);
    central.writeUInt16LE(20, 6);
    central.writeUInt16LE(0x0800, 8);
    central.writeUInt16LE(0x0021, 14);
    central.writeUInt32LE(checksum, 16);
    central.writeUInt32LE(bytes.length, 20);
    central.writeUInt32LE(bytes.length, 24);
    central.writeUInt16LE(filename.length, 28);
    central.writeUInt32LE(offset, 42);
    body.push(local, filename, bytes);
    directory.push(central, filename);
    offset += local.length + filename.length + bytes.length;
  }
  const end = Buffer.alloc(22);
  end.writeUInt32LE(0x06054b50, 0);
  end.writeUInt16LE(entries.length, 8);
  end.writeUInt16LE(entries.length, 10);
  end.writeUInt32LE(directory.reduce((size, bytes) => size + bytes.length, 0), 12);
  end.writeUInt32LE(offset, 16);
  return Buffer.concat([...body, ...directory, end]);
}

export function animationFixture({ frameCount = 64, width = 512, height = 512, solidColor, archive = true, indexedNoise = true } = {}) {
  if (!archive) assert.equal(frameCount, 1);
  const palette = Buffer.alloc(64 * 3);
  const alpha = Buffer.alloc(64);
  for (let i = 0; i < 64; i++) {
    palette[i * 3] = solidColor?.[0] ?? i * 47;
    palette[i * 3 + 1] = solidColor?.[1] ?? i * 89;
    palette[i * 3 + 2] = solidColor?.[2] ?? i * 131;
    alpha[i] = solidColor?.[3] ?? (i % 11 === 0 ? 0 : i % 7 === 0 ? 120 : 255);
  }
  const header = Buffer.alloc(13);
  header.writeUInt32BE(width, 0); header.writeUInt32BE(height, 4);
  header[8] = 8; header[9] = 3;
  const entries = [];
  const digests = [];
  const delays = [];
  for (let frame = 0; frame < frameCount; frame++) {
    const raw = Buffer.alloc((width + 1) * height);
    const rgba = Buffer.alloc(width * 4);
    const digest = createHash('sha256');
    let random = (0x6d2b79f5 ^ (frame + 1) * 0x9e3779b9) >>> 0;
    for (let y = 0; y < height; y++) {
      for (let x = 0; x < width; x++) {
        if (indexedNoise) { random ^= random << 13; random ^= random >>> 17; random ^= random << 5; }
        const color = indexedNoise ? (random >>> 0) & 63 : 0;
        raw[y * (width + 1) + 1 + x] = color;
        rgba[x * 4] = palette[color * 3];
        rgba[x * 4 + 1] = palette[color * 3 + 1];
        rgba[x * 4 + 2] = palette[color * 3 + 2];
        rgba[x * 4 + 3] = alpha[color];
      }
      digest.update(rgba);
    }
    const png = Buffer.concat([signature, chunk('IHDR', header), chunk('PLTE', palette), chunk('tRNS', alpha), chunk('IDAT', deflateSync(raw, { level: 9 })), chunk('IEND', Buffer.alloc(0))]);
    entries.push([`${String(frame).padStart(6, '0')}.png`, png]);
    digests.push(digest.digest('hex'));
    delays.push([80, 125, 160][frame % 3]);
  }
  return { bytes: archive ? storedZip(entries) : entries[0][1], digests, delays, width, height, frameCount };
}

function paeth(left, above, corner) {
  const predicted = left + above - corner;
  const a = Math.abs(predicted - left), b = Math.abs(predicted - above), c = Math.abs(predicted - corner);
  return a <= b && a <= c ? left : b <= c ? above : corner;
}
function frameDigest(compressed, width, height) {
  const raw = inflateSync(Buffer.concat(compressed));
  const stride = width * 4;
  assert.equal(raw.length, (stride + 1) * height);
  let previous = Buffer.alloc(stride);
  let current = Buffer.alloc(stride);
  const digest = createHash('sha256');
  for (let y = 0; y < height; y++) {
    const filter = raw[y * (stride + 1)];
    assert(filter <= 4, `unknown PNG filter ${filter}`);
    for (let x = 0; x < stride; x++) {
      const left = x >= 4 ? current[x - 4] : 0;
      const above = previous[x];
      const corner = x >= 4 ? previous[x - 4] : 0;
      const predictor = filter === 0 ? 0 : filter === 1 ? left : filter === 2 ? above : filter === 3 ? Math.floor((left + above) / 2) : paeth(left, above, corner);
      current[x] = (raw[y * (stride + 1) + 1 + x] + predictor) & 255;
    }
    digest.update(current);
    [previous, current] = [current, previous];
  }
  return digest.digest('hex');
}

export function isAnimation(bytes) {
  if (!bytes.subarray(0, 8).equals(signature)) return false;
  for (let cursor = 8; cursor + 12 <= bytes.length;) {
    const length = bytes.readUInt32BE(cursor);
    const end = cursor + length + 12;
    if (end > bytes.length) return false;
    const kind = bytes.toString('ascii', cursor + 4, cursor + 8);
    if (kind === 'acTL' && length === 8) return true;
    if (kind === 'IEND') return false;
    cursor = end;
  }
  return false;
}

export function verifyPng(bytes, expected) {
  const animated = expected.animated !== false;
  assert.deepEqual(bytes.subarray(0, 8), signature);
  let cursor = 8;
  let sequence = 0;
  let frame = animated ? null : { data: [] };
  let frames = 0;
  let declared = null;
  let ended = false;
  const finishFrame = () => {
    if (!frame) return;
    assert.equal(frameDigest(frame.data, expected.width, expected.height), expected.digests[frames], `RGBA pixels/alpha of frame ${frames}`);
    if (animated) assert.equal(frame.delayNumerator * 1000, expected.delays[frames] * (frame.delayDenominator || 100), `delay of frame ${frames}`);
    frames++;
    frame = null;
  };
  while (cursor + 12 <= bytes.length) {
    const length = bytes.readUInt32BE(cursor);
    const end = cursor + length + 12;
    assert(end <= bytes.length, 'truncated PNG chunk');
    const kind = bytes.toString('ascii', cursor + 4, cursor + 8);
    const data = bytes.subarray(cursor + 8, end - 4);
    assert.equal(crc32(bytes.subarray(cursor + 4, end - 4)), bytes.readUInt32BE(end - 4), `CRC of ${kind}`);
    if (kind === 'IHDR') {
      assert.equal(data.readUInt32BE(0), expected.width);
      assert.equal(data.readUInt32BE(4), expected.height);
      assert.equal(data[8], 8); assert.equal(data[9], 6);
    } else if (kind === 'acTL') {
      assert(animated, 'static PNG must not declare animation frames');
      declared = data.readUInt32BE(0);
      assert.equal(data.readUInt32BE(4), 0, 'animation loops must be preserved');
    } else if (kind === 'fcTL') {
      assert(animated, 'static PNG must not contain frame control');
      finishFrame();
      assert.equal(data.readUInt32BE(0), sequence++);
      assert.equal(data.readUInt32BE(4), expected.width);
      assert.equal(data.readUInt32BE(8), expected.height);
      assert.equal(data.readUInt32BE(12), 0); assert.equal(data.readUInt32BE(16), 0);
      frame = { data: [], delayNumerator: data.readUInt16BE(20), delayDenominator: data.readUInt16BE(22) };
    } else if (kind === 'IDAT') {
      assert(frame, 'image data needs frame control');
      frame.data.push(data);
    } else if (kind === 'fdAT') {
      assert(frame, 'image data needs frame control');
      assert.equal(data.readUInt32BE(0), sequence++);
      frame.data.push(data.subarray(4));
    } else if (kind === 'IEND') {
      finishFrame(); ended = true;
    }
    cursor = end;
  }
  assert(ended, 'complete PNG terminator required');
  assert.equal(cursor, bytes.length);
  assert.equal(declared, animated ? expected.frameCount : null);
  assert.equal(frames, expected.frameCount);
}
