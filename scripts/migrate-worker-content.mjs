// Read-only by default. --apply explicitly includes deletion of verified legacy
// D1 episode payloads. Credentials are read from the environment, never argv.
import process from 'node:process';

const args = process.argv.slice(2);
let apply = false;
let base = process.env.BASE_URL;
let invalid = false;
for (let index = 0; index < args.length; index++) {
  if (args[index] === '--apply') apply = true;
  else if (args[index] === '--base-url' && args[index + 1] && !args[index + 1].startsWith('--')) base = args[++index];
  else invalid = true;
}
base = base?.replace(/\/$/, '');
const token = process.env.NAROU_ADMIN_TOKEN;
const accessId = process.env.CF_ACCESS_CLIENT_ID;
const accessSecret = process.env.CF_ACCESS_CLIENT_SECRET;
const hasAccessToken = Boolean(accessId && accessSecret);
const limit = Number(process.env.NAROU_CONTENT_MIGRATION_LIMIT ?? '20');
if (invalid || !base || (!token && !hasAccessToken) || !Number.isInteger(limit) || limit < 1 || limit > 100) {
  console.error('Usage: BASE_URL=https://your-worker node scripts/migrate-worker-content.mjs [--apply] [--base-url URL]; credentials: NAROU_ADMIN_TOKEN and/or CF_ACCESS_CLIENT_ID + CF_ACCESS_CLIENT_SECRET');
  process.exit(2);
}

async function request(action) {
  const response = await fetch(`${base}/api/admin/content-migration`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json',
      ...(token ? { Authorization: `Bearer ${token}` } : {}),
      ...(hasAccessToken ? { 'cf-access-client-id': accessId, 'cf-access-client-secret': accessSecret } : {}) },
    body: JSON.stringify({ action, limit }),
  });
  const report = await response.json();
  if (!response.ok || report.success !== true) {
    throw new Error(`${action}: HTTP ${response.status}: ${JSON.stringify(report)}`);
  }
  console.log(JSON.stringify({
    action, backend: report.backend, done: report.done,
    planned: report.planned, copied: report.copied, verified: report.verified,
    purged: report.purged, remaining_legacy: report.remaining_legacy,
    planned_bytes: report.planned_bytes, legacy_bytes: report.legacy_bytes, references: report.references,
    garbage_collected: report.garbage_collected, pending_garbage: report.pending_garbage,
  }));
  return report;
}

try {
  let report = await request('plan');
  if (apply) {
    for (const phase of ['copy', 'verify', 'cutover', 'purge', 'gc']) {
      // Re-entering a completed phase would reset the next phase's saved cursor.
      if ((phase === 'copy' && report.copy_done) ||
          (phase === 'verify' && report.verify_done) ||
          (phase === 'cutover' && report.backend === 's3') ||
          (phase === 'purge' && report.purge_done)) continue;
      do { report = await request(phase); } while (!report.done);
    }
  }
} catch (error) {
  console.error(error.message);
  process.exitCode = 1;
}
