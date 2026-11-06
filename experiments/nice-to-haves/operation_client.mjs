// Loopback-only teaching client: exact file bytes, one attempt, no redirect/retry.
import { readFile } from 'node:fs/promises';
import http from 'node:http';

try {
  const [target, operation, filename] = process.argv.slice(2);
  const url = new URL(target);
  if (url.protocol !== 'http:' || url.hostname !== '127.0.0.1' || url.username ||
      url.password || url.hash || !/^[A-Za-z0-9_-]{1,128}$/.test(operation ?? '')) {
    throw new Error('loopback URL and bounded ASCII operation ID required');
  }
  const body = await readFile(filename);
  if (body.length > 4096) throw new Error('fixture body limit');
  const result = await new Promise(resolve => {
    const req = http.request(url, {method: 'POST', headers: {
      'Content-Type': 'application/json', 'Idempotency-Key': operation,
      'Content-Length': body.length,
    }}, res => {
      let bytes = 0;
      res.on('data', chunk => {
        bytes += chunk.length;
        if (bytes > 65536) res.destroy();
      });
      res.on('error', () => resolve({outcome: 'unknown'}));
      res.on('end', () => resolve({outcome: 'http-response', status: res.statusCode}));
    });
    req.setTimeout(2000, () => req.destroy());
    req.on('error', () => resolve({outcome: 'unknown'}));
    req.end(body);
  });
  console.log(JSON.stringify(result));
} catch {
  console.error('invalid local fixture input');
  process.exitCode = 2;
}
