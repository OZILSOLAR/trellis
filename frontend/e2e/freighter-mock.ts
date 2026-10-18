import type { Page } from '@playwright/test';
import { Keypair, TransactionBuilder } from '@stellar/stellar-sdk';

export interface FreighterMockOptions {
  address: string;
  secret: string;
  networkPassphrase: string;
  rpcUrl: string;
}

/**
 * Installs a fake Freighter extension in the page under test.
 *
 * `@stellar/freighter-api` talks to the real extension over
 * `window.postMessage` with the wire shape reverse-engineered from
 * `node_modules/@stellar/freighter-api/build/index.min.js`: a request has
 * `source: "FREIGHTER_EXTERNAL_MSG_REQUEST"` and a `type`; the response must
 * carry `source: "FREIGHTER_EXTERNAL_MSG_RESPONSE"` and `messagedId` (that
 * exact spelling, not `messageId`) matching the request's `messageId`.
 *
 * Every request type maps to a fixed mock response, except signing: signing
 * needs a real private key, which never leaves the Node test process — the
 * page hands the transaction XDR to `window.__mockSignTransaction` (wired to
 * this function's `signXdr` via `page.exposeFunction`), gets back a signed
 * XDR, and relays it as the response.
 */
export async function installFreighterMock(
  page: Page,
  opts: FreighterMockOptions,
): Promise<{ switchAccount: (address: string, secret: string) => Promise<void> }> {
  let keypair = Keypair.fromSecret(opts.secret);

  await page.exposeFunction('__mockSignTransaction', (xdr: string) => {
    const tx = TransactionBuilder.fromXDR(xdr, opts.networkPassphrase);
    tx.sign(keypair);
    return tx.toXDR();
  });

  await page.addInitScript((mock: { address: string; networkPassphrase: string; rpcUrl: string }) => {
    (window as unknown as { freighter: boolean }).freighter = true;
    (window as unknown as { __mockAddress: string }).__mockAddress = mock.address;

    window.addEventListener('message', async (event: MessageEvent) => {
      const data = event.data as { source?: string; type?: string; messageId?: number; transactionXdr?: string };
      if (data?.source !== 'FREIGHTER_EXTERNAL_MSG_REQUEST') return;

      const base = { source: 'FREIGHTER_EXTERNAL_MSG_RESPONSE', messagedId: data.messageId };

      if (data.type === 'SUBMIT_TRANSACTION') {
        const signedTransaction = await (
          window as unknown as { __mockSignTransaction: (xdr: string) => Promise<string> }
        ).__mockSignTransaction(data.transactionXdr ?? '');
        window.postMessage({ ...base, signedTransaction, signerAddress: (window as unknown as { __mockAddress: string }).__mockAddress }, window.location.origin);
        return;
      }

      window.postMessage(
        {
          ...base,
          isConnected: true,
          isAllowed: true,
          publicKey: (window as unknown as { __mockAddress: string }).__mockAddress,
          address: (window as unknown as { __mockAddress: string }).__mockAddress,
          networkDetails: {
            network: 'STANDALONE',
            networkName: 'Standalone',
            networkUrl: mock.rpcUrl,
            networkPassphrase: mock.networkPassphrase,
            sorobanRpcUrl: mock.rpcUrl,
          },
        },
        window.location.origin,
      );
    });
  }, opts);

  // Lets one test act as several roles (payer, then payee, ...) by swapping
  // the identity the mocked wallet reports and signs with.
  return {
    switchAccount: async (address: string, secret: string) => {
      keypair = Keypair.fromSecret(secret);
      await page.evaluate((a) => {
        (window as unknown as { __mockAddress: string }).__mockAddress = a;
      }, address);
    },
  };
}
