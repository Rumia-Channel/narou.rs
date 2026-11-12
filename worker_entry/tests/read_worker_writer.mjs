// Test-only D1 control endpoint inside the same workerd that owns the database.
import worker from '../build/worker/shim.mjs';
export * from '../build/worker/shim.mjs';

export default class WriterFixture extends worker {
  async fetch(request) {
    if (new URL(request.url).pathname === '/__fixture__/sql') {
      if (request.headers.get('Authorization') !== `Bearer ${this.env.NAROU_ADMIN_TOKEN}`) {
        return new Response('Unauthorized', { status: 401 });
      }
      const { queries } = await request.json();
      const result = await this.env.DB.batch(queries.map(query => this.env.DB.prepare(query)));
      return Response.json(result);
    }
    return new worker(this.ctx, this.env).fetch(request);
  }
}
