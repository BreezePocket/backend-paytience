// Generates the backend's test fixtures with the desk's own transaction builder.
// Run with cwd = MM-system-breezepocket so its node_modules and idl resolve:
//   node <this file> <out dir>
import crypto from 'crypto';
import fs from 'fs';
import path from 'path';
import { createRequire } from 'module';
// Resolve @solana/web3.js from the desk repo (this script lives outside it).
const require = createRequire(path.join(process.cwd(), 'package.json'));
const { ComputeBudgetProgram, Keypair, PublicKey, SystemProgram, Transaction, TransactionMessage, VersionedTransaction } = require('@solana/web3.js');
import { BreezepocketClient } from '/Users/skyyap/Documents/GitHub/MM-system-breezepocket/src/program.js';

const outDir = process.argv[2];
const PROGRAM_ID = 'BeytdpJYSGP1oFRxEiBLkSRGuW6HW3Pk9dqSZCuSteQ9';
const seed = (b) => new Uint8Array(32).fill(b);
const mmA = Keypair.fromSeed(seed(1));
const mmB = Keypair.fromSeed(seed(2));
const user = Keypair.fromSeed(seed(3));
const usdcMint = Keypair.fromSeed(seed(4)).publicKey;
const assetMint = Keypair.fromSeed(seed(5)).publicKey;
const blockhash = new PublicKey(Buffer.alloc(32, 11)).toBase58();

const client = new BreezepocketClient({ connection: {}, programId: PROGRAM_ID, usdcMint });
const b64 = (tx) => Buffer.from(tx.serialize({ requireAllSignatures: false, verifySignatures: false })).toString('base64');

const base = { user: user.publicKey, mm: mmA.publicKey, product: 'sell_sol', fixedPrice: 250000000n, expiryTs: 1759824000, amount: 500000000n, yieldAmount: 1234567n };

const solTx = await client.buildOpenPositionTx({ ...base, nonce: 1234567890123n }, blockhash);
solTx.partialSign(user);

const assetTx = await client.buildOpenPositionTx(
  { ...base, mint: assetMint, product: 'buy_sol', amount: 100000000n, nonce: 0xfedcba9876543210n },
  blockhash
);
assetTx.partialSign(user);

const cbIx = await client.openPositionInstruction({ ...base, nonce: 42n });
const cbTx = new Transaction();
cbTx.feePayer = mmA.publicKey;
cbTx.recentBlockhash = blockhash;
cbTx.add(ComputeBudgetProgram.setComputeUnitLimit({ units: 300000 }));
cbTx.add(ComputeBudgetProgram.setComputeUnitPrice({ microLamports: 1000 }));
cbTx.add(cbIx);
cbTx.partialSign(user);

const mmBTx = await client.buildOpenPositionTx({ ...base, mm: mmB.publicKey, nonce: 777n }, blockhash);
mmBTx.partialSign(user);

const v0Ix = await client.openPositionInstruction({ ...base, nonce: 99n });
const v0 = new VersionedTransaction(
  new TransactionMessage({ payerKey: mmA.publicKey, recentBlockhash: blockhash, instructions: [v0Ix] }).compileToV0Message()
);
v0.sign([user]);

const transferTx = new Transaction();
transferTx.feePayer = mmA.publicKey;
transferTx.recentBlockhash = blockhash;
transferTx.add(SystemProgram.transfer({ fromPubkey: user.publicKey, toPubkey: mmA.publicKey, lamports: 1 }));
transferTx.partialSign(user);

const transactions = {
  generated_by: 'scratchpad gen-fixtures.mjs using MM-system-breezepocket/src/program.js BreezepocketClient',
  program_id: PROGRAM_ID,
  mm_a_seed_byte: 1,
  mm_b_seed_byte: 2,
  mm_a_pubkey: mmA.publicKey.toBase58(),
  mm_b_pubkey: mmB.publicKey.toBase58(),
  user_pubkey: user.publicKey.toBase58(),
  usdc_mint: usdcMint.toBase58(),
  cases: {
    open_position_sol: { tx_base64: b64(solTx), fee_payer: mmA.publicKey.toBase58(), nonce: '1234567890123' },
    open_asset_position: { tx_base64: b64(assetTx), fee_payer: mmA.publicKey.toBase58(), nonce: (0xfedcba9876543210n).toString() },
    compute_budget_prefixed: { tx_base64: b64(cbTx), fee_payer: mmA.publicKey.toBase58(), nonce: '42' },
    open_position_mm_b: { tx_base64: b64(mmBTx), fee_payer: mmB.publicKey.toBase58(), nonce: '777' },
    versioned_v0: { tx_base64: Buffer.from(v0.serialize()).toString('base64'), fee_payer: mmA.publicKey.toBase58(), nonce: '99' },
    transfer_only: { tx_base64: b64(transferTx), fee_payer: mmA.publicKey.toBase58(), nonce: null },
  },
};

// Node crypto ed25519 over the hello auth message, with a PKCS8 DER-wrapped seed.
const PKCS8_PREFIX = Buffer.from('302e020100300506032b657004220420', 'hex');
const authSeed = Buffer.alloc(32, 1);
const priv = crypto.createPrivateKey({ key: Buffer.concat([PKCS8_PREFIX, authSeed]), format: 'der', type: 'pkcs8' });
const spki = crypto.createPublicKey(priv).export({ format: 'der', type: 'spki' });
const pubkey = new PublicKey(spki.subarray(spki.length - 32)).toBase58();
const challenge = crypto.createHash('sha256').update('paytience fixture challenge').digest('hex');
const auth = {};
for (const channel of ['ws/rfq', 'ws/sign']) {
  const message = `paytience-mm-auth:v1:${channel}:${challenge}`;
  auth[channel] = { message, signature: crypto.sign(null, Buffer.from(message, 'utf8'), priv).toString('base64') };
}
const authFixture = {
  generated_by: 'scratchpad gen-fixtures.mjs using node:crypto ed25519 (PKCS8 DER-wrapped 32-byte seed of 0x01)',
  seed_byte: 1,
  pubkey,
  challenge,
  channels: auth,
};

fs.writeFileSync(path.join(outDir, 'transactions.json'), JSON.stringify(transactions, null, 2) + '\n');
fs.writeFileSync(path.join(outDir, 'auth.json'), JSON.stringify(authFixture, null, 2) + '\n');
console.log('wrote fixtures', transactions.mm_a_pubkey, pubkey);
