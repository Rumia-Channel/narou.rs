import { readFile, writeFile, access } from 'node:fs/promises';
import { setTimeout as sleep } from 'node:timers/promises';
import { startTransferLimitProxy } from '../tests/transfer_limit_proxy.mjs';

const [descriptorPath, readyPath, stopPath, statsPath] = process.argv.slice(2);
const descriptor = JSON.parse(await readFile(descriptorPath, 'utf8'));
const gateways = [];
try {
  for (const origin of descriptor.origins) gateways.push(await startTransferLimitProxy({ target: origin.base }));
  await writeFile(readyPath, JSON.stringify(gateways.map((gateway) => gateway.base)), { flag: 'wx', mode: 0o600 });
  const deadline = Date.now() + 20 * 60 * 1000;
  for (;;) {
    if (await access(stopPath).then(() => true, () => false)) break;
    if (Date.now() > deadline) throw new Error('Browser gateway controller exceeded fixture lifetime');
    await sleep(100);
  }
} finally {
  await Promise.all(gateways.map((gateway) => gateway.close()));
  await writeFile(statsPath, JSON.stringify(gateways.map((gateway) => gateway.snapshot())), { flag: 'wx', mode: 0o600 });
}
