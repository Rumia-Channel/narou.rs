// Test-only observation/barrier wrapper. Production owns every queue, ledger,
// admission and scheduled transition; this helper never changes D1 or envelopes.
import Worker from '../build/worker/shim.mjs';
export * from '../build/worker/shim.mjs';

export default class RuntimeRecoveryWorker extends Worker {
  async fetch(request) {
    if (new URL(request.url).pathname !== '/__fixture/runtime-recovery') {
      return super.fetch(request);
    }
    if (request.method !== 'GET') return new Response('Method Not Allowed', { status: 405 });
    if (request.headers.get('Authorization') !== `Bearer ${this.env.NAROU_ADMIN_TOKEN}`) {
      return new Response('Unauthorized', { status: 401 });
    }
    // Read through the live workerd, not a competing Wrangler/SQLite process.
    const [jobs, novels, lanes, reservations, claims, gates] = await Promise.all([
      this.env.DB.prepare(`SELECT job_id,kind,target,status,attempts,recovery_attempts,
        site_key,normalized_target,admission_waiting,lease_until,execution_token,
        checkpoint_json,last_error FROM worker_jobs ORDER BY created_at,job_id`).all(),
      this.env.DB.prepare('SELECT id,ncode,general_all_no FROM novels ORDER BY id').all(),
      this.env.DB.prepare('SELECT lane,job_id,site_key,dispatch_token,lease_until FROM worker_acquisition_lanes ORDER BY lane').all(),
      this.env.DB.prepare('SELECT id,generation,available,job_id FROM novel_id_reservations ORDER BY id').all(),
      this.env.DB.prepare('SELECT prefix,novel_id,reservation_generation,toc_url_fold,owner,epoch,job_id,empty_owned FROM novel_namespace_claims ORDER BY prefix').all(),
      this.env.DB.prepare('SELECT prefix,epoch,closed,owner,origin_url_fold,(SELECT COUNT(*) FROM novel_reader_leases r WHERE r.prefix=g.prefix AND r.expires_at>unixepoch()) AS pending_readers FROM novel_access_gates g ORDER BY prefix').all(),
    ]);
    return Response.json({
      jobs: jobs.results, novels: novels.results, lanes: lanes.results,
      reservations: reservations.results, claims: claims.results, gates: gates.results,
    });
  }

  async queue(batch) {
    const ids = batch.messages.map(message => message.body.job_id);
    const observe = async phase => {
      const response = await fetch(`${this.env.RECOVERY_FIXTURE_ORIGIN}/__fixture/queue/${phase}`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ ids }),
      });
      if (!response.ok) throw new Error(`recovery fixture ${phase}: ${response.status}`);
      await response.text();
    };
    // Keep restarted deliveries outside the production handler until the real
    // cron has run. This removes races with local Queue redelivery, without
    // acknowledging, retrying, fabricating or replacing any envelope.
    await observe('before');
    await super.queue(batch);
    await observe('after');
  }
}
