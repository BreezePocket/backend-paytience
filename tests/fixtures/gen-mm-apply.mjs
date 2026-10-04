// Generates tests/fixtures/mm-apply.json: a §5.2 application signed by Node with the
// desk's own helper (src/protocol.js signApplication, node:crypto ed25519).
// Run with cwd = MM-system-breezepocket so its node_modules resolve:
//   cd ../MM-system-breezepocket && node ../backend-paytience/tests/fixtures/gen-mm-apply.mjs ../backend-paytience/tests/fixtures
import fs from 'fs';
import path from 'path';
import { createRequire } from 'module';
import { pathToFileURL } from 'url';

const require = createRequire(path.join(process.cwd(), 'package.json'));
const { Keypair } = require('@solana/web3.js');
const desk = (file) => import(pathToFileURL(path.join(process.cwd(), 'src', file)).href);
const { signApplication, applyMessage } = await desk('protocol.js');
const { ed25519PrivateKey } = await desk('aggregatorClient.js');

const outDir = process.argv[2];
const keypair = Keypair.fromSeed(new Uint8Array(32).fill(1)); // the fixtures' MM A
const timestampMs = 1759000000000;
const body = signApplication({
  privateKey: ed25519PrivateKey(keypair),
  mmPubkey: keypair.publicKey,
  name: 'Acme MM',
  contact: 'ops@acme.example',
  capTotalUsd: 20000,
  assetCapsUsd: { SOL: 10000, WBTC: 5000 },
  timestampMs,
});
const fixture = {
  generated_by: 'tests/fixtures/gen-mm-apply.mjs using MM-system-breezepocket/src/protocol.js signApplication (node:crypto ed25519, seed 0x01 x 32)',
  seed_byte: 1,
  pubkey: keypair.publicKey.toBase58(),
  timestamp_ms: timestampMs,
  message: applyMessage(body.application),
  body,
};
fs.writeFileSync(path.join(outDir, 'mm-apply.json'), JSON.stringify(fixture, null, 2) + '\n');
console.log('wrote mm-apply.json for', fixture.pubkey);
